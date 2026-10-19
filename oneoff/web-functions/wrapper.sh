#!/bin/bash
# Wrapper probe (wrapper-probe/): splits the time before a Web Function's own code runs into Node's
# startup, AWS's wrapper /var/runtime/index.js and its runtime API calls. Needs `resources.sh up`
# (role and bucket). The in-process timings do not depend on where the requests come from, so this
# calls the endpoint with curl from wherever it runs.
#   wrapper.sh create          package wrapper-probe/ and create oneoff-webfn-wrap (preload on)
#   wrapper.sh revise          new revision of the same code: fresh environments
#   wrapper.sh burst [n]       n concurrent 2 s requests (default 40, ~n/2 environments); summary
#   wrapper.sh children        time bare `node` loading wrapper-probe/children/* inside an environment
source "$(dirname "$0")/common.sh"
FN="$PREFIX-wrap"

upload() {
  local pkg="$OUT/build/wrap.zip"
  rm -f "$pkg" && (cd "$HERE/wrapper-probe" && zip -qr "$pkg" .)
  aws s3api put-object --bucket "$BUCKET" --key code/$FN/wrap.zip --body "$pkg" --query VersionId --output text
}
build_config() { echo "{\"codeConfig\":{\"s3Object\":{\"bucket\":\"$BUCKET\",\"key\":\"code/$FN/wrap.zip\",\"versionId\":\"$1\"}},\"runtimeConfig\":{\"runtime\":\"nodejs24.x\"}}"; }
service_config() {
  echo "{\"executionRoleArn\":\"$ROLE_ARN\",\"maxConcurrencyPerEnvironment\":64,
    \"environmentVariables\":{\"AWS_LAMBDA_NODEJS_ENTRYPOINT\":\"index.mjs\",\"NODE_OPTIONS\":\"--import /var/task/pre.mjs\",\"REVISION_NONCE\":\"$(date +%s%N)\"},
    \"telemetryConfig\":{\"loggingConfig\":{\"systemLogLevel\":\"INFO\",\"applicationLogLevel\":\"INFO\"}}}"
}
wait_serving() { # <revisionId>
  while :; do
    local st
    st=$(aws lambda-web get-web-function-endpoint --function-name "$FN" --endpoint-name e0 \
      --query '[state, updateStatus, revisionWeights[0].revisionId]' --output text)
    [[ "$st" == Active* && "$st" != *InProgress* && "$st" == *"$1"* ]] && return
    sleep 3
  done
}

create() {
  local ver out
  ver=$(upload)
  out=$(aws lambda-web create-web-function --function-name "$FN" \
    --revision-config "{\"buildConfig\":$(build_config "$ver"),\"serviceConfig\":$(service_config)}" \
    --endpoint-config '{"endpointName":"e0","endpointType":"HomeRegion","authType":"ApplicationManaged","autoDeploymentMode":"LatestRevision"}')
  echo "https://$(echo "$out" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["endpoint"]["domainName"])')" >"$OUT/wrap.url"
  wait_serving "$(echo "$out" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["revision"]["revisionId"])')"
  echo "$FN $(url_of wrap)"
}

revise() {
  local rid
  rid=$(aws lambda-web create-web-function-revision --function-name "$FN" --build-config "$(build_config "$(upload)")" \
    --service-config "$(service_config)" --query revisionId --output text)
  wait_serving "$rid"
  echo "$rid"
}

burst() {
  local n="${1:-40}" dir="$OUT/build/wrap-burst" u
  u=$(url_of wrap)
  rm -rf "$dir" && mkdir -p "$dir"
  for i in $(seq "$n"); do curl -s -m 30 "$u/?sleep=2000" >"$dir/$i.json" & done
  wait
  python3 -I - "$dir" "$OUT/results/wrapper-burst.json" <<'PY'
import glob, json, statistics, sys
envs = {}
for f in glob.glob(sys.argv[1] + "/*.json"):
    try:
        j = json.load(open(f)); envs[j["bootId"]] = j
    except ValueError:
        print("unparsable response:", open(f).read()[:200])
envs = list(envs.values())
json.dump(envs, open(sys.argv[2], "w"))
# Per label (first occurrence per environment): p50 of wall ms, CPU ms and major page faults.
rows = {}
for e in envs:
    seen = set()
    for label, ms, cpu, majf, minf in e["marks"] or []:
        if label in seen:
            continue
        seen.add(label)
        rows.setdefault(label, []).append((ms, cpu, majf, minf))
    for label, ms in (("node bootstrap complete", e["nodeTiming"]["bootstrapComplete"]), ("listen() done", e["listenMs"]), ("first request", e["firstReqMs"])):
        rows.setdefault(label, []).append((ms, None, None, None))
med = lambda xs: statistics.median(xs) if xs else None
fmt = lambda v: "     -" if v is None else f"{v:6.1f}"
print(f"{len(envs)} environments; p50 of ms since the node process started, CPU ms used so far, major/minor page faults")
for label, v in sorted(rows.items(), key=lambda kv: med([x[0] for x in kv[1]])):
    ms = sorted(x[0] for x in v)
    cpu = med([x[1] for x in v if x[1] is not None]); maj = med([x[2] for x in v if x[2] is not None]); mnr = med([x[3] for x in v if x[3] is not None])
    print(f"  {label[:44]:44s} {fmt(med(ms))} ms (min {ms[0]:6.1f} max {ms[-1]:6.1f})  cpu {fmt(cpu)}  majflt {fmt(maj)}  minflt {fmt(mnr)}  n={len(v)}")
by_cpu = {}
for e in envs:
    m = {}
    for label, ms, *_ in e["marks"] or []:
        m.setdefault(label, ms)
    if "WebSocket getter<" in m:
        by_cpu.setdefault(e.get("cpu", "?"), []).append((m["WebSocket getter<"] - m["WebSocket getter>"], e["firstLineMs"], e["nodeTiming"]["bootstrapComplete"]))
for c, v in sorted(by_cpu.items()):
    print(f"  {c}: {len(v)} environments; p50 bootstrap {statistics.median(x[2] for x in v):.1f} ms, undici {statistics.median(x[0] for x in v):.1f} ms, first line {statistics.median(x[1] for x in v):.1f} ms")
if envs:
    print("wrapper patches seen by the entry module:", envs[0]["patched"])
PY
}

children() {
  curl -s -m 120 "$(url_of wrap)/?children=1" | tee "$OUT/results/wrapper-children.json" | python3 -I -c '
import json, statistics, sys
for f, v in json.load(sys.stdin)["children"].items():
    print(f"  {f:22s} first line at p50 {statistics.median(v):6.1f} ms  {v}")'
}

cmd="${1:-}"; shift || true
case "$cmd" in create | revise | burst | children) "$cmd" "$@" ;; *) sed -n '2,10p' "$0" >&2; exit 2 ;; esac
