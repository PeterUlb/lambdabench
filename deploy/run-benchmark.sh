#!/usr/bin/env bash
# Container entrypoint for the LambdaBench benchmark-runner (CMD of deploy/Dockerfile).
#
# Runs the full benchmark pipeline against the lambdabench-* resources (all in
# eu-central-1, pinned in bencher/src/config.rs): the matrix run (archived
# immediately), then the in-region download+start probe and the synthetic
# download-scaling probe (including its zip-vs-container-image family, assembled
# with crane), all of which write run-scoped Cold Start Anatomy data into results/,
# then archives the probe outputs and rebuilds+publishes the static site from the
# fresh run. The site's data loaders discover the newest results/ inputs at build
# time; nothing is committed. Designed to run unattended on ECS Fargate.
#
# Required environment (set by the BenchRunnerStack task definition):
#   SITE_BUCKET         - S3 bucket the built site is synced into (public via CloudFront)
#   ARCHIVE_BUCKET      - private S3 bucket for raw run-* archives (no public origin)
#   DISTRIBUTION_ID     - CloudFront distribution to invalidate after publish
#   LAMBDABENCH_SITE_DOMAIN  - apex domain baked into canonical/OG/sitemap URLs
# Optional:
#   KEEP_RESOURCES      - if "1", skip teardown (leave the function matrix deployed)
#   LAMBDABENCH_REPO_URL     - public source-repo URL; if set, the footer links "Source
#                         on GitHub", else it shows "(coming soon)" (inherited by
#                         npm run build)
#   LAMBDABENCH_CONTACT_EMAIL - contact email; if set, the built site shows a
#                         "Contact" mailto footer link (inherited by npm run build)
set -euo pipefail

: "${SITE_BUCKET:?SITE_BUCKET must be set}"
: "${ARCHIVE_BUCKET:?ARCHIVE_BUCKET must be set}"
: "${DISTRIBUTION_ID:?DISTRIBUTION_ID must be set}"
: "${LAMBDABENCH_SITE_DOMAIN:?LAMBDABENCH_SITE_DOMAIN must be set}"

cd /lambdabench

# All lambdabench-* resources live in eu-central-1 (pinned in bencher/src/config.rs).
# The bencher pins the region on every SDK client itself; the raw `aws` CLI calls in
# the publish steps below get it explicitly here too, rather than leaning on the
# Fargate-injected AWS_REGION, so the region is never left to default resolution.
REGION="eu-central-1"

# Tear down the lambdabench-* resource matrix on the way out, however we exit.
# This task is unattended and ephemeral, so a failed deploy/run that left the
# half-built matrix behind would accrue cost (the KMS key, any created
# functions) and, worse, be silently adopted by the next run's idempotent deploy
# so it starts from a stale, partial state. Teardown is idempotent and a no-op
# when nothing was created (e.g. a failed build), so running it on every exit is
# safe. It is best-effort: a teardown failure is logged but does not mask an
# earlier failure's exit code; on an otherwise-successful run it surfaces as the
# exit code so leftover resources are not missed. KEEP_RESOURCES=1 opts out, for
# debugging runs where the live functions are meant to be inspected afterwards.
teardown_on_exit() {
  local rc=$?
  if [[ "${KEEP_RESOURCES:-0}" == "1" ]]; then
    echo "== teardown: SKIPPED (KEEP_RESOURCES=1) =="
    exit "$rc"
  fi
  echo "== teardown: delete the functions + log groups + role + table + KMS + bench bucket =="
  local trc=0
  cargo run --release -p bencher -- teardown --yes || trc=$?
  if [[ "$trc" -ne 0 ]]; then
    echo "WARNING: teardown failed (exit $trc); leftover lambdabench-* resources may remain" >&2
    # Preserve an earlier failure; only surface teardown's own failure when the
    # rest of the run succeeded.
    [[ "$rc" -eq 0 ]] && rc="$trc"
  fi
  exit "$rc"
}

echo "== [1/8] doctor: verify toolchain + AWS identity + region =="
cargo run --release -p bencher -- doctor

# Arm the teardown trap only now: doctor and the env checks above create nothing,
# so a failure there needs no cleanup. Every step from here on can create the
# lambdabench-* resources, so from here any exit must tear them down.
trap teardown_on_exit EXIT

