#!/bin/bash
# Build, deploy, and measure the rust-sdk-init probe, then delete everything it
# created. See README.md in this directory. Usage:
#   oneoff/rust-sdk-init/run.sh [cold-starts-per-config]   (default 20)
#
# Needs docker (with arm64 emulation on an x86 host) and AWS credentials for
# the account to measure in. Resource names use the `oneoff-sdk-init-` prefix,
# outside bencher's `lambdabench-` names, so `bencher teardown` never touches
# them and this script never touches the benchmark's resources.
set -euo pipefail

ROUNDS="${1:-20}"
REGION="${AWS_REGION:-eu-central-1}"
ARCH="${PROBE_ARCH:-arm64}"
MEMORY="${PROBE_MEMORY:-128}"
PREFIX="oneoff-sdk-init"
ROLE="$PREFIX-role"
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
WORK="$REPO/.tmp/oneoff-rust-sdk-init"
export AWS_REGION="$REGION"

# name<TAB>environment, one function per config. The `+` modes time a second
# call after an untimed first one (see src/main.rs).
CONFIGS="default-init	PROBE_MODE=default,PROBE_PHASE=init
noop-init	PROBE_MODE=noop,PROBE_PHASE=init
static-init	PROBE_MODE=static,PROBE_PHASE=init
noop-noop	PROBE_MODE=noop+noop,PROBE_PHASE=init
static-static	PROBE_MODE=static+static,PROBE_PHASE=init
static-default	PROBE_MODE=static+default,PROBE_PHASE=init
static-certs	PROBE_MODE=static+certs,PROBE_PHASE=init
default-invoke	PROBE_MODE=default,PROBE_PHASE=invoke"

mkdir -p "$WORK"

echo "+ build ($ARCH, inside amazonlinux:2023)"
docker run --rm --platform "linux/$ARCH" -v "$REPO:/repo" -w /repo amazonlinux:2023 bash -c '
  set -e
  dnf -q -y install gcc gcc-c++ cmake zip >/dev/null
  curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal >/dev/null
  . ~/.cargo/env
  export CARGO_TARGET_DIR=/repo/.tmp/oneoff-rust-sdk-init/target-'"$ARCH"'
  AWS_LC_SYS_NO_JITTER_ENTROPY=1 cargo build -q --locked --release -p oneoff-rust-sdk-init
  cd "$CARGO_TARGET_DIR/release"
  rm -f /repo/.tmp/oneoff-rust-sdk-init/bootstrap.zip
  zip -q /repo/.tmp/oneoff-rust-sdk-init/bootstrap.zip bootstrap
  # The container runs as root; give the build output back to the repo owner.
  chown -R "$(stat -c %u:%g /repo)" /repo/.tmp/oneoff-rust-sdk-init'

cleanup() {
  echo "+ cleanup"
  while IFS=$'\t' read -r name _; do
    aws lambda delete-function --function-name "$PREFIX-$name" >/dev/null 2>&1 || true
    aws logs delete-log-group --log-group-name "/aws/lambda/$PREFIX-$name" 2>/dev/null || true
  done <<<"$CONFIGS"
  aws iam detach-role-policy --role-name "$ROLE" \
    --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole 2>/dev/null || true
  aws iam delete-role --role-name "$ROLE" 2>/dev/null || true
}
trap cleanup EXIT

echo "+ deploy ($REGION, $ARCH, $MEMORY MB)"
role_arn=$(aws iam create-role --role-name "$ROLE" --query Role.Arn --output text \
  --assume-role-policy-document '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}')
aws iam attach-role-policy --role-name "$ROLE" \
  --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
sleep 15 # IAM propagation; create-function rejects a role it cannot assume yet
while IFS=$'\t' read -r name env; do
  aws lambda create-function --function-name "$PREFIX-$name" --runtime provided.al2023 \
    --architectures "$ARCH" --handler bootstrap --memory-size "$MEMORY" --timeout 30 \
    --role "$role_arn" --zip-file "fileb://$WORK/bootstrap.zip" \
    --environment "Variables={$env}" >/dev/null
done <<<"$CONFIGS"
while IFS=$'\t' read -r name _; do
  aws lambda wait function-active-v2 --function-name "$PREFIX-$name"
done <<<"$CONFIGS"

# Each round changes an environment variable first, which forces the next
# invoke onto a fresh execution environment (a cold start).
out="$WORK/results-$(date +%Y%m%d-%H%M%S).tsv"
echo "+ measure ($ROUNDS cold starts per config) -> $out"
for round in $(seq 1 "$ROUNDS"); do
  while IFS=$'\t' read -r name env; do
    fn="$PREFIX-$name"
    aws lambda update-function-configuration --function-name "$fn" \
      --environment "Variables={$env,ROUND=$round-$RANDOM}" >/dev/null
    aws lambda wait function-updated-v2 --function-name "$fn"
    log=$(aws lambda invoke --function-name "$fn" --log-type Tail "$WORK/response.json" \
      --query LogResult --output text | base64 -d)
    init=$(grep -o 'Init Duration: [0-9.]*' <<<"$log" | awk '{print $3}' || true)
    # Empty when the invoke failed (error body), so one bad round doesn't abort the run.
    ms=$(grep -o '"ms":[0-9.]*' "$WORK/response.json" | cut -d: -f2 || true)
    printf '%s\t%s\t%s\t%s\n' "$round" "$name" "$init" "$ms" >>"$out"
  done <<<"$CONFIGS"
done

echo "+ medians (ms timed inside the function; Init Duration from REPORT)"
while IFS=$'\t' read -r name _; do
  awk -v n="$name" -F'\t' '$2 == n && $4 != "" { print $4 }' "$out" | sort -n |
    awk -v n="$name" '{ v[NR] = $1 } END { printf "%-15s n=%-3d median=%9.2f ms  min=%9.2f  max=%9.2f\n", n, NR, v[int((NR + 1) / 2)], v[1], v[NR] }'
done <<<"$CONFIGS"
