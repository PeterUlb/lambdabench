import * as cdk from "aws-cdk-lib";
import { Construct } from "constructs";
import * as ec2 from "aws-cdk-lib/aws-ec2";
import * as ecs from "aws-cdk-lib/aws-ecs";
import * as ecr from "aws-cdk-lib/aws-ecr";
import * as s3 from "aws-cdk-lib/aws-s3";
import * as iam from "aws-cdk-lib/aws-iam";
import * as logs from "aws-cdk-lib/aws-logs";
import * as cloudfront from "aws-cdk-lib/aws-cloudfront";
import { KMS_TAG_KEY, KMS_TAG_VALUE, RESOURCE_WILDCARD } from "./constants";

export interface BenchRunnerStackProps extends cdk.StackProps {
  /** ECR repo holding the runner image (from EcrStack). */
  readonly repository: ecr.IRepository;
  /** Immutable ECR tag of the runner image to pull (e.g. a release date or git
   * SHA). Pinned at deploy time via the `runnerImageTag` CDK context so a new
   * runner image is an explicit infra deploy, not a silent `:latest` push. */
  readonly imageTag: string;
  /** Site origin bucket the publish step syncs into (from SiteStack). */
  readonly siteBucket: s3.IBucket;
  /** Private archive bucket for raw run-* output and probe lifecycle-*.json
   * (from SiteStack). The matrix run is written right after it completes so an
   * aborted probe or publish cannot lose the hours-long run; the probe outputs
   * are written after they are produced. Not publicly reachable. */
  readonly archiveBucket: s3.IBucket;
  /** Distribution the publish step invalidates (from SiteStack). */
  readonly distribution: cloudfront.IDistribution;
  /** Apex domain, injected as LAMBDABENCH_SITE_DOMAIN so the site build bakes the
   * real canonical URLs. */
  readonly siteDomain: string;
  /** Public source-repo URL, injected as LAMBDABENCH_REPO_URL so the site build links
   * the footer's "Source on GitHub". */
  readonly repoUrl: string;
  /** Contact email, injected as LAMBDABENCH_CONTACT_EMAIL so the site build adds the
   * "Contact" mailto footer link. Optional: omit and the link is absent. */
  readonly contactEmail?: string;
}

/**
 * On-demand ECS Fargate task that runs the full benchmark pipeline
 * (doctor -> build -> deploy -> run (hours-long) -> teardown) and then rebuilds
 * and publishes the static site. Nothing runs between invocations; a run is
 * kicked off explicitly with `aws ecs run-task` (see deploy/run.sh).
 *
 * The benchmark and all its AWS resources are pinned to eu-central-1 in
 * bencher/src/config.rs; this stack is deployed to the same region so the task
 * reaches them with minimal latency.
 */
export class BenchRunnerStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props: BenchRunnerStackProps) {
    super(scope, id, props);

    // Reuse the account's default VPC; the task runs in a public subnet with a
    // public IP so it reaches AWS APIs + package registries (cargo/npm/pip/gradle)
    // without the cost of a NAT gateway.
    const vpc = ec2.Vpc.fromLookup(this, "DefaultVpc", { isDefault: true });

    const cluster = new ecs.Cluster(this, "RunnerCluster", { vpc });

    const taskDef = new ecs.FargateTaskDefinition(this, "RunnerTaskDef", {
      // Fargate task size is fixed for the task's whole life, so it is chosen for
      // the dominant phase: the run is network-bound on serial-per-cell Lambda
      // invokes and leaves the CPU mostly idle, so extra vCPUs buy nothing there.
      // The build is the only CPU phase and is largely sequential per artifact;
      // 2 vCPU is plenty. Memory stays at 8 GB as headroom for the memory-hungry
      // build steps (Rust fat-LTO links, the gradle/JVM daemon), not the run.
      cpu: 2048,
      memoryLimitMiB: 8192,
      runtimePlatform: {
        cpuArchitecture: ecs.CpuArchitecture.X86_64,
        operatingSystemFamily: ecs.OperatingSystemFamily.LINUX,
      },
    });

    // --- Least-privilege task role -----------------------------------------
    // Scoped to exactly the operations bencher's deploy/run/teardown perform
    // (see bencher/src/aws/*.rs) against the lambdabench-* resources, plus the site
    // publish (S3 sync + CloudFront invalidation).
    const taskRole = taskDef.taskRole;

    const accountId = cdk.Stack.of(this).account;
    const region = cdk.Stack.of(this).region;
    const fnPrefixArn = `arn:aws:lambda:${region}:${accountId}:function:${RESOURCE_WILDCARD}`;

