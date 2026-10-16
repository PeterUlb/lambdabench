# ecs-express-latency: Lambda -> ECS Express round trip

A one-off comparison point for the warm-floor callout in [`site/src/index.md`](../../site/src/index.md):
how long a warm request takes when the target is an axum hello world behind
[ECS Express Mode](https://docs.aws.amazon.com/AmazonECS/latest/developerguide/express-service-overview.html)
(ALB -> Fargate) instead of a Lambda `Invoke`. It is not part of the benchmark matrix: `bencher`
never builds or deploys it, and its numbers are dated one-off characterizations, not committed data.

## What it measures

- **`server/`**: axum on Fargate, arm64, 1 vCPU / 2 GB, exactly one task (min = max = 1), behind the
  Express-managed public HTTPS ALB in the default VPC. `/` and `/ping` (the health check) return `hello`.
- **`caller/`**: a Lambda function, arm64, 1769 MB (one full vCPU), not in a VPC, with a minimal
  custom-runtime loop like `oneoff/rust-sdk-init`. One `reqwest` client (rustls, HTTP/2 via ALPN) lives
  for the whole process. Each invocation makes `n` sequential GETs and returns each one's wall-clock
  time, from `send()` through reading the full body, measured with `Instant` inside the sandbox.

The process's first request (new TCP and TLS connection) is reported on its own. A 50-request warm-up
invoke is discarded, then the timed rounds run. All timed requests reuse the pooled connection, which
is the production pattern of a client built at init.

This is a different vantage from the site's ~9 ms Lambda front-end figure: that one is measured by the
`probe` subcommand from the bench runner (Fargate, same region) and is a full `Invoke` minus the
handler's `Duration`. Here the ~1 ms handler is included in the number.

## Running it

```sh
oneoff/ecs-express-latency/run.sh            # 5 rounds x 200 GETs; or: run.sh <rounds> <per-round>
oneoff/ecs-express-latency/run.sh teardown   # delete leftovers if a run was killed (kill -9, lost shell)
```

It builds both binaries inside `amazonlinux:2023` with Docker (arm64, emulated on an x86 host, so the
first build takes several minutes), pushes the server image to a new ECR repository, creates three
IAM roles, an ECS cluster, the Express service and the caller function, waits until the endpoint serves
`hello`, measures, and prints p50/p90/p99. On exit, including on failure or Ctrl-C, it deletes
everything it created: it deletes the Express service, waits until the service is `INACTIVE` and the
load balancer created during the run is gone, and only then deletes the cluster and the infrastructure
role (ECS needs that role to delete the ALB). It also deletes the task definition Express registered
(deleting the service leaves it ACTIVE), the function, both log groups, the ECR repository and the
other two roles. Raw responses land in `.tmp/oneoff-ecs-express-latency/results-<timestamp>.jsonl`.

Environment overrides: `AWS_REGION` (default `eu-central-1`), `PROBE_MEMORY` (caller memory, default
`1769`). Resource names use the `oneoff-ecs-express` prefix, so the script and `bencher teardown` never
touch each other's resources. Cost: an ALB plus one 1 vCPU Fargate task for the ~15-25 minutes a run
takes, i.e. cents.

## Last run

2026-10-16, `eu-central-1`, Fargate arm64 1 vCPU, caller Lambda arm64 1769 MB, HTTP/2, 5 x 200 GETs.
On the pooled connection: p50 0.74 ms, p90 0.82, p99 0.97, max 2.15 (per-round p50 0.71-0.77). The
first request on a new TCP + TLS connection took 9.66 ms. For comparison, the Lambda front end adds
about 9 ms at p50 (`hello` cells of the 2026-10-15 `download-start` probe, measured from the bench runner).
