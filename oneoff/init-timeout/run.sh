#!/bin/bash
# Init budget vs function timeout, on-demand and provisioned concurrency.
#   run.sh up                 create the role, zip fn/ (Node) and build rust/ (cargo lambda)
#   run.sh fn <name> <initMs> <timeoutS> [reinitMs]   create oneoff-inittimeout-<name> (arm64,
#                             1024 MB); reinitMs is the init length after a hang. PROBE_RUNTIME=rust
#                             deploys the Rust twin on provided.al2023 instead of Node, and
#                             PROBE_RUNTIME=java the Java twin on java21 (PROBE_SNAPSTART=1 turns
#                             on SnapStart for published versions). PROBE_MEMORY
#                             sets the memory size, and PROBE_ENV adds Node variables (e.g.
#                             "INIT_WORK=800,REINIT_WORK=800", see fn/index.mjs). PROBE_LAYER adds a
#                             layer, such as the slow extension that `run.sh ext` publishes.
#   run.sh ext                publish ext/ (an external extension, slow per EXT_INIT_MS /
#                             EXT_REINIT_MS) as a layer and print its ARN
#   run.sh pc <name>          publish a version, alias `live`, provisioned concurrency 1, wait READY
#   run.sh publish <name>     publish a version and point alias `live` at it (for SnapStart)
#   run.sh invoke <name> [qualifier] [hang|exit|oom|<json>]   one sync invoke; prints wall time,
#                             result, log tail
#   run.sh logs <name>        every INIT_REPORT / REPORT / timeout line from the log group
#   run.sh down               delete functions, log groups, role
set -euo pipefail
export AWS_REGION="${AWS_REGION:-eu-central-1}"
PREFIX="oneoff-inittimeout"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$(cd "$HERE/../.." && pwd)/.tmp/oneoff-init-timeout"
mkdir -p "$OUT"
ACCOUNT="$(aws sts get-caller-identity --query Account --output text)"
ROLE="$PREFIX-role"
ROLE_ARN="arn:aws:iam::$ACCOUNT:role/$ROLE"

up() {
  if ! aws iam get-role --role-name "$ROLE" >/dev/null 2>&1; then
    aws iam create-role --role-name "$ROLE" \
      --assume-role-policy-document '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}' >/dev/null
    aws iam attach-role-policy --role-name "$ROLE" --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
    sleep 10 # a freshly created role takes a few seconds to become assumable
  fi
  rm -f "$OUT/fn.zip" && (cd "$HERE/fn" && zip -q "$OUT/fn.zip" index.mjs)
  (cd "$HERE/rust" && cargo lambda build -q --release --arm64 -p oneoff-init-timeout --output-format zip \
    --lambda-dir "$OUT/rust")
  rm -rf "$OUT/java" "$OUT/java.zip" && javac --release 21 -d "$OUT/java" "$HERE/java/Probe.java" &&
    (cd "$OUT/java" && zip -q "$OUT/java.zip" Probe.class)
  echo "up: $ROLE"
}

fn() {
  local f="$PREFIX-$1" runtime=nodejs24.x handler=index.handler zip="$OUT/fn.zip"
  case "${PROBE_RUNTIME:-node}" in
    rust) runtime=provided.al2023 handler=bootstrap zip="$OUT/rust/bootstrap/bootstrap.zip" ;;
    java) runtime=java21 handler=Probe::handleRequest zip="$OUT/java.zip" ;;
  esac
  aws lambda create-function --function-name "$f" --runtime "$runtime" --architectures arm64 \
    --memory-size "${PROBE_MEMORY:-1024}" --timeout "$3" --role "$ROLE_ARN" --handler "$handler" \
    --zip-file "fileb://$zip" --environment "Variables={INIT_MS=$2${4:+,REINIT_MS=$4}${PROBE_ENV:+,$PROBE_ENV}}" \
    ${PROBE_LAYER:+--layers "$PROBE_LAYER"} ${PROBE_SNAPSTART:+--snap-start ApplyOn=PublishedVersions} >/dev/null
  aws lambda wait function-active-v2 --function-name "$f"
  echo "$f $runtime init=${2}ms timeout=${3}s${4:+ reinit=${4}ms}"
}

