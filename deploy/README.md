# Hosting and publishing infrastructure

The operator runbook for lambdabench.dev: how the site is hosted on AWS and how a fresh
benchmark run is published. This is the deployment behind the live site, kept in the repo
for transparency and reproducibility.

Two independent lifecycles:

1. **Hosting infra** (long-lived): a private S3 bucket fronted by CloudFront (via Origin
   Access Control) with a Route 53 ALIAS record. Built once with CDK.
2. **Benchmark + publish** (ephemeral, on-demand): an ECS Fargate task that builds the Lambda
   artifacts, deploys the full function matrix (see the README "Matrix" section for the exact count),
   runs the benchmark, tears them down, then rebuilds the static site from the fresh run
   and publishes it. Nothing benchmark-related runs between runs.

The site bucket, the runner, and its container image asset live in **eu-central-1** (Frankfurt),
matching the region the benchmark itself hardcodes in `bencher/src/config.rs`. The ACM certificate and the
CLOUDFRONT-scoped WAF Web ACL live in **us-east-1** because CloudFront only consumes them from
there; SiteStack references both cross-region.

```
[ECS Fargate task, on-demand] ── doctor → run [build+deploy+benchmark] (hours-long)
   (image in ECR, full toolchain)        │
                                         ├─→ aws s3 cp run-* → ARCHIVE BUCKET (private, no CDN)
                                         │      (archived immediately, before the probes)
                                         ├─→ probe download-start [in-region, non-fatal]
                                         ├─→ probe download-scaling --with-image [1..200 MB, non-fatal]
                                         ├─→ prose check [lifecycle.md claims vs data, non-fatal]
                                         ├─→ aws s3 cp lifecycle-*.json → ARCHIVE BUCKET
                                         ├─→ teardown --yes
                                         └─→ site: npm ci && npm run build
                                                       │   (loaders discover newest results/run-* + lifecycle-*)
                                                       ├─→ findings prose check [ordering claims vs fresh stats.json, non-fatal]
                                                       └─→ aws s3 sync out-site/ → SITE BUCKET
                                                            └─→ cloudfront create-invalidation

[Viewers] → Route53 ALIAS → CloudFront distribution (ACM cert + WAF, Free plan covers DDoS + DNS)
                                   └─ OAC → private S3 SITE BUCKET (eu-central-1)
```

## Why these choices

- **CloudFront flat-rate Free plan** ($0/mo) covers one distribution + one apex domain, Route 53
  DNS, WAF, DDoS protection, serverless edge compute, and a 5 GB S3 storage credit. The built site
  is a few MB (~3 MB, of which `stats.json` is the bulk) and serves well under the 100 GB transfer /
  1M request monthly allowance, so the Free tier is comfortable. (Note: AWS _Free Tier_ promotional accounts cannot subscribe to flat-rate
  plans; a standard paid account is required.)
- **TLS cert and WAF Web ACL are CDK-managed.** The plan also offers a plan-issued TLS cert and an
  auto-created WAF, but those resources sit outside CloudFormation: the next `cdk deploy` reconciles
  the distribution back to "no domain / no cert / no ACL" and detaches them. We instead provision an
  ACM certificate and a CLOUDFRONT-scoped WAF Web ACL in `EdgeStack` (us-east-1, where CloudFront
  requires them) and bind both to the distribution at synth time. The Web ACL mirrors the rule set
  the plan would attach by default.
