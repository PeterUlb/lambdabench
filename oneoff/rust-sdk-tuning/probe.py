#!/usr/bin/env python3
"""Deploy, measure and delete the rust-sdk-tuning probe functions.

Usage (from the repo root; needs the aws CLI, run resources.sh up and build.sh first):
  python3 -I oneoff/rust-sdk-tuning/probe.py deploy  CONFIGS
  python3 -I oneoff/rust-sdk-tuning/probe.py measure CONFIGS [ROUNDS] [WARM]
  python3 -I oneoff/rust-sdk-tuning/probe.py summary RESULTS.jsonl [RESULTS.jsonl ...]
  python3 -I oneoff/rust-sdk-tuning/probe.py delete  CONFIGS

CONFIGS is a TSV file, one function per line: name, variant (a build.sh zip),
arch, memory MB, and space-separated KEY=VALUE environment variables, where
`@payload=FILE` (relative to this directory) sets the invoke payload instead and
`@runtime=` / `@handler=` deploy a managed runtime (default provided.al2023, bootstrap).
Lines starting with # are skipped. Function names get the `oneoff-sdk-tuning-` prefix.

Each measure round forces a cold start on every function (an environment
variable change puts the next invoke on a fresh execution environment), invokes
it once, then WARM more times, all functions in parallel. Every invoke appends
one JSON line to .tmp/oneoff-rust-sdk-tuning/results-<timestamp>-<region>.jsonl.
AWS_REGION picks the region (default eu-central-1); resources.sh must have run there.
"""

import base64
import concurrent.futures
import json
import os
import re
import statistics
import subprocess
import sys
import tempfile
import time
import uuid

PREFIX = "oneoff-sdk-tuning-"
REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
OUT = os.path.join(REPO, ".tmp", "oneoff-rust-sdk-tuning")
REGION = os.environ.get("AWS_REGION", "eu-central-1")
PARALLEL = 8


def aws(*args, check=True):
    cmd = ["aws", "--region", REGION, "--output", "json", *args]
    for attempt in range(6):
        p = subprocess.run(cmd, capture_output=True, text=True)
        if p.returncode == 0:
            return json.loads(p.stdout) if p.stdout.strip() else None
        # Expired credentials fail every later call too: stop the whole run.
        if re.search(r"session has expired|ExpiredToken|reauthenticate", p.stderr):
            print(f"aws credentials expired: {p.stderr.strip()}", file=sys.stderr)
            os._exit(2)
        # Control-plane throttles and the update-in-progress race are transient.
        if re.search(r"TooManyRequests|Throttl|ResourceConflict|Rate exceeded", p.stderr):
            time.sleep(2 + attempt * 2)
            continue
        if check:
            raise RuntimeError(f"{' '.join(args[:2])}: {p.stderr.strip()}")
        return None
    raise RuntimeError(f"{' '.join(args[:2])}: still throttled")


def load_configs(path):
    configs = []
    for line in open(path):
        line = line.rstrip("\n")
        if not line.strip() or line.startswith("#"):
            continue
        name, variant, arch, memory, env = (line.split("\t") + [""])[:5]
        configs.append(
            {
                "name": name,
                "fn": PREFIX + name,
                "variant": variant,
                "arch": arch,
                "memory": int(memory),
                "env": dict(kv.split("=", 1) for kv in env.split()),
            }
        )
        # `@payload=FILE` is the invoke payload, not a function variable.
        payload = configs[-1]["env"].pop("@payload", None)
        configs[-1]["payload"] = os.path.join(os.path.dirname(__file__), payload) if payload else None
        configs[-1]["runtime"] = configs[-1]["env"].pop("@runtime", "provided.al2023")
        configs[-1]["handler"] = configs[-1]["env"].pop("@handler", "bootstrap")
    return configs