    // --- Execution-role permissions boundary -------------------------------
    // The runner creates the Lambda execution role at run time (it needs the role
    // ARN before CreateFunction, and bencher runs standalone without CDK), so it
    // holds iam:CreateRole/AttachRolePolicy/PutRolePolicy on role/lambdabench-*.
    // Left unbounded that is a privilege-escalation path: create a lambdabench-*
    // role, attach AdministratorAccess, pass it to a Lambda. This managed policy is
    // the hard ceiling on what any role the runner creates can do: exactly the
    // services the benchmark's execution role uses (see bencher/src/aws/iam.rs).
    // A bounded role's effective permissions are the intersection of its policies
    // and this boundary, so a broader attached policy grants nothing beyond it. The
    // ARN is required as the boundary on every CreateRole (condition below) and
    // passed to the container so bencher can set it.
    const execRoleBoundary = new iam.ManagedPolicy(this, "ExecRoleBoundary", {
      description:
        "Ceiling for lambdabench Lambda execution roles created by the benchmark runner.",
      statements: [
        // AWSLambdaBasicExecutionRole equivalent, scoped to the lambdabench log groups.
        new iam.PolicyStatement({
          actions: [
            "logs:CreateLogGroup",
            "logs:CreateLogStream",
            "logs:PutLogEvents",
          ],
          resources: [
            `arn:aws:logs:${region}:${accountId}:log-group:/aws/lambda/${RESOURCE_WILDCARD}`,
            `arn:aws:logs:${region}:${accountId}:log-group:/aws/lambda/${RESOURCE_WILDCARD}:*`,
          ],
        }),
        // The scenario data-plane calls: DDB read/write, KMS encrypt, S3 read/write.
        new iam.PolicyStatement({
          actions: ["dynamodb:GetItem", "dynamodb:PutItem"],
          resources: [
            `arn:aws:dynamodb:${region}:${accountId}:table/${RESOURCE_WILDCARD}`,
          ],
        }),
        new iam.PolicyStatement({
          actions: ["kms:Encrypt"],
          resources: [`arn:aws:kms:${region}:${accountId}:key/*`],
        }),
        new iam.PolicyStatement({
          actions: ["s3:GetObject", "s3:PutObject"],
          resources: [`arn:aws:s3:::${RESOURCE_WILDCARD}/*`],
        }),
      ],
    });
    // Requiring this exact boundary on every privilege-granting IAM call closes
    // the escalation; reused in the CreateRole/Attach/Put statement below.
    const boundaryCondition = {
      StringEquals: {
        "iam:PermissionsBoundary": execRoleBoundary.managedPolicyArn,
      },
    };

