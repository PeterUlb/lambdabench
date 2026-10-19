#!/bin/bash
# The probes behind site/src/web-functions.md. Each writes $OUT/results/<label>.json(l);
# analyze.py summarizes them. <name> is a short name from web.sh / event.sh.
#
#   run.sh cold-web <name> <cycles>              new revision per cycle, then 6 sequential requests
#   run.sh cold-event-url <name> <cycles>         force cold (config update), then 6 GETs via the URL
#   run.sh cold-event-invoke <name> <cycles>      force cold, Invoke with the log tail (Init Duration)
#   run.sh warm <label> <url> <n> [sign]          n sequential warm GETs (sign: SigV4 for IamAuth)
#   run.sh warm-invoke <label> <name> <n> [tail]  n warm Invokes, without the log tail unless `tail`
#   run.sh burst <label> <url> <N> [clients]      N simultaneous GETs, split over parallel clients
#   run.sh load <label> <url> <concurrency> <seconds> [clients]
#                                                 closed-loop load, split over parallel clients
# URLs include the path, e.g. "$(cat .../load.url)/sleep?ms=1000".
source "$(dirname "$0")/common.sh"

payload_for() { # Invoke payload for an Event Function scenario: events/<scenario>.json or {}
  local f="$EVENTS/$1.json"; [ -f "$f" ] && python3 -I -c 'import json,sys; print(json.dumps(json.dumps(json.load(open(sys.argv[1])))))' "$f" || echo '"{}"'
}

cold_web() {
  local name="$1" cycles="$2" out="$OUT/results/cold-web-$1.jsonl" url
  url="$(url_of "$name")/run"; : >"$out"
  for c in $(seq 1 "$cycles"); do
    local rid; rid=$("$HERE/web.sh" revise "$name")
    client_run "tmp-cold-$name" "{\"action\":\"seq\",\"url\":\"$url\",\"n\":6}"
    python3 -I -c 'import json,sys; print(json.dumps({"cycle":int(sys.argv[2]),"rev":sys.argv[3],"reqs":json.load(open(sys.argv[1]))}))' \
      "$OUT/results/tmp-cold-$name.json" "$c" "$rid" >>"$out"
  done
  rm -f "$OUT/results/tmp-cold-$name.json"; echo "wrote $out"
}

parallel_clients() { # <label> <clients> <payload-json>: run the payload on N clients, merge rows
  local label="$1" n="$2" payload="$3" i
  for i in $(seq 1 "$n"); do client_run "$label-part$i" "$payload" & done
  wait
  python3 -I -c 'import json,sys; rows=[r for f in sys.argv[2:] for r in json.load(open(f))]; json.dump(rows, open(sys.argv[1],"w"))' \
    "$OUT/results/$label.json" $(for i in $(seq 1 "$n"); do echo "$OUT/results/$label-part$i.json"; done)
  for i in $(seq 1 "$n"); do rm -f "$OUT/results/$label-part$i.json"; done
  echo "wrote $OUT/results/$label.json"
}

cmd="${1:-}"; shift || true
case "$cmd" in
  cold-web) cold_web "$@" ;;
  cold-event-url)
    client_run "cold-event-url-$1" "{\"action\":\"event-url\",\"fn\":\"$PREFIX-$1\",\"url\":\"$(url_of "$1")/${3:-}\",\"cycles\":$2}" ;;
  cold-event-invoke)
    client_run "cold-event-invoke-$1" "{\"action\":\"event-invoke\",\"fn\":\"$PREFIX-$1\",\"cycles\":$2,\"warm\":3,\"payload\":$(payload_for "${3:-none}")}" ;;
  warm)
    client_run "warm-$1" "{\"action\":\"seq\",\"url\":\"$2\",\"n\":$3,\"sign\":$([ "${4:-}" = sign ] && echo true || echo false)}" ;;
  warm-invoke)
    client_run "warm-$1" "{\"action\":\"event-invoke\",\"fn\":\"$PREFIX-$2\",\"cycles\":1,\"warm\":$3,\"tail\":$([ "${4:-}" = tail ] && echo true || echo false),\"payload\":$(payload_for "${5:-none}")}" ;;
  burst)
    n="${4:-1}"; parallel_clients "burst-$1" "$n" "{\"action\":\"burst\",\"url\":\"$2\",\"concurrency\":$(($3 / n))}" ;;
  load)
    n="${5:-1}"; parallel_clients "load-$1" "$n" "{\"action\":\"load\",\"url\":\"$2\",\"concurrency\":$(($3 / n)),\"durationMs\":$(($4 * 1000))}" ;;
  *) sed -n '2,14p' "$0" >&2; exit 2 ;;
esac