def resources():
    env = dict(
        line.strip().split("=", 1)
        for line in open(os.path.join(OUT, f"resources-{REGION}.env"))
        if "=" in line
    )
    # The same resources under the names the matrix's smithyfull handler reads,
    # so a smithyfull build can run as a probe variant unchanged.
    for probe, matrix in (("PROBE_TABLE", "LAMBDABENCH_TABLE"),
                          ("PROBE_KMS_KEY_ID", "LAMBDABENCH_KMS_KEY_ID"),
                          ("PROBE_BUCKET", "LAMBDABENCH_BUCKET"),
                          ("PROBE_ORDER_PK", "LAMBDABENCH_ORDER_PK"),
                          ("PROBE_RECEIPT_KEY", "LAMBDABENCH_RECEIPT_KEY")):
        env[matrix] = env[probe]
    return env.pop("ROLE_ARN"), env


def env_arg(env):
    return json.dumps({"Variables": env})


def deploy_one(c, role, base_env):
    zip_path = os.path.join(OUT, f"{c['variant']}-{c['arch']}.zip")
    env = {**base_env, **c["env"]}
    exists = aws("lambda", "get-function", "--function-name", c["fn"], check=False)
    if exists is None:
        aws(
            "lambda", "create-function", "--function-name", c["fn"],
            "--runtime", c["runtime"], "--handler", c["handler"],
            "--architectures", c["arch"], "--memory-size", str(c["memory"]),
            "--timeout", "30", "--role", role, "--zip-file", f"fileb://{zip_path}",
            "--environment", env_arg(env),
        )
    else:
        # Redeploying under the same name may change the arch, runtime or handler
        # too, so set all of them, not just code, memory and environment.
        aws("lambda", "update-function-code", "--function-name", c["fn"],
            "--zip-file", f"fileb://{zip_path}", "--architectures", c["arch"])
        aws("lambda", "wait", "function-updated-v2", "--function-name", c["fn"])
        aws("lambda", "update-function-configuration", "--function-name", c["fn"],
            "--runtime", c["runtime"], "--handler", c["handler"],
            "--memory-size", str(c["memory"]), "--environment", env_arg(env))
    aws("lambda", "wait", "function-updated-v2", "--function-name", c["fn"])
    return c["name"]


def invoke(fn, payload=None):
    extra = ["--payload", f"fileb://{payload}"] if payload else []
    with tempfile.NamedTemporaryFile(dir=OUT, suffix=".json") as body_file:
        out = aws("lambda", "invoke", "--function-name", fn, "--log-type", "Tail",
                  *extra, body_file.name)
        body = open(body_file.name).read()
    log = base64.b64decode(out.get("LogResult", "")).decode(errors="replace")
    report = {}
    for key, field in (("init_ms", "Init Duration"), ("duration_ms", "Duration"),
                       ("max_mem_mb", "Max Memory Used")):
        m = re.search(rf"\t{field}: ([0-9.]+)", log)
        report[key] = float(m.group(1)) if m else None
    try:
        parsed = json.loads(body)
    except ValueError:
        parsed = {"raw": body}
    # A handler can also print one `PROBE {json}` line to stderr when its
    # response cannot carry extra fields (the smithyfull copies do).
    m = re.search(r"PROBE (\{.*\})", log)
    if m:
        report["probe"] = json.loads(m.group(1))
    return report, parsed, out.get("FunctionError")


def cycle(c, round_no, warm, base_env, sink):
    env = {**base_env, **c["env"], "PROBE_NONCE": f"{round_no}-{uuid.uuid4().hex[:8]}"}
    aws("lambda", "update-function-configuration", "--function-name", c["fn"],
        "--environment", env_arg(env))
    aws("lambda", "wait", "function-updated-v2", "--function-name", c["fn"])
    rows = []
    for i in range(1 + warm):
        report, body, error = invoke(c["fn"], c["payload"])
        rows.append({"name": c["name"], "region": REGION, "variant": c["variant"], "arch": c["arch"],
                     "memory": c["memory"], "env": c["env"], "round": round_no,
                     "idx": i, "cold": i == 0, "error": error, **report, "body": body})
    with open(sink, "a") as f:
        for row in rows:
            f.write(json.dumps(row) + "\n")
    return c["name"], rows[0]


