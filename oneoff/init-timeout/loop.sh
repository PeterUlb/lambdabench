#!/bin/bash
# How long a doom loop lasts. Needs a function from `run.sh fn <name> 100 3 14000` (a re-init that
# cannot fit its 3 s timeout's 13 s cutoff).
#   loop.sh seq <name> <minutes> [gapS]   one hang, then one invoke every gapS (default 30) s
#   loop.sh par <name> <minutes> <callers>   callers invoke back to back; one hang after 60 s
# One line per invoke: unix ms, caller, wall ms, initId or the error type.
set -euo pipefail
export AWS_REGION="${AWS_REGION:-eu-central-1}"
F="oneoff-inittimeout-$2"
OUT="$(cd "$(dirname "$0")/../.." && pwd)/.tmp/oneoff-init-timeout"
mkdir -p "$OUT"

one() { # <caller> [payload]; the file name carries $F so parallel runs stay apart
  local t0 t1 r="$OUT/loop-$F-$1.json"
  t0=$(date +%s%3N)
  aws lambda invoke --function-name "$F" --cli-read-timeout 0 --cli-binary-format raw-in-base64-out \
    --payload "${2:-{\}}" "$r" >/dev/null 2>&1 || true
  t1=$(date +%s%3N)
  echo "$t0 $1 $((t1 - t0)) $(python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d.get("initId") or d.get("errorType"))' "$r" 2>/dev/null || echo ERR)"
}

end=$(( $(date +%s) + $3 * 60 ))
case "$1" in
  seq)
    one 0 >/dev/null; one 0 '{"hang":true}'
    while [ "$(date +%s)" -lt "$end" ]; do one 0; sleep "${4:-30}"; done ;;
  par)
    for c in $(seq 1 "$4"); do (while [ "$(date +%s)" -lt "$end" ]; do one "$c"; done) & done
    sleep 60; one hang '{"hang":true}'
    wait ;;
esac