pc() {
  local f="$PREFIX-$1" v
  v=$(aws lambda publish-version --function-name "$f" --query Version --output text)
  aws lambda create-alias --function-name "$f" --name live --function-version "$v" >/dev/null
  aws lambda put-provisioned-concurrency-config --function-name "$f" --qualifier live \
    --provisioned-concurrent-executions 1 >/dev/null
  local s=""
  for _ in $(seq 1 60); do
    s=$(aws lambda get-provisioned-concurrency-config --function-name "$f" --qualifier live --query Status --output text)
    [ "$s" = IN_PROGRESS ] || break; sleep 10
  done
  echo "$f:live (v$v) PC status $s"
}

publish() {
  local f="$PREFIX-$1" v
  v=$(aws lambda publish-version --function-name "$f" --query Version --output text)
  aws lambda wait published-version-active --function-name "$f:$v"
  aws lambda create-alias --function-name "$f" --name live --function-version "$v" >/dev/null
  echo "$f:live (v$v)"
}

invoke() {
  local f="$PREFIX-$1" q="${2:-}" payload='{}' t0 t1 meta
  case "${3:-}" in
    hang) payload='{"hang":true}' ;;
    exit) payload='{"exit":true}' ;;
    oom) payload='{"oom":true}' ;;
    "") ;;
    *) payload="$3" ;;
  esac
  t0=$(date +%s%3N)
  meta=$(aws lambda invoke --function-name "$f" ${q:+--qualifier "$q"} --cli-read-timeout 0 \
    --cli-binary-format raw-in-base64-out --payload "$payload" --log-type Tail \
    --query '[FunctionError,LogResult]' --output text "$OUT/resp.json")
  t1=$(date +%s%3N)
  echo "--- $f${q:+:$q} payload=$payload wall=$((t1 - t0))ms $(date -u +%H:%M:%S)"
  echo "result: $(cat "$OUT/resp.json")"
  echo "$meta" | awk '{print $NF}' | base64 -d 2>/dev/null | grep -E 'INIT_REPORT|REPORT|timed out|Status' || true
}

ext() {
  rm -f "$OUT/ext.zip" && (cd "$HERE/ext" && zip -qr "$OUT/ext.zip" extensions slowext.mjs)
  aws lambda publish-layer-version --layer-name "$PREFIX-slowext" --zip-file "fileb://$OUT/ext.zip" \
    --compatible-architectures arm64 --query LayerVersionArn --output text
}

logs() {
  aws logs filter-log-events --log-group-name "/aws/lambda/$PREFIX-$1" \
    --query 'events[].[timestamp,message]' --output text |
    grep -E 'INIT_START|INIT_REPORT|REPORT|timed out|Status:' | sed 's/\t/ /'
}

down() {
  for f in $(aws lambda list-functions --query "Functions[?starts_with(FunctionName, '$PREFIX-')].FunctionName" --output text); do
    echo "delete function $f"; aws lambda delete-function --function-name "$f"
  done
  for g in $(aws logs describe-log-groups --log-group-name-prefix "/aws/lambda/$PREFIX-" --query 'logGroups[].logGroupName' --output text); do
    echo "delete log group $g"; aws logs delete-log-group --log-group-name "$g"
  done
  for v in $(aws lambda list-layer-versions --layer-name "$PREFIX-slowext" --query 'LayerVersions[].Version' --output text); do
    echo "delete layer $PREFIX-slowext:$v"; aws lambda delete-layer-version --layer-name "$PREFIX-slowext" --version-number "$v"
  done
  if aws iam get-role --role-name "$ROLE" >/dev/null 2>&1; then
    aws iam detach-role-policy --role-name "$ROLE" --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
    aws iam delete-role --role-name "$ROLE"; echo "delete role $ROLE"
  fi
}

cmd="${1:-}"; shift || true
case "$cmd" in up|fn|pc|publish|invoke|ext|logs|down) "$cmd" "$@" ;; *) sed -n '2,20p' "$0" >&2; exit 2 ;; esac