def cmd_deploy(path):
    role, base_env = resources()
    configs = load_configs(path)
    with concurrent.futures.ThreadPoolExecutor(PARALLEL) as pool:
        for name in pool.map(lambda c: deploy_one(c, role, base_env), configs):
            print("deployed", name)


def cmd_measure(path, rounds=15, warm=5):
    _, base_env = resources()
    configs = load_configs(path)
    sink = os.path.join(OUT, f"results-{time.strftime('%Y%m%d-%H%M%S')}-{REGION}.jsonl")
    print("results ->", sink)
    for r in range(int(rounds)):
        t = time.time()
        failed = 0
        with concurrent.futures.ThreadPoolExecutor(PARALLEL) as pool:
            futures = [pool.submit(cycle, c, r, int(warm), base_env, sink) for c in configs]
            for fut in concurrent.futures.as_completed(futures):
                try:
                    name, first = fut.result()
                    if first["error"] or first["init_ms"] is None:
                        failed += 1
                        print(f"  {name}: cold invoke not clean: {first}", file=sys.stderr)
                except Exception as e:  # one bad cycle should not abort a long run
                    failed += 1
                    print(f"  cycle failed: {e}", file=sys.stderr)
        print(f"round {r + 1}/{rounds} done in {time.time() - t:.0f}s ({failed} not clean)")
    cmd_summary(sink)


def median(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else float("nan")


def cmd_summary(*paths):
    groups = {}
    for path in paths:
        for line in open(path):
            row = json.loads(line)
            if row["error"]:
                continue
            groups.setdefault(row["name"], []).append(row)
    print(f"{'config':34} {'n':>3} {'init':>7} {'1st':>7} {'cold':>7} {'p10':>7} {'p90':>7}"
          f" {'warm':>6} | {'load':>6} {'clnt':>5} {'prewm':>6} {'hndl1':>6}")
    for name, rows in groups.items():
        cold = [r for r in rows if r["cold"] and r["init_ms"] is not None]
        # A later invoke that reports an Init Duration landed on a recycled
        # sandbox: it is neither the round's cold start nor a warm sample.
        warm = [r for r in rows if not r["cold"] and r.get("init_ms") is None]
        totals = sorted(r["init_ms"] + r["duration_ms"] for r in cold)
        if not totals:
            continue
        body = lambda k: median([r["body"].get(k) if isinstance(r["body"], dict) else None
                                 for r in cold])
        print(
            f"{name:34} {len(cold):>3} {median([r['init_ms'] for r in cold]):7.1f}"
            f" {median([r['duration_ms'] for r in cold]):7.1f} {median(totals):7.1f}"
            f" {totals[len(totals) // 10]:7.1f} {totals[(len(totals) * 9) // 10]:7.1f}"
            f" {median([r['duration_ms'] for r in warm]):6.1f} |"
            f" {body('load_ms'):6.1f} {body('clients_ms'):5.1f} {body('prewarm_ms'):6.1f}"
            f" {body('handler_ms'):6.1f}"
        )


def cmd_delete(path):
    for c in load_configs(path):
        aws("lambda", "delete-function", "--function-name", c["fn"], check=False)
        aws("logs", "delete-log-group", "--log-group-name", f"/aws/lambda/{c['fn']}",
            check=False)
        print("deleted", c["name"])


if __name__ == "__main__":
    commands = {"deploy": cmd_deploy, "measure": cmd_measure, "summary": cmd_summary,
                "delete": cmd_delete}
    if len(sys.argv) < 3 or sys.argv[1] not in commands:
        sys.exit(__doc__)
    commands[sys.argv[1]](*sys.argv[2:])
