#!/usr/bin/env python3
"""Guard the hand-written ORDERING claims in the findings prose against the
freshly built stats.json.

check-lifecycle-prose.py guards lifecycle.md's numeric claims against the probe
JSONs. This is its sibling for the OTHER drift surface: every number on the
rendered pages self-updates from stats.json, but the *orderings* the prose
asserts (which series wins where) are hand-written and a re-run can flip an
edge. Guarded claims and where their prose lives:

  - the SnapStart win/lose split per scenario   (site/src/java-snapstart.md
    "When SnapStart wins, and when it loses"; README "Finding: SnapStart
    priming is decisive ...")
  - Go's `batch` warm median trails Rust's      (README "Finding: Go's slower
    `batch` warm time is encoding/json, not GC")
  - `oneclient` init-vs-total ordering flip     (README "Finding: cold start =
    for Go vs Rust                               init + first request";
                                                 site/src/lifecycle.md)
  - the opt-level directions                    (site/src/rust.md "Opt-level:
    (oz usually cold-wins, o3 warm-wins on CPU)  speed vs size")
  - the jitter tax phase split + oneclient      (site/src/rust.md; README
    cliff                                        jitter-entropy Finding)
  - the non-GC runtime holds the lowest cache   (site/src/comparison.md "Warm
    warm tail at every tier                      tail vs median")
  - the smithy-java version quoted in prose     (README + java-snapstart.md
    matches scenarios/java/gradle.properties     "used in this run (X)")

Reads the stats.json the site build just produced (the Framework data-loader
cache), so it must run AFTER `npm run build`. Tolerances are deliberately
generous: categorical prose ("wins", "loses") is asserted at >= 75% of shared
cells, hedged prose ("usually", "bar a stray near-tie tier") at > 50%, so
ordinary run noise passes and only a flipped finding trips. On drift it exits
non-zero and names the claim + the prose to revisit; wired into the publish
pipeline as a NON-FATAL warning, like the lifecycle check.

Run: python3 scripts/check-findings-prose.py
Override the stats.json path with LAMBDABENCH_STATS_JSON.
"""

import json
import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
STATS_JSON = os.environ.get(
    "LAMBDABENCH_STATS_JSON",
    os.path.join(REPO, "site", "src", ".observablehq", "cache", "data", "stats.json"),
)

# Categorical prose ("wins", "loses at every tier") vs hedged prose ("usually",
# "bar a stray near-tie tier"): the fraction of shared cells that must agree.
CATEGORICAL = 0.75
HEDGED = 0.50

failures = []
notes = []


def check(name, ok, detail, prose):
    """Record one claim's outcome. `prose` names what the finding says, so a
    failure points straight at the sentence to update."""
    status = "ok " if ok else "DRIFT"
    notes.append(f"  [{status}] {name}: {detail}")
    if not ok:
        failures.append(f"{name}: {detail}\n           prose to revisit: {prose}")


if not os.path.exists(STATS_JSON):
    print(
        f"no built stats.json at {STATS_JSON} (run `npm run build` in site/ first, "
        "or set LAMBDABENCH_STATS_JSON)",
        file=sys.stderr,
    )
    sys.exit(2)

with open(STATS_JSON) as f:
    stats = json.load(f)

cells = stats.get("cells", [])


def frac(pairs, pred):
    """Fraction of pairs satisfying pred, with the count for the detail line.
    Returns (fraction, n); fraction is None when there is nothing to compare."""
    if not pairs:
        return None, 0
    hits = sum(1 for p in pairs if pred(p))
    return hits / len(pairs), len(pairs)


def fmt(fr, n):
    return "no shared cells" if fr is None else f"{fr:.0%} of {n} shared cells"


# ---- Claim 1: the SnapStart win/lose split per scenario ---------------------
# java-snapstart.md: SnapStart "wins" on oneclient/threeclient/lettercount/batch
# and "loses" on hello/cache/smithy/smithyfull, with authz behind "(bar a stray
# near-tie tier)". Compared on total cold P50 over the shared
# (arch, memory) cells of each scenario.
snap_pairs = {}
for c in stats.get("snapCells", []):
    if c["lang"] != "java":
        continue
    key = (c["arch"], c["scenario"], c["memory_mb"])
    snap_pairs.setdefault(key, {})[bool(c["snapstart"])] = c["coldP50"]


def snap_scenario_pairs(scenario):
    return [
        v
        for (arch, s, mem), v in snap_pairs.items()
        if s == scenario and True in v and False in v
    ]


for scenario in ("oneclient", "threeclient", "lettercount", "batch"):
    fr, n = frac(snap_scenario_pairs(scenario), lambda v: v[True] < v[False])
    check(
        f"snapstart wins on {scenario}",
        fr is not None and fr >= CATEGORICAL,
        f"snapstart faster in {fmt(fr, n)}",
        "java-snapstart.md 'When SnapStart wins' + README priming Finding",
    )
