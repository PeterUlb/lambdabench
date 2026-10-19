#!/bin/bash
# Event Function baselines with a Function URL (auth NONE). The function is oneoff-webfn-<name>.
#   event.sh create <name> node <scenario> <memoryMB>   the benchmark's Node bundle, nodejs24.x
#   event.sh create <name> rust <scenario> <memoryMB>   dist/rust-<scenario>-arm64-o3.zip, provided.al2023
# Both arm64. The Function URL is saved as <name>.url.
source "$(dirname "$0")/common.sh"

create() {
  local name="$1" lang="$2" scenario="$3" mem="$4" fn="$PREFIX-$1" zip runtime handler
  case "$lang" in
    node)
      zip="$OUT/build/event-$name.zip"; rm -f "$zip"
      (cd "$NODE_DIST/$scenario" && zip -qr "$zip" .)
      runtime=nodejs24.x; handler=index.handler ;;
    rust)
      zip="$RUST_DIST/rust-$scenario-arm64-o3.zip"; runtime=provided.al2023; handler=bootstrap ;;
    *) echo "lang must be node or rust" >&2; exit 2 ;;
  esac
  aws lambda create-function --function-name "$fn" --runtime "$runtime" --architectures arm64 \
    --memory-size "$mem" --timeout 30 --role "$ROLE_ARN" --handler "$handler" --zip-file "fileb://$zip" \
    --environment "Variables={LAMBDABENCH_TABLE=$TABLE,LAMBDABENCH_KEY=seed}" >/dev/null
  aws lambda wait function-active-v2 --function-name "$fn"
  local url
  url=$(aws lambda create-function-url-config --function-name "$fn" --auth-type NONE --query FunctionUrl --output text)
  aws lambda add-permission --function-name "$fn" --statement-id url --action lambda:InvokeFunctionUrl \
    --principal '*' --function-url-auth-type NONE >/dev/null
  aws lambda add-permission --function-name "$fn" --statement-id url-invoke --action lambda:InvokeFunction \
    --principal '*' --invoked-via-function-url >/dev/null
  echo "${url%/}" >"$OUT/$name.url"
  echo "$fn $(url_of "$name")"
}

cmd="${1:-}"; shift || true
case "$cmd" in create) create "$@" ;; *) sed -n '2,6p' "$0" >&2; exit 2 ;; esac