- **The plan subscription itself is not modelled in IaC, and can't be a custom resource yet.** The
  underlying API _does_ exist (CloudTrail records a real `CreateSubscription` call:
  `eventSource: pricingplanmanager.amazonaws.com`, `readOnly: false`, with `planName`/`planTier`/
  `resourceArns` request params), but it has **no published SDK/CLI service model**. As of boto3 1.43.36
  (verify against the current release): there is no `pricingplanmanager` (or `pricing-plan-manager` /
  `cloudfront-pricing-plans`) client, and the `cloudfront` client has no plan operation. So a CDK `AwsCustomResource` (which dispatches
  through the SDK) has nothing to call, which is why both
  [aws/aws-cdk#37857](https://github.com/aws/aws-cdk/issues/37857) and
  [hashicorp/terraform-provider-aws#45450](https://github.com/hashicorp/terraform-provider-aws/issues/45450)
  are open. A hand-rolled SigV4 call to the undocumented endpoint would be brittle and unsupported.
  **Subscribing the distribution to the plan is one click in the CloudFront v4 console** (below);
  the subscription persists across deploys and republishing content never re-touches it. Revisit a
  custom resource once a service model ships.
- **ECS Fargate, not CodeBuild**, for the run: CodeBuild's maximum build timeout is exactly 8h and
  the run might exceed that ceiling; a Fargate task has no such limit.
- **No clean-URL edge function.** The site is built with Observable's `preserveExtension: true`
  (`site/observablehq.config.js`), so every link, canonical tag, and sitemap entry already points
  at a real `.html` object. CloudFront's `defaultRootObject` serves `/`. No rewrite needed.
- **Default VPC, public subnet, no NAT.** The Fargate task runs with a public IP in the account's
  default VPC and reaches AWS APIs + package registries directly. $0 standing network cost.

## Layout

| Path               | What it is                                                                                                  |
| ------------------ | ----------------------------------------------------------------------------------------------------------- |
| `cdk/`             | CDK app: `EdgeStack`, `SiteStack`, `BenchRunnerStack`.                                                      |
| `Dockerfile`       | Benchmark-runner image (full toolchain + awscli). Built by `cdk deploy` as a Docker asset, for linux/amd64. |
| `run-benchmark.sh` | Container entrypoint: the full pipeline + site publish.                                                     |
| `run.sh`           | Launch one on-demand run via `aws ecs run-task`.                                                            |

## One-time setup

Prerequisites: AWS CLI v2 + Docker (running: `cdk deploy` builds the runner image), credentials for
the target account, Node 20+ for CDK.

### 1. Create the Route 53 hosted zone (console)

Create a public hosted zone for your domain in the Route 53 console and point your registrar's
nameservers at it. Note the **Hosted Zone ID** and the **domain name**.

### 2. Deploy the infrastructure (CDK)

```sh
cd deploy/cdk
npm install
npx cdk bootstrap aws://<account>/eu-central-1   # once per account/region
npx cdk bootstrap aws://<account>/us-east-1      # required for EdgeStack (cert + WAF)
npx cdk deploy --all \
  -c siteDomain=bench.example.com \
  -c hostedZoneId=Z0123456789ABCDEFGHIJ \
  -c repoUrl=https://github.com/you/lambdabench
#   repoUrl is injected as LAMBDABENCH_REPO_URL so the footer links "Source on GitHub".
# optional: -c contactEmail=you@example.com
#   injected as LAMBDABENCH_CONTACT_EMAIL so the published site shows a "Contact"
#   mailto footer link. Omit and the link is absent.
```

This creates the us-east-1 ACM certificate and CLOUDFRONT WAF Web ACL (`EdgeStack`), the
private site bucket, the CloudFront distribution wired to the apex domain + cert + Web ACL,
the Route 53 ALIAS record, and the Fargate cluster + task definition. It also builds the
runner image from `deploy/Dockerfile` and pushes it (see "The runner image" below), so the
first deploy takes as long as a full toolchain build. ACM validation is via DNS in the
existing hosted zone, so the cert issues automatically once the registrar's nameservers point
at the Route 53 zone; if they don't yet, `EdgeStack` will block on the DNS-01 challenge until
they do.
Note the stack outputs (`SiteBucketName`, `ArchiveBucketName`, `DistributionId`, `DistributionDomainName`).

### 3. Subscribe the distribution to the flat-rate plan (console)

The plan subscription is the only step that can't be expressed in CDK today (see "Why these
choices" above). In the [CloudFront v4 console](https://console.aws.amazon.com/cloudfront/v4/home):

1. Open the distribution → its billing should read **Pay-as-you-go**. Click **Switch to a plan**,
   pick **Free**, and confirm. Billing then reads **Free plan ($0/month)**.
2. Click **Manage plan** and confirm the Route 53 hosted zone is attached (it should appear
   automatically because the distribution's apex domain matches a zone in the same account).
   This puts the zone on the plan's DNS allowance; the existing CDK ALIAS record continues
   to point the apex at the distribution and ALIAS-to-CloudFront queries don't count against
   the allowance.

The CDK-managed cert and Web ACL satisfy the plan's domain + WAF requirements, so no further
clicks are needed and nothing the plan adds drifts from CloudFormation.

## The runner image

There is no separate build-and-push step. The runner image is a **CDK Docker asset**:
`cdk deploy` builds `deploy/Dockerfile` with the repo root as its build context, pushes it to
the CDK bootstrap assets repository (`cdk-hnb659fds-container-assets-<account>-<region>`, created
by `cdk bootstrap`), and wires the resulting URI into the Fargate task definition.

The tag CDK assigns is a **content hash of the build context**:

- The image carries the repo (`COPY . /lambdabench`), so the hash identifies which source the
  Fargate task runs.
- A source change produces a different hash, so it cannot overwrite the previous image. The
  bootstrap assets repo is `IMMUTABLE`.
- The task moves to a new image only when a `cdk deploy` pushes one.
- `.dockerignore` at the repo root is honored for both the hash and the staged context, so build
  output and caches neither bloat the asset (~3.6 MB staged) nor churn the hash.

Rebuilds are incremental: Docker layer caching means a source-only change re-runs just the final
`COPY`, and only that layer is pushed. The toolchain layers rebuild when the `Dockerfile` changes.
`cdk deploy` still needs Docker running, and the image is built for `linux/amd64` explicitly
(`platform: Platform.LINUX_AMD64`) because the task definition is `X86_64`, so an arm64 machine
would otherwise produce an image the task cannot start.

The bootstrap assets repo's only lifecycle rule expires _untagged_ images after a year, and every
asset image is tagged, so old runner images are never reclaimed automatically: each `cdk deploy`
from changed source leaves the previous full-toolchain image behind. Delete them by hand when the
repo grows.

### Deploy-time context

Context is **not** carried over between deploys: `-c` values live only in the invoking
process, and nothing writes them to `cdk.json` or `cdk.context.json` (that file caches
lookup-provider results, like the ECS VPC, and is gitignored). Every `cdk deploy` must
repeat the full set. Omitting one is not equally loud:

| Omitted                                 | Result                                                                    |
| --------------------------------------- | ------------------------------------------------------------------------- |
| `siteDomain`, `hostedZoneId`, `repoUrl` | Throws at synth before any AWS call.                                      |
| `contactEmail`                          | No error; the next published site simply drops the "Contact" footer link. |

To avoid retyping them, either add your values to the `context` block of `cdk.json`
(they're public config, not secrets) or wrap the invocation in a shell alias/script.

## Running the benchmark + publishing

```sh
deploy/run.sh                    # launch one hours-long run, then auto-publish the site
KEEP_RESOURCES=1 deploy/run.sh   # leave the function matrix deployed after the run
```

Follow progress in the CloudWatch log group `/lambdabench/runner` (stream prefix `lambdabench-runner`).
The task runs the pipeline in the diagram above, then exits. Three operational notes the diagram
can't show:

- The matrix run is archived **immediately after it completes**, before the probes, so a probe or
  build failure can never lose the hours-long run.
- Both `probe` steps and the two prose checks (lifecycle claims vs probe data; findings ordering
  claims vs the freshly built `stats.json`) are **non-fatal**: a failure there is logged but does not
  abort the task at that step. But probe output is not committed, so if a probe produced no data the
  site build fails loud and nothing publishes (the live site keeps the previous publish); the probe
  retries transient per-cell failures itself to avoid that.
- The `download-scaling` step deploys ephemeral `lambdabench-synthdl-*` functions, and with
  `--with-image` also assembles padded container images with `crane` and pushes them to the
  `lambdabench-synthdl` ECR repo; it tears down the functions, images, and repo itself.

The raw `run-*.jsonl.gz` / `run-*.meta.json` and the probe `lifecycle-*-<id>.json` files both land in
the **archive bucket** (a separate private S3 bucket, not fronted by CloudFront, so never publicly
reachable). Only the built site in `out-site/` is synced to the public origin bucket. Inspect or
download archives with the AWS CLI against `ArchiveBucketName` from the stack outputs.

To refresh the data, run `deploy/run.sh` again. The site build's data loaders always pick the newest
`results/run-*` (matrix) and `results/lifecycle-*` (probe) files.

## Verification

1. **Infra:** after `cdk deploy`, the bucket is private and the distribution answers on both
   its `*.cloudfront.net` domain and the apex (`curl -I https://<domain>/` returns `200` with
   the CDK-managed ACM cert and CloudFront headers). After the plan subscription, the
   distribution's billing page reads "Free plan ($0/month)" and the Route 53 zone shows
   attached under **Manage plan**.
2. **Site build (local sanity, no run needed):**
   ```sh
   cd site && LAMBDABENCH_SITE_DOMAIN=<domain> LAMBDABENCH_REPO_URL=<repo> npm run build
   # LAMBDABENCH_REPO_URL is required (the "Source on GitHub" footer link; the build fails without it)
   # add LAMBDABENCH_CONTACT_EMAIL=<email> to render the "Contact" mailto link
   ```
   `out-site/` contains `index.html`, the hashed `stats.*.json`, and `sitemap.xml`/`robots.txt`
   carrying the real domain with `.html` page URLs.
3. **End-to-end:** `deploy/run.sh`; tail logs through all phases. Afterward confirm no `lambdabench-*`
   Lambdas / role / table / KMS key / bench bucket remain (teardown clean) and the live site's
   footer/meta reflects the new `run_id`.
4. **Plan budget:** in the CloudFront console, usage stays near 0% of the Free allowance.
