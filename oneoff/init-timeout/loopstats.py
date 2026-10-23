# loopstats.py <loop output file> <function name>: when the failures of a `loop.sh par` run started
# and stopped, and, per CloudWatch log stream (one per environment), how many re-inits timed out
# and over what span, so the stuck environment's lifetime shows next to the failure timeline.
import collections, json, subprocess, sys

out, fn = sys.argv[1], sys.argv[2]
rows = [l.split() for l in open(out) if l[0].isdigit()]
t0 = min(int(r[0]) for r in rows)
hang = next(int(r[0]) for r in rows if r[1] == "hang")
fails = [int(r[0]) for r in rows if r[1] != "hang" and r[3] in ("Sandbox.Timedout", "ERR")]
ok = sum(1 for r in rows if r[1] != "hang" and r[3] not in ("Sandbox.Timedout", "ERR"))
print(f"{fn}: hang at {(hang - t0) / 1000:.0f} s, {len(fails)} failures, {ok} successes", end="")
if fails:
    print(f", failures from {(fails[0] - hang) / 60000:.1f} to {(fails[-1] - hang) / 60000:.1f} min after the hang")
else:
    print()

evs, tok = [], None
while True:
    cmd = ["aws", "logs", "filter-log-events", "--log-group-name", f"/aws/lambda/{fn}",
           "--start-time", str(t0), "--filter-pattern", '"Phase: invoke"', "--output", "json"]
    if tok:
        cmd += ["--next-token", tok]
    r = json.loads(subprocess.check_output(cmd))
    evs += r["events"]
    tok = r.get("nextToken")
    if not tok:
        break
streams = collections.defaultdict(list)
for e in evs:
    streams[e["logStreamName"]].append(e["timestamp"])
for st, ts in streams.items():
    print(f"  stream …{st[-8:]}: {len(ts)} timed-out re-inits, {(min(ts) - hang) / 60000:.1f} to {(max(ts) - hang) / 60000:.1f} min after the hang")
