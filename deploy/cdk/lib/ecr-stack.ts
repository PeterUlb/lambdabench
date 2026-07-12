import * as cdk from "aws-cdk-lib";
import { Construct } from "constructs";
import * as ecr from "aws-cdk-lib/aws-ecr";

/**
 * Holds the private ECR repository for the benchmark-runner image. Kept in its
 * own stack so the image can be built and pushed once, independently of the
 * runner task definition (which references it by repository).
 */
export class EcrStack extends cdk.Stack {
  readonly repository: ecr.Repository;

  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);

    this.repository = new ecr.Repository(this, "RunnerRepo", {
      repositoryName: "lambdabench-runner",
      imageScanOnPush: true,
      // Immutable tags: a tag once pushed cannot be overwritten, so each new
      // image uses a unique tag. The runner stack pins the exact tag to pull via
      // the `runnerImageTag` CDK context (e.g. a date or git SHA), so a new
      // image moves only on an explicit cdk deploy.
      imageTagMutability: ecr.TagMutability.IMMUTABLE,
      // The runner is rebuilt rarely; keep only the few most recent images.
      lifecycleRules: [{ maxImageCount: 5 }],
      removalPolicy: cdk.RemovalPolicy.RETAIN,
    });

    new cdk.CfnOutput(this, "RunnerRepoUri", {
      value: this.repository.repositoryUri,
    });
  }
}
