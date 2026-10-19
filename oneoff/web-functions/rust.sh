#!/bin/bash
# Rust (axum) on Web Functions via process.execve (rust-execve/), serving the `oneclient` scenario.
# Two Web Functions with the same package:
#   oneoff-webfn-rs-index    index.mjs execs into the server after AWS's wrapper ran
#   oneoff-webfn-rs-preload  a NODE_OPTIONS=--require preload execs before the wrapper; the server
#                            then calls /load and /ready itself
# Needs `resources.sh up` and `cargo lambda` (builds with the benchmark's lambda profile).
#   rust.sh create                  build, create both functions
#   rust.sh cold <variant> <cycles> new revision per cycle, then 6 sequential requests from the
#                                   in-region client; writes results/cold-web-rs-<variant>.jsonl
#                                   (summarize with `analyze.py cold-web rs-<variant>`)
source "$(dirname "$0")/common.sh"
SRC="$HERE/rust-execve"

build() {
  (cd "$SRC" && AWS_LC_SYS_NO_JITTER_ENTROPY=1 cargo lambda build --profile lambda --arm64 >&2)
  local pkg="$OUT/build/rust-execve"
  rm -rf "$pkg" && mkdir -p "$pkg"
  cp "$SRC/target/lambda/server/bootstrap" "$pkg/server" && chmod 755 "$pkg/server"
  cp "$SRC/index.mjs" "$SRC/pre.cjs" "$pkg/"
  rm -f "$pkg.zip" && (cd "$pkg" && zip -qr "$pkg.zip" .)
  aws s3api put-object --bucket "$BUCKET" --key code/$PREFIX-rs/rust-execve.zip --body "$pkg.zip" --query VersionId --output text
}
build_config() { echo "{\"codeConfig\":{\"s3Object\":{\"bucket\":\"$BUCKET\",\"key\":\"code/$PREFIX-rs/rust-execve.zip\",\"versionId\":\"$1\"}},\"runtimeConfig\":{\"runtime\":\"nodejs24.x\"}}"; }
service_config() { # <variant>
  local extra=""
  [ "$1" = preload ] && extra=",\"NODE_OPTIONS\":\"--require /var/task/pre.cjs\""
  echo "{\"executionRoleArn\":\"$ROLE_ARN\",\"maxConcurrencyPerEnvironment\":64,
    \"environmentVariables\":{\"AWS_LAMBDA_NODEJS_ENTRYPOINT\":\"index.mjs\",\"VARIANT\":\"$1\",\"LAMBDABENCH_TABLE\":\"$TABLE\",\"LAMBDABENCH_KEY\":\"seed\",\"REVISION_NONCE\":\"$(date +%s%N)\"$extra},
    \"telemetryConfig\":{\"loggingConfig\":{\"systemLogLevel\":\"INFO\",\"applicationLogLevel\":\"INFO\"}}}"
}
wait_serving() { # <fn> <revisionId>
  while :; do
    local st
    st=$(aws lambda-web get-web-function-endpoint --function-name "$1" --endpoint-name e0 \
      --query '[state, updateStatus, revisionWeights[0].revisionId]' --output text)
    [[ "$st" == Active* && "$st" != *InProgress* && "$st" == *"$2"* ]] && return
    sleep 2
  done
}

create() {
  local ver out v
  ver=$(build)
  echo "$ver" >"$OUT/rs.codever"
  for v in index preload; do
    out=$(aws lambda-web create-web-function --function-name "$PREFIX-rs-$v" \
      --revision-config "{\"buildConfig\":$(build_config "$ver"),\"serviceConfig\":$(service_config $v)}" \
      --endpoint-config '{"endpointName":"e0","endpointType":"HomeRegion","authType":"ApplicationManaged","autoDeploymentMode":"LatestRevision"}')
    echo "https://$(echo "$out" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["endpoint"]["domainName"])')" >"$OUT/rs-$v.url"
    wait_serving "$PREFIX-rs-$v" "$(echo "$out" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["revision"]["revisionId"])')"
    echo "$PREFIX-rs-$v $(url_of rs-$v)"
  done
}

cold() {
  local v="$1" cycles="$2" fn="$PREFIX-rs-$1" out="$OUT/results/cold-web-rs-$1.jsonl" url ver c rid
  url="$(url_of rs-$v)/item"; ver=$(cat "$OUT/rs.codever"); : >"$out"
  for c in $(seq 1 "$cycles"); do
    rid=$(aws lambda-web create-web-function-revision --function-name "$fn" --build-config "$(build_config "$ver")" \
      --service-config "$(service_config "$v")" --query revisionId --output text)
    wait_serving "$fn" "$rid"
    client_run "tmp-cold-rs-$v" "{\"action\":\"seq\",\"url\":\"$url\",\"n\":6}"
    python3 -I -c 'import json,sys; print(json.dumps({"cycle":int(sys.argv[2]),"rev":sys.argv[3],"reqs":json.load(open(sys.argv[1]))}))' \
      "$OUT/results/tmp-cold-rs-$v.json" "$c" "$rid" >>"$out"
  done
  rm -f "$OUT/results/tmp-cold-rs-$v.json"; echo "wrote $out"
}

cmd="${1:-}"; shift || true
case "$cmd" in create | cold) "$cmd" "$@" ;; *) sed -n '2,12p' "$0" >&2; exit 2 ;; esac
