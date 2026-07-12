# LambdaBench hosting + runner CDK app

Infrastructure for hosting the LambdaBench site and running the benchmark on demand.
See **[../README.md](../README.md)** for the full setup, the one-time console steps, and the run workflow.

Stacks (`bin/cdk.ts`):

- `LambdaBenchEdgeStack`: us-east-1 ACM certificate + CLOUDFRONT WAF Web ACL (CloudFront requires both in us-east-1).
- `LambdaBenchSiteStack`: private S3 origin + CloudFront (OAC) + Route 53 ALIAS.
- `LambdaBenchEcrStack`: ECR repo for the benchmark-runner image.
- `LambdaBenchRunnerStack`: on-demand ECS Fargate task that runs the benchmark and publishes the site.

## Commands

```sh
npm install
npx cdk synth -c siteDomain=<domain> -c hostedZoneId=<zoneId> -c repoUrl=<repo>   # synthesize templates
npx cdk deploy --all -c siteDomain=<domain> -c hostedZoneId=<zoneId> -c repoUrl=<repo>
```

`siteDomain`, `hostedZoneId`, and `repoUrl` are required (the app refuses to synth
without them); `contactEmail` and `runnerImageTag` are optional context flags.
See ../README.md for what each injects.
`SiteStack`, `EcrStack`, and `RunnerStack` are pinned to `eu-central-1`; `EdgeStack` is pinned to
`us-east-1` because CloudFront consumes its ACM cert and WAF Web ACL only from there. The CloudFront
flat-rate plan subscription and apex-domain binding are console steps (the API has no SDK/CLI model
yet); see ../README.md.
