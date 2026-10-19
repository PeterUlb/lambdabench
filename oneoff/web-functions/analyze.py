#!/usr/bin/env python3
"""Summaries of the probe results in .tmp/oneoff-web-functions/results and of the Web Functions'
platform logs. Run with python3 -I.

  analyze.py cold-web <name>             cold vs warm waits + process-relative timestamps
  analyze.py cold-event-url <name>       Event Function cold vs warm waits through the Function URL
  analyze.py cold-event-invoke <name>    Init Duration + caller wall for forced cold Invokes
  analyze.py warm <label> [...]          warm percentiles (seq or warm-invoke results)
  analyze.py burst <label>               environments, requests per environment, latency
  analyze.py load <label> [windowSec]    per-window environments, in-flight per environment, latency
  analyze.py init <name> [minutes]       platform.initReport duration/CPU/memory percentiles
  analyze.py lifetimes <name> [minutes]  idle time before spin-down, unused environments
  analyze.py cpu <name> [minutes]        CPU per request and idle CPU from platform.invokeSummary
  analyze.py cloudfront                  client waits vs CloudFront time-to-first-byte (cloudfront.sh)
"""
import collections
import json
import os
import subprocess
import sys
import time
from datetime import datetime

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
OUT = os.path.join(REPO, ".tmp", "oneoff-web-functions", "results")
PREFIX = "oneoff-webfn"
REGION = os.environ.get("AWS_REGION", "eu-central-1")


def q(a, p):
    a = sorted(a)
    return a[min(len(a) - 1, int(p * len(a)))] if a else float("nan")


def dist(a):
    return f"p10 {q(a, .1):.0f}  p50 {q(a, .5):.0f}  p90 {q(a, .9):.0f}  max {max(a):.0f} ms (n={len(a)})"


def load_json(name):
    return json.load(open(os.path.join(OUT, name)))


def cold_web(name):
    rows = [json.loads(l) for l in open(os.path.join(OUT, f"cold-web-{name}.jsonl"))]
    cold = [r["reqs"][0] for r in rows if r["reqs"][0]["status"] == 200 and r["reqs"][0]["reqN"] == 1]
    warm = [x for r in rows for x in r["reqs"][1:] if x["status"] == 200]
    print(f"cold-web {name}: {len(cold)}/{len(rows)} cycles confirmed cold")
    print(f"  cold wait   {dist([c['wait'] for c in cold])}")
    print(f"  warm wait   p50 {q([w['wait'] for w in warm], .5):.1f} ms")
    for k, lab in [("moduleStart", "module first line"), ("listen", "listen() done"), ("firstReq", "first request")]:
        print(f"  {lab:18s} p50 {q([c[k] for c in cold], .5):.0f} ms after the node process started")
    pre = [c["wait"] - c["uptime"] for c in cold]
    print(f"  before the process existed (cold wait - process uptime at response) p50 {q(pre, .5):.0f} ms")


def cold_event_url(name):
    rows = load_json(f"cold-event-url-{name}.json")
    cold = [c["reqs"][0] for c in rows if c["reqs"][0]["status"] == 200]
    confirmed = sum(1 for c in cold if c["reqN"] in (1, -1))
    warm = [r for c in rows for r in c["reqs"][1:] if r["status"] == 200]
    print(f"cold-event-url {name}: {len(cold)} cycles ({confirmed} confirmed or unconfirmable cold)")
    print(f"  cold wait   {dist([c['wait'] for c in cold])}")
    print(f"  warm wait   p50 {q([w['wait'] for w in warm], .5):.1f} ms")


def cold_event_invoke(name):
    rows = load_json(f"cold-event-invoke-{name}.json")
    c = [r for r in rows if r["cold"] and not r["fnErr"]]
    print(f"cold-event-invoke {name}: Init Duration p50 {q([r['init'] or 0 for r in c], .5):.0f} ms, "
          f"Init+first Duration p50 {q([(r['init'] or 0) + r['dur'] for r in c], .5):.0f} ms")
    print(f"  cold wall   {dist([r['wall'] for r in c])}")