for scenario in ("hello", "cache", "smithy", "smithyfull"):
    fr, n = frac(snap_scenario_pairs(scenario), lambda v: v[False] < v[True])
    check(
        f"snapstart loses on {scenario}",
        fr is not None and fr >= CATEGORICAL,
        f"plain faster in {fmt(fr, n)}",
        "java-snapstart.md 'When SnapStart wins' + README priming Finding",
    )
fr, n = frac(snap_scenario_pairs("authz"), lambda v: v[False] < v[True])
check(
    "snapstart loses on authz (near-ties allowed)",
    fr is not None and fr > HEDGED,
    f"plain faster in {fmt(fr, n)}",
    "java-snapstart.md '(bar a stray near-tie tier) authz'",
)


# ---- Claim 2: Go's batch warm median trails Rust's --------------------------
# README: "Go's slower `batch` warm time is `encoding/json`" presumes Go's warm
# median IS slower than Rust's. `cells` carries the representative Rust build
# (o3, jitter off), matching what the prose compares.
def lang_cells(lang, scenario):
    return {
        (c["arch"], c["memory_mb"]): c
        for c in cells
        if c["lang"] == lang and c["scenario"] == scenario
    }


def shared_p50s(lang_a, lang_b, scenario, family):
    a, b = lang_cells(lang_a, scenario), lang_cells(lang_b, scenario)
    out = []
    for key in a.keys() & b.keys():
        pa, pb = (a[key].get(family) or {}), (b[key].get(family) or {})
        if pa.get("p50") is not None and pb.get("p50") is not None:
            out.append((pa["p50"], pb["p50"]))
    return out


fr, n = frac(shared_p50s("go", "rust", "batch", "warm"), lambda p: p[0] > p[1])
check(
    "go batch warm median slower than rust",
    fr is not None and fr >= CATEGORICAL,
    f"go slower in {fmt(fr, n)}",
    "README 'Finding: Go's slower batch warm time is encoding/json, not GC'",
)

# ---- Claim 3: the oneclient init-vs-total flip (Go vs Rust) -----------------
# README: "Go's init is *lower*, but ... read by total cold start the ordering
# flips and Rust comes out ahead."
fr, n = frac(shared_p50s("go", "rust", "oneclient", "coldInit"), lambda p: p[0] < p[1])
check(
    "go oneclient init lower than rust",
    fr is not None and fr >= CATEGORICAL,
    f"go init lower in {fmt(fr, n)}",
    "README 'Finding: cold start = init + first request' (Go vs Rust on oneclient)",
)
fr, n = frac(shared_p50s("rust", "go", "oneclient", "cold"), lambda p: p[0] < p[1])
check(
    "rust oneclient cold total lower than go",
    fr is not None and fr >= CATEGORICAL,
    f"rust total lower in {fmt(fr, n)}",
    "README 'Finding: cold start = init + first request' (Go vs Rust on oneclient)",
)

# ---- Claim 4: the opt-level directions --------------------------------------
# rust.md: "oz's usually-smaller binary tends to give a lower cold init"
# (hedged) and o3 is faster warm on the CPU-bound scenarios (categorical for
# lettercount/batch: "large for the CPU-bound scenarios ... o3 can be markedly
# faster").
opt_pairs = {}
for c in stats.get("optCells", []):
    key = (c["arch"], c["scenario"], c["memory_mb"])
    opt_pairs.setdefault(key, {})[c["opt"]] = c

opt_shared = [v for v in opt_pairs.values() if "o3" in v and "oz" in v]
fr, n = frac(
    [
        v
        for v in opt_shared
        if v["oz"]["coldInitP50"] is not None and v["o3"]["coldInitP50"] is not None
    ],
    lambda v: v["oz"]["coldInitP50"] < v["o3"]["coldInitP50"],
)
check(
    "oz usually cold-init-faster than o3",
    fr is not None and fr > HEDGED,
    f"oz lower cold init in {fmt(fr, n)}",
    "rust.md 'oz's usually-smaller binary tends to give a lower cold init'",
)
cpu_bound = [
    v
    for (arch, s, mem), v in opt_pairs.items()
    if s in ("lettercount", "batch")
    and "o3" in v
    and "oz" in v
    and v["o3"]["warmP50"] is not None
    and v["oz"]["warmP50"] is not None
]
fr, n = frac(cpu_bound, lambda v: v["o3"]["warmP50"] < v["oz"]["warmP50"])
check(
    "o3 warm-faster than oz on CPU-bound scenarios",
    fr is not None and fr >= CATEGORICAL,
    f"o3 faster warm in {fmt(fr, n)} (lettercount+batch)",
    "rust.md 'large for the CPU-bound scenarios (lettercount, and especially batch)'",
)

# ---- Claim 5: the jitter tax lands in opposite phases, cliff on oneclient ---
# rust.md + README: on oneclient the tax lands in the first request (a cliff
# that grows as memory shrinks); on lettercount it lands in init (a flat bump).
jit_pairs = {}
for c in stats.get("jitterCells", []):
    key = (c["arch"], c["scenario"], c["memory_mb"])
    jit_pairs.setdefault(key, {})[c["jitter"]] = c


