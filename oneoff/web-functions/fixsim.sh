#!/bin/bash
# What a wrapper that did not load undici would save an Express + DynamoDB app (fixsim/). Two Web
# Functions with the same bundle: oneoff-webfn-ddb-control (a no-op preload) and oneoff-webfn-ddb-fix
# (pre-fix.mjs keeps the wrapper's `import http` from loading undici). Needs `resources.sh up`
# (role, bucket, the seeded table) and `cd fixsim && npm install`.
#   fixsim.sh create           build, create both functions
#   fixsim.sh revise           new revision of both: fresh environments
#   fixsim.sh burst [n]        n concurrent GetItem requests (+2 s) at each function, interleaved; summary
source "$(dirname "$0")/common.sh"
SIM="$HERE/fixsim"

build() {
  (cd "$SIM" && npx --no-install esbuild app.mjs --bundle --platform=node --format=esm --outfile=dist/index.mjs \
    --log-level=warning --banner:js='import { createRequire } from "module"; const require = createRequire(import.meta.url);' \
    && cp pre-control.mjs pre-fix.mjs dist/)
  rm -f "$OUT/build/fixsim.zip" && (cd "$SIM/dist" && zip -qr "$OUT/build/fixsim.zip" .)
  aws s3api put-object --bucket "$BUCKET" --key code/$PREFIX-fixsim/fixsim.zip --body "$OUT/build/fixsim.zip" --query VersionId --output text
}
build_config() { echo "{\"codeConfig\":{\"s3Object\":{\"bucket\":\"$BUCKET\",\"key\":\"code/$PREFIX-fixsim/fixsim.zip\",\"versionId\":\"$1\"}},\"runtimeConfig\":{\"runtime\":\"nodejs24.x\"}}"; }
service_config() { # <variant>
  echo "{\"executionRoleArn\":\"$ROLE_ARN\",\"maxConcurrencyPerEnvironment\":64,
    \"environmentVariables\":{\"AWS_LAMBDA_NODEJS_ENTRYPOINT\":\"index.mjs\",\"NODE_OPTIONS\":\"--import /var/task/pre-$1.mjs\",\"VARIANT\":\"$1\",\"TABLE\":\"$TABLE\",\"REVISION_NONCE\":\"$(date +%s%N)\"},
    \"telemetryConfig\":{\"loggingConfig\":{\"systemLogLevel\":\"INFO\",\"applicationLogLevel\":\"INFO\"}}}"
}
wait_serving() { # <fn> <revisionId>
  while :; do
    local st
    st=$(aws lambda-web get-web-function-endpoint --function-name "$1" --endpoint-name e0 \
      --query '[state, updateStatus, revisionWeights[0].revisionId]' --output text)
    [[ "$st" == Active* && "$st" != *InProgress* && "$st" == *"$2"* ]] && return
    sleep 3
  done
}

create() {
  local ver out v
  ver=$(build)
  for v in control fix; do
    out=$(aws lambda-web create-web-function --function-name "$PREFIX-ddb-$v" \
      --revision-config "{\"buildConfig\":$(build_config "$ver"),\"serviceConfig\":$(service_config $v)}" \
      --endpoint-config '{"endpointName":"e0","endpointType":"HomeRegion","authType":"ApplicationManaged","autoDeploymentMode":"LatestRevision"}')
    echo "https://$(echo "$out" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["endpoint"]["domainName"])')" >"$OUT/ddb-$v.url"
    wait_serving "$PREFIX-ddb-$v" "$(echo "$out" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["revision"]["revisionId"])')"
    echo "$PREFIX-ddb-$v $(url_of ddb-$v)"
  done
}

revise() {
  local ver v rid
  ver=$(build)
  for v in control fix; do
    rid=$(aws lambda-web create-web-function-revision --function-name "$PREFIX-ddb-$v" --build-config "$(build_config "$ver")" \
      --service-config "$(service_config $v)" --query revisionId --output text)
    wait_serving "$PREFIX-ddb-$v" "$rid"
    sleep 3
  done
}

burst() {
  local n="${1:-40}" dir="$OUT/build/fixsim-burst" i v
  rm -rf "$dir" && mkdir -p "$dir"
  for i in $(seq "$n"); do
    for v in control fix; do curl -s -m 30 "$(url_of ddb-$v)/item?sleep=2000" >"$dir/$v-$i.json" & done
  done
  wait
  mkdir -p "$OUT/results/fixsim"
  python3 -I - "$dir" "$OUT/results/fixsim/burst-$(date +%s).json" <<'PY'
import glob, json, statistics, sys
envs = {}
for f in glob.glob(sys.argv[1] + "/*.json"):
    try:
        j = json.load(open(f))
    except ValueError:
        print("unparsable response:", open(f).read()[:200]); continue
    envs[(j["variant"], j["bootId"])] = j
envs = list(envs.values())
json.dump(envs, open(sys.argv[2], "w"))
print(f"{len(envs)} environments -> {sys.argv[2]}")
groups = {}
for e in envs:
    groups.setdefault((e["cpu"], e["variant"]), []).append(e)
p50 = lambda xs: statistics.median(xs) if xs else float("nan")
for (cpu, variant), v in sorted(groups.items()):
    print(f"  {cpu} {variant:7s} n={len(v):2d}  first line {p50([e['firstLineMs'] for e in v]):6.1f}  listen {p50([e['listenMs'] for e in v]):6.1f}"
          f"  first req {p50([e['firstReqMs'] for e in v]):6.1f}  first GetItem {p50([e['firstGetMs'] for e in v]):6.1f}"
          f"  -> done {p50([e['firstGetDoneMs'] for e in v]):6.1f}  cpu@done {p50([e['afterFirstGet']['cpuMs'] for e in v]):5.0f}"
          f"  undici: first line {sum(e['atFirstLine']['undici'] for e in v)}/{len(v)}, after GetItem {sum(e['afterFirstGet']['undici'] for e in v)}/{len(v)}, now {sum(e['now']['undici'] for e in v)}/{len(v)}")
PY
}

cmd="${1:-}"; shift || true
case "$cmd" in create | revise | burst) "$cmd" "$@" ;; *) sed -n '2,9p' "$0" >&2; exit 2 ;; esac
