import * as cdk from "aws-cdk-lib";
import { Template, Match } from "aws-cdk-lib/assertions";
import { EdgeStack } from "../lib/edge-stack";
import { SiteStack } from "../lib/site-stack";
import { EcrStack } from "../lib/ecr-stack";
import { BenchRunnerStack } from "../lib/bench-runner-stack";

// Locks the IAM posture of the benchmark runner. The runner must create the
// Lambda execution role at run time and therefore holds privilege-granting IAM
// actions (CreateRole / AttachRolePolicy / PutRolePolicy) on role/lambdabench-*.
// Left ungated that is a privilege-escalation path (create a role, attach
// AdministratorAccess, pass it to a Lambda). These tests pin the two properties
// that close it: a permissions-boundary ceiling exists, and every
// privilege-granting action is conditioned on it. A regression that dropped the
// condition or widened the boundary would be silent in a `cdk deploy`.
describe("BenchRunnerStack IAM posture", () => {
  const account = "111111111111";
  const region = "eu-central-1";
  const app = new cdk.App();
  // Vpc.fromLookup needs a concrete answer at synth time; seed the default-VPC
  // context so the lookup resolves deterministically in the test.
  app.node.setContext(
    `vpc-provider:account=${account}:filter.isDefault=true:region=${region}:returnAsymmetricSubnets=true`,
    {
      vpcId: "vpc-12345",
      vpcCidrBlock: "10.0.0.0/16",
      availabilityZones: [],
      subnetGroups: [
        {
          name: "Public",
          type: "Public",
          subnets: [
            {
              subnetId: "subnet-1",
              availabilityZone: `${region}a`,
              routeTableId: "rtb-1",
              cidr: "10.0.0.0/24",
            },
          ],
        },
      ],
    },
  );

  const edge = new EdgeStack(app, "TestEdgeStack", {
    env: { account, region: "us-east-1" },
    crossRegionReferences: true,
    siteDomain: "bench.example.com",
    hostedZoneId: "Z0123456789ABCDEFGHIJ",
  });
  const site = new SiteStack(app, "TestSiteStack", {
    env: { account, region },
    crossRegionReferences: true,
    siteDomain: "bench.example.com",
    hostedZoneId: "Z0123456789ABCDEFGHIJ",
    certificate: edge.certificate,
    webAcl: edge.webAcl,
  });
  const ecr = new EcrStack(app, "TestEcrStack", { env: { account, region } });
  const runner = new BenchRunnerStack(app, "TestRunnerStack", {
    env: { account, region },
    repository: ecr.repository,
    imageTag: "testtag",
    siteBucket: site.bucket,
    archiveBucket: site.archiveBucket,
    distribution: site.distribution,
    siteDomain: "bench.example.com",
    repoUrl: "https://github.com/example/lambdabench",
  });
  const template = Template.fromStack(runner);

  test("a permissions-boundary managed policy exists for the execution role", () => {
    template.resourceCountIs("AWS::IAM::ManagedPolicy", 1);
  });

  test("the boundary grants only the benchmark data-plane, never iam:* or a wildcard action", () => {
    const policy = template.findResources("AWS::IAM::ManagedPolicy");
    const doc = Object.values(policy)[0].Properties.PolicyDocument;
    const actions = doc.Statement.flatMap((s: { Action: string | string[] }) =>
      Array.isArray(s.Action) ? s.Action : [s.Action],
    );
    // The exact ceiling: logs write + DDB get/put + KMS encrypt + S3 get/put.
    expect(new Set(actions)).toEqual(
      new Set([
        "logs:CreateLogGroup",
        "logs:CreateLogStream",
        "logs:PutLogEvents",
        "dynamodb:GetItem",
        "dynamodb:PutItem",
        "kms:Encrypt",
        "s3:GetObject",
        "s3:PutObject",
      ]),
    );
    // No IAM action and no action wildcard can hide in the ceiling.
    for (const a of actions) {
      expect(a).not.toMatch(/^iam:/);
      expect(a).not.toContain("*");
    }
  });

  test("privilege-granting IAM actions are gated on the permissions boundary", () => {
    // CreateRole / AttachRolePolicy / PutRolePolicy / PutRolePermissionsBoundary
    // must carry the iam:PermissionsBoundary condition; without it the runner
    // could create (or grow, or re-boundary) an unbounded role. Asserting the
    // condition is present on the granting statement is the crux of the fix.
    template.hasResourceProperties("AWS::IAM::Policy", {
      PolicyDocument: {
        Statement: Match.arrayWith([
          Match.objectLike({
            Effect: "Allow",
            Action: [
              "iam:CreateRole",
              "iam:AttachRolePolicy",
              "iam:PutRolePolicy",
              "iam:PutRolePermissionsBoundary",
            ],
            Condition: {
              StringEquals: { "iam:PermissionsBoundary": Match.anyValue() },
            },
          }),
        ]),
      },
    });
  });

  test("PassRole is restricted to the Lambda service", () => {
    template.hasResourceProperties("AWS::IAM::Policy", {
      PolicyDocument: {
        Statement: Match.arrayWith([
          Match.objectLike({
            Action: "iam:PassRole",
            Condition: {
              StringEquals: { "iam:PassedToService": "lambda.amazonaws.com" },
            },
          }),
        ]),
      },
    });
  });

  test("ECR push/pull for the image probe is scoped to lambdabench-* repositories", () => {
    // The zip-vs-image probe family pushes padded images to lambdabench-synthdl.
    // The layer/manifest actions must stay scoped to repository/lambdabench-* so
    // the runner cannot touch any other repo in the account.
    template.hasResourceProperties("AWS::IAM::Policy", {
      PolicyDocument: {
        Statement: Match.arrayWith([
          Match.objectLike({
            Effect: "Allow",
            Action: Match.arrayWith([
              "ecr:CreateRepository",
              "ecr:DeleteRepository",
              "ecr:UploadLayerPart",
              "ecr:PutImage",
              "ecr:BatchDeleteImage",
            ]),
            Resource: Match.stringLikeRegexp("repository/lambdabench-\\*$"),
          }),
        ]),
      },
    });
  });

  test("the task role can let Lambda pull the image (Get/SetRepositoryPolicy, repo-scoped)", () => {
    // Same-account image pull needs one side to grant the Lambda service principal
    // retrieval; Lambda auto-adds the repo policy on CreateFunction only if the
    // creating principal holds Get/SetRepositoryPolicy. Without these, image-package
    // functions never leave Failed, so guard that both are granted and repo-scoped.
    template.hasResourceProperties("AWS::IAM::Policy", {
      PolicyDocument: {
        Statement: Match.arrayWith([
          Match.objectLike({
            Effect: "Allow",
            Action: Match.arrayWith([
              "ecr:GetRepositoryPolicy",
              "ecr:SetRepositoryPolicy",
            ]),
            Resource: Match.stringLikeRegexp("repository/lambdabench-\\*$"),
          }),
        ]),
      },
    });
  });

  test("ECR GetAuthorizationToken is granted (registry-wide, no resource scope)", () => {
    // The ECR auth token is registry-wide and takes no resource, so crane login
    // needs this account-wide. Without it the image probe cannot authenticate.
    template.hasResourceProperties("AWS::IAM::Policy", {
      PolicyDocument: {
        Statement: Match.arrayWith([
          Match.objectLike({
            Effect: "Allow",
            Action: "ecr:GetAuthorizationToken",
            Resource: "*",
          }),
        ]),
      },
    });
  });

  test("the boundary ARN is injected into the runner container", () => {
    template.hasResourceProperties("AWS::ECS::TaskDefinition", {
      ContainerDefinitions: Match.arrayWith([
        Match.objectLike({
          Environment: Match.arrayWith([
            Match.objectLike({
              Name: "LAMBDABENCH_EXEC_ROLE_BOUNDARY_POLICY_ARN",
            }),
          ]),
        }),
      ]),
    });
  });
});