def jit_scenario(scenario):
    return {
        (arch, mem): v
        for (arch, s, mem), v in jit_pairs.items()
        if s == scenario and "on" in v and "off" in v
    }


def gaps(v, field):
    return v["on"][field] - v["off"][field]


onecl = jit_scenario("oneclient")
fr, n = frac(
    list(onecl.values()), lambda v: gaps(v, "firstReqP50") > gaps(v, "initP50")
)
check(
    "oneclient jitter tax lands in the first request",
    fr is not None and fr >= CATEGORICAL,
    f"firstReq gap > init gap in {fmt(fr, n)}",
    "rust.md 'oneclient ... first TLS in the Invoke phase (the cliff)'",
)
lcount = jit_scenario("lettercount")
fr, n = frac(
    list(lcount.values()), lambda v: gaps(v, "initP50") > gaps(v, "firstReqP50")
)
check(
    "lettercount jitter tax lands in init",
    fr is not None and fr >= CATEGORICAL,
    f"init gap > firstReq gap in {fmt(fr, n)}",
    "rust.md 'lettercount ... first TLS in the Init phase (the flat bump)'",
)
# The cliff grows as memory shrinks: per arch, the oneclient total-P50 gap at the
# smallest shared tier exceeds the gap at the largest.
for arch in stats["dimensions"].get("architectures", []):
    mems = sorted(m for (a, m) in onecl.keys() if a == arch)
    if len(mems) < 2:
        continue
    lo, hi = onecl[(arch, mems[0])], onecl[(arch, mems[-1])]
    check(
        f"oneclient jitter cliff grows as memory shrinks ({arch})",
        gaps(lo, "totalP50") > gaps(hi, "totalP50"),
        f"total gap {gaps(lo, 'totalP50'):.0f} ms @{mems[0]}MB vs "
        f"{gaps(hi, 'totalP50'):.0f} ms @{mems[-1]}MB",
        "rust.md 'the on-vs-off gap grows steeply as memory shrinks'",
    )

# ---- Claim 6: the non-GC runtime holds the lowest cache warm tail -----------
# comparison.md: "The non-GC runtime holds the lowest tail at every tier."
# Rust vs every other series' warm P99 on cache, per (arch, memory); a 15%
# tolerance admits ties without letting a genuine reversal pass.
rust_cache = lang_cells("rust", "cache")
worst = None
compared = 0
for key, rc in rust_cache.items():
    r99 = (rc.get("warm") or {}).get("p99")
    if r99 is None:
        continue
    others = [
        (c.get("warm") or {}).get("p99")
        for c in cells
        if c["scenario"] == "cache"
        and c["lang"] != "rust"
        and (c["arch"], c["memory_mb"]) == key
    ]
    others = [o for o in others if o is not None]
    if not others:
        continue
    compared += 1
    ratio = r99 / min(others)
    if worst is None or ratio > worst[0]:
        worst = (ratio, key)
check(
    "rust holds the lowest cache warm P99 at every tier",
    compared > 0 and worst is not None and worst[0] <= 1.15,
    f"worst rust/min(other) P99 ratio {worst[0]:.2f} at {worst[1]}"
    if worst
    else "no comparable cells",
    "comparison.md 'The non-GC runtime holds the lowest tail at every tier'",
)

# ---- Claim 7: the smithy-java version quoted in prose matches the build -----
# README + java-snapstart.md both say the framework residue is "a property of
# the smithy-java API surface used in this run (X)". X is hand-typed; the source
# of truth is scenarios/java/gradle.properties.
props = os.path.join(REPO, "scenarios", "java", "gradle.properties")
with open(props) as f:
    m = re.search(r"^smithyJavaVersion=(\S+)", f.read(), re.MULTILINE)
if not m:
    check(
        "smithy-java version declared",
        False,
        f"no smithyJavaVersion= in {props}",
        "README + java-snapstart.md '(used in this run (X))'",
    )
else:
    ver = m.group(1)
    for rel in ("README.md", os.path.join("site", "src", "java-snapstart.md")):
        with open(os.path.join(REPO, rel)) as f:
            ok = f"({ver})" in f.read()
        check(
            f"smithy-java version in {rel}",
            ok,
            f"gradle.properties says {ver}; prose must quote ({ver})",
            f"{rel} 'the smithy-java API surface used in this run ({ver})'",
        )

print("Findings prose-vs-data check:")
print("\n".join(notes))
if failures:
    print(
        "\nDRIFT: the built stats.json no longer matches the findings prose. "
        "Update the prose (or re-verify the run):",
        file=sys.stderr,
    )
    for f in failures:
        print(f"  - {f}", file=sys.stderr)
    sys.exit(1)
print("\nAll checked findings claims still hold.")