def warm(*labels):
    for label in labels:
        d = load_json(f"warm-{label}.json")
        if d and "wall" in d[0]:
            v = [r["wall"] for r in d if not r["cold"]]
        else:
            v = [r["total"] for r in d[1:] if r["status"] == 200]
        print(f"warm {label:24s} p50 {q(v, .5):5.1f}  p90 {q(v, .9):5.1f}  p99 {q(v, .99):5.1f} ms (n={len(v)})")


def burst(label):
    d = load_json(f"burst-{label}.json")
    ok = [r for r in d if r["status"] == 200]
    envs = collections.Counter(r["boot"] for r in ok)
    per = collections.Counter(envs.values())
    print(f"burst {label}: N={len(d)} status={dict(collections.Counter(r['status'] for r in d))} "
          f"errTypes={dict(collections.Counter(r['errType'] for r in d if r.get('errType')))}")
    print(f"  environments {len(envs)}, requests per environment {dict(sorted(per.items()))}, "
          f"cold (reqN 1) {sum(1 for r in ok if r['reqN'] == 1)}")
    print(f"  latency {dist([r['total'] for r in ok])}")


def load(label, win="15"):
    d = load_json(f"load-{label}.json")
    win = float(win) * 1000
    print(f"load {label}: {len(d)} requests, status {dict(collections.Counter(r['status'] for r in d))}")
    seen = set()
    for w in range(int(max(r["t0"] for r in d) // win) + 1):
        rs = [r for r in d if w * win <= r["t0"] < (w + 1) * win and r["status"] == 200]
        if not rs:
            continue
        envs = collections.defaultdict(list)
        for r in rs:
            envs[r["boot"]].append(r)
        new = len(set(envs) - seen)
        seen |= set(envs)
        mi = [max(x["inflight"] for x in v) for v in envs.values()]
        lat = [r["total"] for r in rs]
        print(f"  t={w * win / 1000:4.0f}s rps={len(rs) / (win / 1000):6.0f} envs={len(envs):3d} new={new:3d} "
              f"in-flight/env p50={q(mi, .5)} max={max(mi)}  latency p50={q(lat, .5):.0f} p99={q(lat, .99):.0f} ms")
    med = q([x["total"] for x in d if x["status"] == 200], .5)
    slow = sum(1 for r in d if r["status"] == 200 and r["total"] > med + 250)
    print(f"  distinct environments {len(seen)}; requests >250 ms above the median: {slow}")


def logs(name, minutes, pattern):
    start = int((time.time() - float(minutes) * 60) * 1000)
    evs, tok = [], None
    while True:
        cmd = ["aws", "logs", "filter-log-events", "--region", REGION, "--log-group-name",
               f"/aws/lambda/web/{PREFIX}-{name}.local", "--start-time", str(start),
               "--filter-pattern", pattern, "--output", "json"]
        if tok:
            cmd += ["--next-token", tok]
        r = json.loads(subprocess.check_output(cmd))
        evs += [json.loads(e["message"]) for e in r["events"]]
        tok = r.get("nextToken")
        if not tok:
            return evs


def init(name, minutes="180"):
    ok = [e["record"]["metrics"] for e in logs(name, minutes, '"platform.initReport"') if e["record"]["status"] == "success"]
    print(f"init {name}: n={len(ok)} duration {dist([m['totalDurationMs'] for m in ok])}")
    print(f"  CPU p50 {q([m['totalCpuTimeMs'] for m in ok], .5):.0f} ms, memory p50 {q([m['maxMemoryUsageMb'] for m in ok], .5)} MB")


def lifetimes(name, minutes="180"):
    ts = lambda s: datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()
    env = collections.defaultdict(dict)
    for e in logs(name, minutes, '?"platform.initStart" ?"platform.spinDownStart" ?"platform.invokeSummary"'):
        rec = e["record"]
        d = env[rec["instanceId"]]
        if e["type"] == "platform.initStart":
            d["init"] = ts(e["timestamp"])
            d["rev"] = rec.get("revisionId")
        elif e["type"] == "platform.spinDownStart":
            d["down"] = ts(e["timestamp"])
        elif rec["metrics"]["requests"] > 0:
            # last summary window that had requests: its end bounds the last request
            d["lastBusy"] = ts(e["timestamp"])
            d["reqs"] = d.get("reqs", 0) + rec["metrics"]["requests"]
    done = [d for d in env.values() if "down" in d and "init" in d]
    unused = [d for d in done if not d.get("reqs")]
    idle = [d["down"] - d["lastBusy"] for d in done if "lastBusy" in d]
    print(f"lifetimes {name}: {len(env)} environments, {len(done)} spun down, {len(unused)} never served a request")
    if idle:
        print(f"  spin-down after the last busy ~60 s window: p10 {q(idle, .1):.0f} p50 {q(idle, .5):.0f} p90 {q(idle, .9):.0f} s")
    if unused:
        life = [d["down"] - d["init"] for d in unused]
        print(f"  unused environments lived p10 {q(life, .1):.0f} p50 {q(life, .5):.0f} p90 {q(life, .9):.0f} s")
    revs = collections.defaultdict(list)
    for d in done:
        revs[d.get("rev")].append(d)
    if len(revs) > 1:
        for rev, ds in sorted(revs.items(), key=lambda kv: min(d["init"] for d in kv[1])):
            print(f"  {rev}: {len(ds)} environments, {sum(1 for d in ds if not d.get('reqs'))} never served a request")


def cpu(name, minutes="60"):
    ev = [e["record"]["metrics"] for e in logs(name, minutes, '"platform.invokeSummary"')]
    busy = [m for m in ev if m["requests"] > 1000]
    idle = [m["cpuTimeMs"] for m in ev if m["requests"] == 0]
    if busy:
        print(f"cpu {name}: {sum(m['cpuTimeMs'] for m in busy) / sum(m['requests'] for m in busy):.2f} ms CPU per request "
              f"over {len(busy)} busy windows; max memory {max(m['maxMemoryUsageMb'] for m in ev)} MB")
    if idle:
        print(f"  idle windows: CPU p50 {q(idle, .5):.0f} ms per ~60 s (n={len(idle)})")


def cloudfront():
    lines = [json.loads(m) for m in load_json("cf-logs.json")]
    ms = lambda ls, k: [float(l[k]) * 1000 for l in ls]
    pct = lambda v: f"p50 {q(v, .5):5.1f}  p90 {q(v, .9):5.1f}  p99 {q(v, .99):5.1f} ms (n={len(v)})"
    w = [l for l in lines if l["cs-uri-query"] == "w=1"]
    for label in ("cf", "direct"):
        print(f"{'client wait, warm, ' + label:36s} {pct([r['wait'] for r in load_json(f'cf-warm-{label}.json') if r['status'] == 200])}")
    print(f"{'CloudFront time-to-first-byte, warm':36s} {pct(ms(w, 'time-to-first-byte'))}")
    print(f"{'CloudFront origin-fbl, warm':36s} {pct(ms(w, 'origin-fbl'))}")
    # The cold request is the slowest of its cycle on both sides, so pair it with the cycle's slowest log line.
    print("cold: client wait / CloudFront time-to-first-byte / origin-fbl (ms)")
    for row in map(json.loads, open(os.path.join(OUT, "cf-cold.jsonl"))):
        cold = [r for r in row["reqs"] if r["reqN"] == 1]
        ls = [l for l in lines if l["cs-uri-query"] == f"c={row['cycle']}"]
        if cold and ls:
            top = max(ls, key=lambda l: float(l["time-to-first-byte"]))
            print(f"  cycle {row['cycle']:2d}  {cold[0]['wait']:4.0f} / {float(top['time-to-first-byte']) * 1000:4.0f}"
                  f" / {float(top['origin-fbl']) * 1000:4.0f}  {cold[0]['cpu']}")


CMDS = {"cold-web": cold_web, "cold-event-url": cold_event_url, "cold-event-invoke": cold_event_invoke,
        "warm": warm, "burst": burst, "load": load, "init": init, "lifetimes": lifetimes, "cpu": cpu,
        "cloudfront": cloudfront}

if __name__ == "__main__":
    if len(sys.argv) < 2 or sys.argv[1] not in CMDS:
        sys.exit(__doc__)
    CMDS[sys.argv[1]](*sys.argv[2:])
