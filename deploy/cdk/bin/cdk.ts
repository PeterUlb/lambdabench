#!/usr/bin/env node
// Entry point for the LambdaBench hosting + benchmark-runner infrastructure.
//
// Four stacks. The site, ECR, and runner are pinned to eu-central-1 (colocated
// with the benchmark's own resources, which bencher/src/config.rs hardcodes to
// that region). EdgeStack is pinned to us-east-1 because CloudFront only
// consumes ACM certificates and CLOUDFRONT-scoped WAF Web ACLs from there.
//   - EdgeStack:        us-east-1 ACM certificate + CLOUDFRONT WAF Web ACL,
//                       referenced cross-region by SiteStack.
//   - SiteStack:        long-lived static-hosting infra (private S3 origin +
//                       CloudFront via OAC + Route 53 ALIAS).
//   - EcrStack:         private ECR repo holding the benchmark-runner image.
//   - BenchRunnerStack: on-demand ECS Fargate task that builds/deploys/runs
//                       the benchmark, then rebuilds and publishes the site.
//
// The CloudFront flat-rate plan subscription is the only piece not modelled here:
// the API exists (CloudTrail records CreateSubscription against
// pricingplanmanager.amazonaws.com) but has no published SDK/CLI service model yet
// (verified against boto3 1.43.36; see aws/aws-cdk#37857,
// hashicorp/terraform-provider-aws#45450), so an AwsCustomResource has nothing to
// dispatch. Subscribing is a one-time CloudFront-console click (deploy/README.md);
// revisit once a service model ships.
import * as cdk from "aws-cdk-lib";
import { SiteStack } from "../lib/site-stack";
import { EcrStack } from "../lib/ecr-stack";
import { BenchRunnerStack } from "../lib/bench-runner-stack";
import { EdgeStack } from "../lib/edge-stack";

// The benchmark and its companion site are pinned to Frankfurt.
const REGION = "eu-central-1";

const app = new cdk.App();

// Account comes from the ambient CLI credentials; region is fixed. Route 53 is a
// global service but the hosted zone is referenced by id/name, so an env-bound
// stack is required for the cross-stack references and the ECS VPC lookup.
const env: cdk.Environment = {
  account: process.env.CDK_DEFAULT_ACCOUNT,
  region: REGION,
};

// Required deploy-time context (pass via `cdk deploy -c key=value` or cdk.context.json):
//   siteDomain      - apex domain the site is served on, e.g. "bench.example.com".
//   hostedZoneId    - id of the existing Route 53 hosted zone for that domain.
//   runnerImageTag  - immutable ECR tag of the runner image the Fargate task pulls
//                     (e.g. "v20260625", a git SHA, or a release date). The repo is
//                     immutable, so each new build pushes a new tag and bumps this
//                     context value plus a CDK redeploy. Falls back to "bootstrap"
//                     on a first deploy (named so, not "latest", since an immutable
//                     tag is write-once, never a moving pointer).
//   repoUrl         - public source-repo URL (e.g. "https://github.com/you/lambdabench").
//                     Injected as LAMBDABENCH_REPO_URL so the published site links the
//                     footer's "Source on GitHub".
//   contactEmail    - OPTIONAL contact email. Injected as LAMBDABENCH_CONTACT_EMAIL
//                     so the published site shows a "Contact" mailto footer
//                     link. Omit and the link is absent. Note: a public mailto
//                     is exposed to spam scrapers; use a retireable address.
// The hosted zone is created by hand in the console (see deploy/README.md); its
// name is derived from siteDomain.
const siteDomain = app.node.tryGetContext("siteDomain") as string | undefined;
const hostedZoneId = app.node.tryGetContext("hostedZoneId") as
  string | undefined;
const repoUrl = app.node.tryGetContext("repoUrl") as string | undefined;
const contactEmail = app.node.tryGetContext("contactEmail") as
  string | undefined;
const runnerImageTag =
  (app.node.tryGetContext("runnerImageTag") as string | undefined) ??
  "bootstrap";

if (!siteDomain) {
  throw new Error(
    "siteDomain is required (pass via `-c siteDomain=bench.example.com`).",
  );
}
if (!hostedZoneId) {
  throw new Error(
    "hostedZoneId is required (pass via `-c hostedZoneId=Z0123456789ABCDEFGHIJ`).",
  );
}
if (!repoUrl) {
  throw new Error(
    "repoUrl is required (pass via `-c repoUrl=https://github.com/you/lambdabench`).",
  );
}
// `bootstrap` is the documented first-deploy default (not `latest`: the ECR repo
// is immutable, so a tag is write-once and `latest` would falsely imply a moving
// pointer). Warn rather than throw so the bootstrap path works, but nudge toward a
// pinned tag.
if (runnerImageTag === "bootstrap") {
  console.warn(
    "runnerImageTag=bootstrap: first-deploy default. The ECR repo is immutable, so this " +
      "tag can be pushed only once; pass an immutable tag (e.g. a git SHA or release date) " +
      "via `-c runnerImageTag=<tag>` for real deploys.",
  );
}

// CloudFront only accepts ACM certificates and CLOUDFRONT-scoped WAF Web ACLs
// from us-east-1, so the cert and ACL live in their own stack pinned there
// and are consumed cross-region by SiteStack. See:
// https://docs.aws.amazon.com/cdk/api/v2/docs/aws-cdk-lib.aws_certificatemanager-readme.html#cross-region-certificates
const edgeStack = new EdgeStack(app, "LambdaBenchEdgeStack", {
  env: { account: env.account, region: "us-east-1" },
  crossRegionReferences: true,
  siteDomain,
  hostedZoneId,
  description:
    "LambdaBench edge resources: us-east-1 ACM certificate + CLOUDFRONT WAF Web ACL.",
});

const siteStack = new SiteStack(app, "LambdaBenchSiteStack", {
  env,
  crossRegionReferences: true,
  siteDomain,
  hostedZoneId,
  certificate: edgeStack.certificate,
  webAcl: edgeStack.webAcl,
  description:
    "LambdaBench static site: private S3 origin + CloudFront (OAC) + Route 53 ALIAS.",
});

const ecr = new EcrStack(app, "LambdaBenchEcrStack", {
  env,
  description: "LambdaBench benchmark-runner container image repository.",
});

new BenchRunnerStack(app, "LambdaBenchRunnerStack", {
  env,
  repository: ecr.repository,
  imageTag: runnerImageTag,
  siteBucket: siteStack.bucket,
  archiveBucket: siteStack.archiveBucket,
  distribution: siteStack.distribution,
  siteDomain,
  repoUrl,
  contactEmail,
  description:
    "LambdaBench on-demand ECS Fargate benchmark runner + site publisher.",
});