# `run` owns the whole pipeline: it builds the artifacts and deploys the
# function matrix (both scoped to the cells it will invoke) before running, so there is
# no separate build/deploy step here - that would recompile and re-upload the same
# artifacts for nothing. A build or deploy failure still aborts before the run.
# No --profile flag: the publish pipeline runs the default `full` profile (the
# published methodology). Per-cell sample counts vary by scenario and dimension
# (light scenarios 50x50, CPU probes long-warm, SnapStart cold-cycle clamped); the
# resolved counts are recorded per run in the meta's `iteration_buckets`. See
# config::Profile / Cell::iterations.
echo "== [2/8] run: build + deploy + benchmark (hours-long) -> results/run-<id>.{jsonl.gz,meta.json} =="
cargo run --release -p bencher -- run

# Archive the matrix run IMMEDIATELY, before the probes. The matrix run is the
# pipeline's primary, hours-long product; the probes that follow drive repeated
# control-plane cold-forces and can fail, and the site build after them can fail
# too. Archiving the raw run here (rather than only at the end) means neither a
# probe hiccup nor a build failure can lose it: the run is preserved the moment it
# completes, decoupled from everything downstream. The probe outputs are archived
# separately at step [7], after they are produced. The matrix functions stay
# deployed for the probes (teardown is on the EXIT trap, which has not fired yet).
echo "== [3/8] archive matrix run: copy raw results/run-* to s3://$ARCHIVE_BUCKET/ =="
aws s3 cp results/ "s3://$ARCHIVE_BUCKET/" --region "$REGION" \
  --recursive --exclude "*" --include "run-*.jsonl.gz" --include "run-*.meta.json"

# Refresh the pre-Init download+start measurement while the matrix is still
# deployed and dist/ is still built (both left in place by `run`; the EXIT
# teardown trap has not fired yet). The probe writes a run-scoped, gitignored
# results/lifecycle-download-start-<id>.json, which the site's data loader
# discovers at build time (newest wins), so the published Cold Start Anatomy table
# reflects THIS run. Running here (on Fargate, in-region) keeps warm_rtt small, so
# the per-sample residual spread is far tighter than from an off-region client.
#
# This is a documentation probe (caller-side wall-clock), deliberately OUTSIDE the
# benchmark matrix and its fairness/measurement-purity invariants (see DESIGN.md).
# NON-FATAL by the `if ! ...` guard (required because `set -e` would otherwise
# abort): a warning here names WHICH probe failed. There is no committed fallback,
# so if this probe produces no output the site build (step [8]) fails loud at the
# loader and nothing publishes; the live site keeps serving the previous publish,
# and the matrix run is already archived (step [3]). The probe itself now retries
# a transient per-cell failure (bencher probe::retry_transient), so a one-off
# control-plane blip does not reach this warning.
echo "== [4/8] probe download-start: pre-Init download+start (in-region) -> results/lifecycle-download-start-<id>.json =="
if ! cargo run --release -p bencher -- probe download-start; then
  echo "WARNING: probe download-start step failed; the site build will fail loud (no committed fallback)" >&2
fi

# download-scaling probe: deploys ephemeral padded functions (the fixed 1..200 MB
# sweep in config::SYNTH_DEFAULT_SIZES_MB), measures the download+start residual at
# each size, and tears them down itself, writing results/lifecycle-download-scaling-<id>.json.
# Self-contained (independent of the matrix), and in-region here so warm_rtt is tight.
#
# --with-image ALSO runs the zip-vs-container-image family: per size it assembles
# ONE padded container image with crane (daemonless: base pull + one layer append +
# CMD + push to the lambdabench-synthdl ECR repo, no Docker/finch VM, so it runs
# here on Fargate), deploys touched+untouched functions from it, measures, tears the
# images + repo down, and writes results/lifecycle-download-scaling-image-<id>.json.
# Both series are measured in THIS one invocation so the shared chart stays a
# single-session snapshot.
#
# NON-FATAL by the `if ! ...` guard, same rationale as the download-start step:
# the warning names the failing probe, but with no committed fallback a produced-
# nothing outcome fails the build loud at step [8] rather than publishing stale
# data. Its functions/images are named lambdabench-synthdl-* and swept by teardown
# (functions + ECR repo) as a backstop.
echo "== [5/8] probe download-scaling --with-image (in-region) -> results/lifecycle-download-scaling{,-image}-<id>.json =="
if ! cargo run --release -p bencher -- probe download-scaling --with-image; then
  echo "WARNING: probe download-scaling step failed; the site build will fail loud (no committed fallback)" >&2
fi

