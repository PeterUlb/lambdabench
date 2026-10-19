# Shared settings for the Web Functions probe scripts; sourced, not run.
# Every resource name carries the `oneoff-webfn` prefix, outside bencher's `lambdabench-`
# names, so neither side touches the other's resources. AWS_REGION picks the region
# (default eu-central-1). Results and endpoint URLs go to .tmp/oneoff-web-functions/.
set -euo pipefail
REGION="${AWS_REGION:-eu-central-1}"
export AWS_REGION="$REGION"
PREFIX="oneoff-webfn"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
OUT="$REPO/.tmp/oneoff-web-functions"
mkdir -p "$OUT/results" "$OUT/build"
ACCOUNT="${ACCOUNT:-$(aws sts get-caller-identity --query Account --output text)}"
export ACCOUNT
ROLE="$PREFIX-role"
ROLE_ARN="arn:aws:iam::$ACCOUNT:role/$ROLE"
BUCKET="$PREFIX-$ACCOUNT-$REGION"
TABLE="$PREFIX-table"
CLIENT="$PREFIX-client"
# Scenario artifacts built by the benchmark: Node bundles (cd scenarios/node && npm run build)
# and Rust zips (cargo run -p bencher -- build).
NODE_DIST="$REPO/scenarios/node/dist"
RUST_DIST="$REPO/dist"
EVENTS="$HERE/events"

# Invoke the client with a JSON payload; rows land in $OUT/results/<name>.json.
client_run() {
  local name="$1" payload="$2" key="results/$1.json" resp="$OUT/build/$1.resp"
  payload=$(python3 -I -c 'import json,sys; d=json.loads(sys.argv[1]); d["s3Key"]=sys.argv[2]; print(json.dumps(d))' "$payload" "$key")
  aws lambda invoke --function-name "$CLIENT" --cli-read-timeout 0 --cli-binary-format raw-in-base64-out \
    --payload "$payload" "$resp" >/dev/null
  if grep -q errorMessage "$resp"; then cat "$resp" >&2; return 1; fi
  aws s3 cp --quiet "s3://$BUCKET/$key" "$OUT/results/$1.json"
}

# URL saved by web.sh / event.sh for a short name (and optional endpoint).
url_of() { cat "$OUT/$1${2:+-$2}.url"; }