    // Lambda control + data plane on the lambdabench-* functions.
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: [
          "lambda:CreateFunction",
          "lambda:UpdateFunctionCode",
          "lambda:UpdateFunctionConfiguration",
          "lambda:DeleteFunction",
          "lambda:GetFunction",
          "lambda:PublishVersion",
          "lambda:InvokeFunction",
        ],
        resources: [fnPrefixArn],
      }),
    );

    // CloudWatch Logs teardown: the functions' auto-created log groups
    // (/aws/lambda/lambdabench-*) outlive DeleteFunction. The driver deletes each
    // by exact name (from config, not an account listing), so no DescribeLogGroups
    // is needed; DeleteLogGroup is scoped to the lambdabench-* groups. The trailing
    // ":*" matches the log-stream suffix CloudWatch appends to the ARN in IAM.
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: ["logs:DeleteLogGroup"],
        resources: [
          `arn:aws:logs:${region}:${accountId}:log-group:/aws/lambda/${RESOURCE_WILDCARD}`,
          `arn:aws:logs:${region}:${accountId}:log-group:/aws/lambda/${RESOURCE_WILDCARD}:*`,
        ],
      }),
    );

    const roleArn = `arn:aws:iam::${accountId}:role/${RESOURCE_WILDCARD}`;
    // Privilege-granting actions are gated on the permissions boundary: the
    // condition allows each only when the request sets the boundary to
    // execRoleBoundary, so every role the runner creates or modifies is capped by
    // that ceiling and cannot be minted or grown beyond what the benchmark needs.
    // PutRolePermissionsBoundary is conditioned the same way (it can only set the
    // boundary to execRoleBoundary, never widen it) because bencher::ensure_role
    // converges a pre-existing boundary-less role onto the ceiling via that call.
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: [
          "iam:CreateRole",
          "iam:AttachRolePolicy",
          "iam:PutRolePolicy",
          "iam:PutRolePermissionsBoundary",
        ],
        resources: [roleArn],
        conditions: boundaryCondition,
      }),
    );
    // Privilege-removing and read-only actions carry no boundary condition: they
    // cannot escalate, and gating teardown's detach/delete on the boundary would
    // let a boundary change strand roles the runner could no longer clean up.
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: [
          "iam:DeleteRole",
          "iam:GetRole",
          "iam:DetachRolePolicy",
          "iam:DeleteRolePolicy",
        ],
        resources: [roleArn],
      }),
    );
    // PassRole so CreateFunction can attach the execution role, scoped both by name
    // prefix (lambdabench-*) and destination service, so it can only go to Lambda.
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: ["iam:PassRole"],
        resources: [roleArn],
        conditions: {
          StringEquals: { "iam:PassedToService": "lambda.amazonaws.com" },
        },
      }),
    );

    // DynamoDB table lifecycle + seed item.
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: [
          "dynamodb:CreateTable",
          "dynamodb:DeleteTable",
          "dynamodb:DescribeTable",
          "dynamodb:PutItem",
        ],
        resources: [
          `arn:aws:dynamodb:${region}:${accountId}:table/${RESOURCE_WILDCARD}`,
        ],
      }),
    );

    // KMS. The driver does exactly: ListAliases, CreateKey (tagged at creation),
    // CreateAlias, DeleteAlias, ScheduleKeyDeletion, and on teardown ListKeys +
    // ListResourceTags for the orphan sweep (see bencher/src/aws/kms.rs).
    // Encrypt belongs to the Lambda execution role, not this runner.
    //
    // CreateKey, ListAliases and ListKeys require an account-wide resource.
    // kms:TagResource cannot be tag-scoped (the tag is not yet present) and the
    // key id is unknown until CreateKey returns, so it is scoped to in-region keys.
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: ["kms:CreateKey", "kms:ListAliases", "kms:ListKeys"],
        resources: ["*"],
      }),
    );
    // ListResourceTags reads a key's tags during teardown's orphan sweep, so it
    // cannot be tag-scoped (it is the call that discovers whether the tag is
    // present); scope it to in-region keys.
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: ["kms:ListResourceTags"],
        resources: [`arn:aws:kms:${region}:${accountId}:key/*`],
      }),
    );
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: ["kms:TagResource"],
        resources: [`arn:aws:kms:${region}:${accountId}:key/*`],
      }),
    );
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: ["kms:CreateAlias", "kms:DeleteAlias"],
        resources: [
          `arn:aws:kms:${region}:${accountId}:alias/${RESOURCE_WILDCARD}`,
          `arn:aws:kms:${region}:${accountId}:key/*`,
        ],
      }),
    );
    // ScheduleKeyDeletion is scoped by the lambdabench resource tag, not by alias:
    // teardown removes the alias before scheduling, and both the creation-time
    // orphan cleanup and teardown's tag sweep schedule a key that never received
    // the alias, so a kms:ResourceAliases condition would reject them. The tag is
    // present from key creation; its key/value live in ./constants.ts and must
    // mirror config.rs.
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: ["kms:ScheduleKeyDeletion"],
        resources: [`arn:aws:kms:${region}:${accountId}:key/*`],
        conditions: {
          StringEquals: { [`aws:ResourceTag/${KMS_TAG_KEY}`]: KMS_TAG_VALUE },
        },
      }),
    );

    // The lambdabench data bucket (lambdabench-<region>-<account_id>): create,
    // seed/head objects, list, delete on teardown. ListBucket covers HeadBucket and
    // GetObject covers HeadObject (both used by the seed presence checks).
    // PutBucketPublicAccessBlock and PutBucketPolicy back the harden_bucket step
    // (Block Public Access + deny-non-TLS policy).
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: [
          "s3:CreateBucket",
          "s3:DeleteBucket",
          "s3:ListBucket",
          "s3:PutObject",
          "s3:GetObject",
          "s3:DeleteObject",
          "s3:PutBucketPublicAccessBlock",
          "s3:PutBucketPolicy",
        ],
        resources: [
          `arn:aws:s3:::${RESOURCE_WILDCARD}`,
          `arn:aws:s3:::${RESOURCE_WILDCARD}/*`,
        ],
      }),
    );

    // sts:GetCallerIdentity (no resource) - the driver resolves the account id.
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: ["sts:GetCallerIdentity"],
        resources: ["*"],
      }),
    );

    // ECR for the zip-vs-container-image probe family (`bencher probe
    // download-scaling --with-image`): the driver ensures the lambdabench-synthdl
    // repo, logs crane in, assembles+pushes one padded image per size, reads the
    // pushed size back, and tears the repo down by exact name. Private-repo actions
    // are scoped to the lambdabench-* repositories; the padded base is pulled
    // anonymously from ECR Public. GetAuthorizationToken is account-wide because it
    // takes no resource (the token is registry-wide).
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: ["ecr:GetAuthorizationToken"],
        resources: ["*"],
      }),
    );
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: [
          // Repo lifecycle (ensure + teardown).
          "ecr:CreateRepository",
          "ecr:PutLifecyclePolicy",
          "ecr:DeleteRepository",
          // Push: crane pulls existing layers/manifests to dedupe, uploads new
          // layers, and puts the manifest.
          "ecr:BatchCheckLayerAvailability",
          "ecr:GetDownloadUrlForLayer",
          "ecr:BatchGetImage",
          "ecr:InitiateLayerUpload",
          "ecr:UploadLayerPart",
          "ecr:CompleteLayerUpload",
          "ecr:PutImage",
          // Read the pushed size back + per-run image teardown.
          "ecr:DescribeImages",
          "ecr:BatchDeleteImage",
          // Let Lambda pull the image at cold start. Same-account image pull needs
          // one side to grant the Lambda service principal ecr:BatchGetImage +
          // ecr:GetDownloadUrlForLayer. The exec role grants no ECR, so instead
          // Lambda auto-adds the repo retrieval policy on CreateFunction, which it can
          // only do when the creating principal (this task role) holds
          // Get/SetRepositoryPolicy. Without these, image-package functions never
          // leave the Failed state.
          "ecr:GetRepositoryPolicy",
          "ecr:SetRepositoryPolicy",
        ],
        resources: [
          `arn:aws:ecr:${region}:${accountId}:repository/${RESOURCE_WILDCARD}`,
        ],
      }),
    );

    // --- Site publish permissions ------------------------------------------
    props.siteBucket.grantReadWrite(taskRole);
    // The runner only writes archives to ArchiveBucket (run-* and
    // lifecycle-*.json); read access is unnecessary and intentionally withheld.
    props.archiveBucket.grantWrite(taskRole);
    taskRole.addToPrincipalPolicy(
      new iam.PolicyStatement({
        actions: ["cloudfront:CreateInvalidation"],
        resources: [
          `arn:aws:cloudfront::${accountId}:distribution/${props.distribution.distributionId}`,
        ],
      }),
    );

    // --- Container ----------------------------------------------------------
    // Explicit log group name so it is easy to find and tail; otherwise CDK
    // auto-generates a hashed name.
    const logGroup = new logs.LogGroup(this, "RunnerLogGroup", {
      logGroupName: "/lambdabench/runner",
      retention: logs.RetentionDays.ONE_MONTH,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
    });

    taskDef.addContainer("Runner", {
      image: ecs.ContainerImage.fromEcrRepository(
        props.repository,
        props.imageTag,
      ),
      logging: ecs.LogDrivers.awsLogs({
        streamPrefix: "lambdabench-runner",
        logGroup,
      }),
      environment: {
        // Consumed by the container entrypoint (deploy/run-benchmark.sh).
        SITE_BUCKET: props.siteBucket.bucketName,
        ARCHIVE_BUCKET: props.archiveBucket.bucketName,
        DISTRIBUTION_ID: props.distribution.distributionId,
        // Read by bencher (aws/iam.rs): the ARN of the managed policy to set as the
        // permissions boundary on the Lambda execution role it creates, capping it
        // at the same ceiling the runner's IAM grant is gated on. Standalone bencher
        // runs (no CDK) leave this unset and create the role without a boundary.
        LAMBDABENCH_EXEC_ROLE_BOUNDARY_POLICY_ARN:
          execRoleBoundary.managedPolicyArn,
        LAMBDABENCH_SITE_DOMAIN: props.siteDomain,
        LAMBDABENCH_REPO_URL: props.repoUrl,
        ...(props.contactEmail
          ? { LAMBDABENCH_CONTACT_EMAIL: props.contactEmail }
          : {}),
      },
    });

    new cdk.CfnOutput(this, "ClusterName", { value: cluster.clusterName });
    new cdk.CfnOutput(this, "LogGroupName", { value: logGroup.logGroupName });
    new cdk.CfnOutput(this, "TaskDefinitionArn", {
      value: taskDef.taskDefinitionArn,
    });
    // The public subnets to hand to `aws ecs run-task --network-configuration`.
    new cdk.CfnOutput(this, "PublicSubnetIds", {
      value: vpc.publicSubnets.map((s) => s.subnetId).join(","),
    });
  }
}