# Guard the hand-written Cold Start Anatomy prose against data drift: assert the
# key lifecycle.md claims (provisioning floor, 200 MB residual, ~4-8 ms/MB
# slope, runtime families tracking, image init-climb / flat-residual / crossover)
# still hold against the three JSONs the probes just refreshed. Most exact ms on
# the page are derived from those JSONs at build time, but the qualitative shape
# claims are prose, and lifecycle.md is the one page carrying such claims (the other
# pages render from stats.json or use dated README snapshots), so it is the drift
# surface. NON-FATAL: a Lambda-platform shift that moves the numbers should warn
# loudly (so the prose gets revisited) without blocking the publish; the data
# itself is still valid.
echo "== [6/8] prose check: lifecycle.md claims vs refreshed probe data =="
if ! python3 scripts/check-lifecycle-prose.py; then
  echo "WARNING: lifecycle.md prose has drifted from the probe data; revisit the Cold Start Anatomy page" >&2
fi

# Archive the probe outputs to the private archive bucket, alongside the matrix
# run archived at step [3]. Same bucket, not fronted by CloudFront, so these files
# are not publicly reachable; viewers only ever see what the site build emits into
# out-site/. The probe JSONs live in results/ under lifecycle-*-<id>.json (the
# gitignored, run-scoped names the loaders discover), so this preserves the exact
# data the published Cold Start Anatomy page is built from.
echo "== [7/8] archive probe outputs: copy results/lifecycle-*.json to s3://$ARCHIVE_BUCKET/ =="
aws s3 cp results/ "s3://$ARCHIVE_BUCKET/" --region "$REGION" \
  --recursive --exclude "*" --include "lifecycle-*.json"

# Publish before the EXIT trap tears resources down, so the site is built and
# pushed while the run's results are still in hand.
echo "== [8/8] publish: build the site from the newest run and deploy it =="
# The data loaders auto-select the newest results/ inputs: stats.json.js picks the
# newest run-*, and the lifecycle-*.json.js loaders the newest probe outputs.
# LAMBDABENCH_SITE_DOMAIN bakes the real canonical URLs. With no committed probe
# fallback, a probe that produced nothing makes this build fail loud here.
( cd site && npm ci && npm run build )

# Sibling of the lifecycle prose check, for the other drift surface: the ORDERING
# claims the findings prose asserts (SnapStart win/lose split, Go-vs-Rust shapes,
# opt-level and jitter directions, the cache tail floor, the quoted smithy-java
# version) checked against the stats.json the build above just produced. NON-FATAL
# for the same reason as step [6]: a flipped finding should warn loudly without
# blocking the publish; the data itself is still valid.
if ! python3 scripts/check-findings-prose.py; then
  echo "WARNING: findings prose has drifted from the run data; revisit the README/site findings" >&2
fi

# Sync the static bundle in two passes with different cache headers: content-hashed
# assets get a long immutable cache, the un-hashed files a short cache so a
# republish is picked up promptly (the invalidation below also covers it). The
# un-hashed set is HTML/SEO plus the root icons (favicon.svg, apple-touch-icon.png):
# the icons are served under stable URLs, so an immutable header would pin a stale
# icon in browser caches for a year (CloudFront invalidation never reaches those).
#
# Each pass owns --delete over its OWN key space, and the two spaces are disjoint
# (pass 1 excludes the un-hashed set, pass 2 includes only it), so together they
# prune every stale object. A filter-excluded object is never eligible for
# --delete, so pass 1 leaves stale HTML behind and pass 2 must delete it: without
# --delete on pass 2 a renamed or removed page lingers in the bucket and keeps
# being served. The disjoint scopes are also why pass 2's --delete cannot touch
# the assets pass 1 just uploaded: they are excluded from pass 2 entirely.
aws s3 sync site/out-site/ "s3://$SITE_BUCKET/" --delete --region "$REGION" \
  --exclude "*.html" --exclude "robots.txt" --exclude "sitemap.xml" \
  --exclude "favicon.svg" --exclude "apple-touch-icon.png" \
  --cache-control "public, max-age=31536000, immutable"
aws s3 sync site/out-site/ "s3://$SITE_BUCKET/" --delete --region "$REGION" \
  --exclude "*" --include "*.html" --include "robots.txt" --include "sitemap.xml" \
  --include "favicon.svg" --include "apple-touch-icon.png" \
  --cache-control "public, max-age=300"

aws cloudfront create-invalidation --distribution-id "$DISTRIBUTION_ID" --paths '/*' --region "$REGION"

echo "== done: site published to s3://$SITE_BUCKET and invalidated on $DISTRIBUTION_ID =="
