#!/bin/bash
# Usage: OUT=<dir> run.sh <arch> <n>. Forces <n> cold starts of
# oneoff-sdk-tuning-flags-<arch> (a new env value per invoke) and appends each
# result to <dir>/res-<arch>.jsonl. An invoke that errors or reports it was not
# a cold start (it reached the old sandbox) is skipped and logged on stderr, so
# every kept line is one cold start. AWS_REGION picks the region (default eu-central-1).
set -u
a=$1; n=$2; F=oneoff-sdk-tuning-flags-$a; D=${OUT:-.}; R=${AWS_REGION:-eu-central-1}
mkdir -p "$D"
aws lambda wait function-active-v2 --region "$R" --function-name "$F" || exit 1
for i in $(seq 1 "$n"); do
  aws lambda update-function-configuration --region "$R" --function-name "$F" --environment "Variables={NONCE=$a$i$RANDOM}" >/dev/null || continue
  aws lambda wait function-updated-v2 --region "$R" --function-name "$F" || continue
  o="$D/o.$a.$i.json"
  err=$(aws lambda invoke --region "$R" --function-name "$F" --cli-read-timeout 90 --query FunctionError --output text "$o") || continue
  if [ "$err" != "None" ]; then echo "skip $a $i: function error $err" >&2; continue; fi
  if ! grep -q '"cold": true' "$o"; then echo "skip $a $i: not a cold start" >&2; continue; fi
  { cat "$o"; echo; } >>"$D/res-$a.jsonl"
done
