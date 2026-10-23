#!/bin/bash
# Web Functions under test. <name> is a short name; the function is oneoff-webfn-<name>.
#   web.sh create <name> <scenario> [maxConcurrency] [padMB]
#       package server/server.mjs + the scenario's Node bundle (+ events/<scenario>.json, and an
#       incompressible pad file if padMB > 0), create the function with endpoint e0
#       (ApplicationManaged, LatestRevision) and wait until it serves.
#   web.sh revise <name> [maxConcurrency]
#       create a new revision of the same code (fresh environments: forces a cold start) and wait
#       until the endpoint routes to it. Prints the elapsed time on stderr.
#   web.sh recode <name> [maxConcurrency] [sleepCpuMs]
#       like revise, but with changed code: appends a comment to the package's index.mjs (and, with
#       sleepCpuMs, makes /sleep burn that much CPU first), uploads it as a new object version, and
#       creates the revision from that.
#   web.sh endpoint <name> <endpoint> <ApplicationManaged|IamAuth>
#       add another HomeRegion endpoint (its URL is saved as <name>-<endpoint>.url).
#   WEB_INIT_MS=<ms> holds the entry module's import (PROBE_INIT_MS), WEB_LISTEN_MS=<ms> then
#   delays listen() after it (PROBE_LISTEN_DELAY_MS), WEB_DEFER_MS=<ms> makes every request wait
#   that long after startup instead (PROBE_DEFER_MS), and WEB_TIMEOUT=<s> sets timeoutSeconds
#   (default 30), for the revision that create, revise or recode makes.
source "$(dirname "$0")/common.sh"

revision_config() { # <name> <maxConcurrency>
  local name="$1" mc="$2"
  read -r key ver <"$OUT/$name.code"
  cat <<J
{"buildConfig":{"codeConfig":{"s3Object":{"bucket":"$BUCKET","key":"$key","versionId":"$ver"}},"runtimeConfig":{"runtime":"nodejs24.x"}},
 "serviceConfig":{"executionRoleArn":"$ROLE_ARN","maxConcurrencyPerEnvironment":$mc,"timeoutSeconds":${WEB_TIMEOUT:-30},
  "environmentVariables":{"AWS_LAMBDA_NODEJS_ENTRYPOINT":"index.mjs","LAMBDABENCH_TABLE":"$TABLE","LAMBDABENCH_KEY":"seed","REVISION_NONCE":"$(date +%s%N)","PROBE_INIT_MS":"${WEB_INIT_MS:-0}","PROBE_LISTEN_DELAY_MS":"${WEB_LISTEN_MS:-0}","PROBE_DEFER_MS":"${WEB_DEFER_MS:-0}"},
  "telemetryConfig":{"loggingConfig":{"systemLogLevel":"INFO","applicationLogLevel":"INFO"}}}}
J
}

wait_serving() { # <fn> <endpoint> <revisionId>
  while :; do
    local st
    st=$(aws lambda-web get-web-function-endpoint --function-name "$1" --endpoint-name "$2" \
      --query '[state, updateStatus, revisionWeights[0].revisionId]' --output text)
    [[ "$st" == Active* && "$st" != *InProgress* && "$st" == *"$3"* ]] && return
    sleep 2
  done
}

create() {
  local name="$1" scenario="$2" mc="${3:-64}" pad="${4:-0}" fn="$PREFIX-$1" pkg="$OUT/build/web-$1"
  rm -rf "$pkg" && mkdir -p "$pkg"
  cp "$NODE_DIST/$scenario"/* "$pkg/" && mv "$pkg/index.mjs" "$pkg/scenario.mjs"
  cp "$HERE/server/server.mjs" "$pkg/index.mjs"
  cp "$HERE/server/background.mjs" "$pkg/background.mjs"
  [ -f "$EVENTS/$scenario.json" ] && cp "$EVENTS/$scenario.json" "$pkg/event.json"
  [ "$pad" -gt 0 ] && head -c $((pad * 1000000)) /dev/urandom >"$pkg/pad.bin"
  rm -f "$pkg.zip" && (cd "$pkg" && zip -qr "$pkg.zip" .)
  local key="code/$fn/$scenario.zip"
  echo "$key $(aws s3api put-object --bucket "$BUCKET" --key "$key" --body "$pkg.zip" --query VersionId --output text)" >"$OUT/$name.code"
  local out
  out=$(aws lambda-web create-web-function --function-name "$fn" --revision-config "$(revision_config "$name" "$mc")" \
    --endpoint-config '{"endpointName":"e0","endpointType":"HomeRegion","authType":"ApplicationManaged","autoDeploymentMode":"LatestRevision"}')
  echo "https://$(echo "$out" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["endpoint"]["domainName"])')" >"$OUT/$name.url"
  wait_serving "$fn" e0 "$(echo "$out" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["revision"]["revisionId"])')"
  echo "$fn $(url_of "$name")"
}

revise() {
  local name="$1" mc="${2:-64}" fn="$PREFIX-$1" t0 rid
  t0=$(date +%s.%N)
  rid=$(aws lambda-web create-web-function-revision --function-name "$fn" \
    --build-config "$(revision_config "$name" "$mc" | python3 -I -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["buildConfig"]))')" \
    --service-config "$(revision_config "$name" "$mc" | python3 -I -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["serviceConfig"]))')" \
    --query revisionId --output text)
  wait_serving "$fn" e0 "$rid"
  echo "$rid live after $(python3 -I -c "import time; print(round(time.time() - $t0, 1))")s" >&2
  echo "$rid"
}

recode() {
  local name="$1" mc="${2:-64}" cpu="${3:-}" pkg="$OUT/build/web-$1" key
  read -r key _ <"$OUT/$name.code"
  [ -n "$cpu" ] && sed -i "s|^      case \"/sleep\":\$|&\n        busy($cpu);|" "$pkg/index.mjs"
  echo "// recode $(date +%s%N)" >>"$pkg/index.mjs"
  rm -f "$pkg.zip" && (cd "$pkg" && zip -qr "$pkg.zip" .)
  echo "$key $(aws s3api put-object --bucket "$BUCKET" --key "$key" --body "$pkg.zip" --query VersionId --output text)" >"$OUT/$name.code"
  revise "$name" "$mc"
}

endpoint() {
  local name="$1" ep="$2" auth="$3" fn="$PREFIX-$1" dom
  dom=$(aws lambda-web create-web-function-endpoint --function-name "$fn" --endpoint-name "$ep" \
    --endpoint-type HomeRegion --auth-type "$auth" --auto-deployment-mode LatestRevision --query domainName --output text)
  until [ "$(aws lambda-web get-web-function-endpoint --function-name "$fn" --endpoint-name "$ep" --query state --output text)" = Active ]; do sleep 3; done
  echo "https://$dom" >"$OUT/$name-$ep.url"
  echo "$fn/$ep https://$dom"
}

cmd="${1:-}"; shift || true
case "$cmd" in create | revise | recode | endpoint) "$cmd" "$@" ;; *) sed -n '2,19p' "$0" >&2; exit 2 ;; esac
