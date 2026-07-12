#!/usr/bin/env python3
"""Guard the hand-written numeric claims on the Cold Start Anatomy page
(site/src/lifecycle.md) against the freshly-produced probe data.

Almost every number on the site is rendered from stats.json (self-updating) or,
in the README "Finding" sections, framed as a dated one-off (so it does not
silently rot). lifecycle.md derives its exact ms from the probe JSONs at build
time, but it still asserts *qualitative* claims about the off-matrix probes (the
download+start table and the download-scaling / zip-vs-image charts) in prose, so
a Lambda-platform change could quietly falsify those shape claims.

This script asserts the KEY prose claims still hold against the three
probe JSONs (download+start, download-scaling, and the zip-vs-container-image
family). Those outputs are NOT committed: like the matrix run they are written
run-scoped into the repo-root results/ dir (lifecycle-<kind>-<run_id>.json,
gitignored) and the site's data loaders discover the newest of each at build time.
This script mirrors that discovery so it checks the same data the site will build
from. Tolerances are deliberately generous: they catch a real platform shift (the
floor moving, the slope changing, the runtime families diverging), not ordinary
run-to-run noise. On drift it exits non-zero and names the claim + the prose line
to revisit; wired into the publish pipeline as a NON-FATAL warning.

Run: python3 scripts/check-lifecycle-prose.py
"""
import json
import os
import re
import statistics
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RESULTS_DIR = os.path.join(REPO, "results")

failures = []
notes = []


def check(name, ok, detail, prose):
    """Record one claim's outcome. `prose` names what lifecycle.md says, so a
    failure points straight at the sentence to update."""
    status = "ok " if ok else "DRIFT"
    notes.append(f"  [{status}] {name}: {detail}")
    if not ok:
        failures.append(f"{name}: {detail}\n           prose to revisit: {prose}")


def newest_probe_file(kind):
    """Absolute path of the newest results/lifecycle-<kind>-<run_id>.json, where
    <run_id> is <unix_ms>-<hex> (the shared bencher run-id format). Mirrors the
    site loaders' discovery (site/src/lib/results-input.js): the regex is anchored
    on -<digits>-<hex>.json so 'download-scaling' never matches a
    'download-scaling-image' file, and ordering is numeric on <unix_ms> so a
    9-vs-13-digit clock change cannot misrank. Exits 2 (like the old missing-file
    behavior) when no matching probe output exists, so the pipeline treats absent
    data as a hard, visible failure rather than silently passing."""
    pat = re.compile(rf"^lifecycle-{re.escape(kind)}-(\d+)-[0-9a-f]+\.json$")
    try:
        entries = os.listdir(RESULTS_DIR)
    except OSError as e:
        print(f"cannot read results dir {RESULTS_DIR}: {e}", file=sys.stderr)
        sys.exit(2)
    matches = [(int(m.group(1)), f) for f in entries if (m := pat.match(f))]
    if not matches:
        print(
            f"no {kind} probe output in {RESULTS_DIR} (expected lifecycle-{kind}-<run_id>.json)",
            file=sys.stderr,
        )
        sys.exit(2)
    matches.sort(reverse=True)
    return os.path.join(RESULTS_DIR, matches[0][1])


def load(kind):
    with open(newest_probe_file(kind)) as f:
        return json.load(f)


start = load("download-start")
scaling = load("download-scaling")
# The container-image family is refreshed by the publish pipeline (crane is
# daemonless, so it runs on Fargate like the zip families), so its output is a
# first-class input to this check, same as the two above.
image = load("download-scaling-image")
scells = start["cells"]
ssamp = scaling["samples"]
isamp = image["samples"]


def require(name, ok, detail):
    """Fail loud if a data point the claims below rely on is absent.

    Every claim is otherwise gated by an `if <point> present` guard, so a
    truncated or empty probe JSON would make the claim silently skip and the
    script still print "all hold" and exit 0, defeating the guard's purpose.
    This asserts the expected shape up front so a partial run is caught as a
    hard failure, not a vacuous pass."""
    if not ok:
        failures.append(f"{name}: missing expected probe data ({detail}); the guard "
                        f"cannot check its claims. Re-run the probe or inspect the JSON.")


# The claims below are anchored to these specific points; a run missing any of
# them can't be checked, so assert their presence rather than skipping silently.
EXPECTED_SCALING_SIZES = {1, 10, 200}
EXPECTED_FAMILIES = {"python", "rust"}
scaling_sizes_by_fam = {}
for s in ssamp:
    scaling_sizes_by_fam.setdefault(s.get("family", "python"), set()).add(s["size_mb"])
require("scaling families", set(scaling_sizes_by_fam) == EXPECTED_FAMILIES,
        f"expected families {sorted(EXPECTED_FAMILIES)}, got {sorted(scaling_sizes_by_fam)}")
for fam in EXPECTED_FAMILIES:
    got = scaling_sizes_by_fam.get(fam, set())
    require(f"scaling sizes ({fam})", EXPECTED_SCALING_SIZES <= got,
            f"expected sizes {sorted(EXPECTED_SCALING_SIZES)} for {fam}, got {sorted(got)}")

require("start hello cells", any(c["scenario"] == "hello" for c in scells),
        "no hello cells in the download-start probe")
require("start rust/hello cell",
        any(c["lang"] == "rust" and c["scenario"] == "hello" for c in scells),
        "no rust/hello cell in the download-start probe")
# lifecycle.md renders the 128 MB rust/hello cell inline (see its
# `c.memory_mb === 128` selector), so Claim 6c below must guard THAT cell. If the
# probe ever drops the 128 MB tier, the page would throw `undefined.init_p50` at
# build time; assert the exact cell up front so that surfaces here first.
require("start rust/hello 128MB cell",
        any(c["lang"] == "rust" and c["scenario"] == "hello" and c["memory_mb"] == 128
            for c in scells),
        "no 128 MB rust/hello cell in the download-start probe (the tier lifecycle.md renders)")
# lifecycle.md renders the > 13 MB @ 512 MB cells inline (its `dlBig` selector, the
# "large bundle lift" bullet). With none present the page shows `NaN-NaN MB` while
# Claim 2 below silently skips (its `if big:` guard), so assert the cell up front,
# exactly as for the rust/hello 128 MB tier above.
require("start big-artifact 512MB cell",
        any(c["zip_bytes"] > 13e6 and c["memory_mb"] == 512 for c in scells),
        "no > 13 MB @ 512 MB cell in the download-start probe (the cell lifecycle.md renders)")

img_sizes_by_fam = {}
for s in isamp:
    img_sizes_by_fam.setdefault(s["family"], set()).add(s["size_mb"])
for fam in ("image-touched", "image-untouched"):
    got = img_sizes_by_fam.get(fam, set())
    require(f"image sizes ({fam})", {10, 200} <= got,
            f"expected sizes 10 and 200 for {fam}, got {sorted(got)}")
require("image zip_baseline", len(image.get("zip_baseline", [])) > 0,
        "no zip_baseline in the container-image probe (needed for the overhead-flip claim)")

# A shape failure means the claims can't be evaluated; report and stop before the
# per-claim checks run against absent points (which would look like a clean pass).
if failures:
    print("Cold Start Anatomy prose-vs-data check: cannot run.", file=sys.stderr)
    for f in failures:
        print(f"  - {f}", file=sys.stderr)
    sys.exit(1)


# --- Claim 1: the ~110 ms-scale provisioning floor (read off the hello cells). ---
# lifecycle.md derives the floor it prints ("a flat floor of ~<floor> ms up to a
# few MB") from these same hello cells at build time, so the printed number cannot
# drift; what this guards is the REGIME the surrounding narrative assumes (a
# roughly-100 ms flat floor that download is "lost in").
hello = [c["residual_p50"] for c in scells if c["scenario"] == "hello"]
floor = statistics.median(hello) if hello else 0.0
check(
    "provisioning floor (hello cells)",
    90 <= floor <= 140,
    f"median hello residual = {floor:.0f} ms (expect 90-140)",
    "'a flat floor of ~<floor> ms up to a few MB' (floor derived at build time; "
    "the narrative assumes a ~100 ms regime)",
)

# --- Claim 2: large matrix artifacts sit a lift above the floor. ---
# lifecycle.md renders the lift from data: "the ~14-17 MB Java/Python bundles
# sit ~<lo>-<hi> ms above the floor" (dlBigLift). This guards that the largest
# matrix bundle clears the floor by a visible margin, so the "download term only
# becomes visible once the artifact is genuinely large" claim holds.
# The `require` above guarantees at least one such cell, so this runs unconditionally.
big = [c for c in scells if c["zip_bytes"] > 13e6 and c["memory_mb"] == 512]
over = max(c["residual_p50"] for c in big) - floor
check(
    "large matrix artifact lift",
    over >= 60,
    f"max(14-17MB residual) - floor = {over:.0f} ms (drift if < 60)",
    "'the ~14-17 MB Java/Python bundles sit <lift> ms above the floor'",
)

# --- Claim 3: 200 MB residual is of order a second, both families. ---
# lifecycle.md: "of order a second and up at 200 MB" (the exact per-run values it
# prints are derived from this JSON). The absolute value swings run to run
# (measured ~1.0-1.7 s across runs), so the band is wide: this guards "the
# download term is hundreds of ms to ~2 s at 200 MB", not a precise value.
by_fam = {}
init_by_fam = {}
for s in ssamp:
    by_fam.setdefault(s.get("family", "python"), {})[s["size_mb"]] = s["residual_p50"]
    init_by_fam.setdefault(s.get("family", "python"), {})[s["size_mb"]] = s["init_p50"]
for fam, sizes in by_fam.items():
    if 200 in sizes:
        r = sizes[200]
        check(
            f"200 MB residual ({fam})",
            800 <= r <= 2100,
            f"{r:.0f} ms (expect 800-2100, of order ~1-2 s)",
            "'of order a second and up at 200 MB'",
        )

# --- Claim 4: near-linear ~4-8 ms/MB climb past the knee (10 -> 200 MB). ---
# lifecycle.md: "very roughly 4-8 ms per MB". Slope swings run to run; guard the
# "near-linear, single-digit ms/MB" shape.
for fam, sizes in by_fam.items():
    if 10 in sizes and 200 in sizes:
        slope = (sizes[200] - sizes[10]) / (200 - 10)
        check(
            f"download slope ({fam})",
            3.0 <= slope <= 11.0,
            f"{slope:.1f} ms/MB over 10-200 MB (expect 3-11)",
            "'a near-linear climb of very roughly 4-8 ms per MB'",
        )

# --- Claim 5: the two runtime families are within noise of each other, so the
# gap between them is not a runtime effect. lifecycle.md: "at every size their
# min-max bands overlap and the gap between their medians stays smaller than the
# spread within either family ... that gap is within noise, not a runtime effect".
# NOT a crossover check: which family reads higher, and whether the median
# ordering flips across sizes, is itself run-to-run noise, so guarding the flip
# would false-alarm on clean noise (a run with one family incidentally on top at
# every size). The prose-matching, run-agnostic invariant is band overlap plus the
# between-family gap staying under the within-family spread; that is what breaks
# if a real per-runtime download cost ever emerges.
fams = list(by_fam.keys())
if len(fams) == 2:
    a, b = fams
    band = {}  # family -> {mb: (residual_min, residual_max)}
    for s in ssamp:
        band.setdefault(s.get("family", "python"), {})[s["size_mb"]] = (
            s["residual_min"], s["residual_max"])
    common = sorted(set(by_fam[a]) & set(by_fam[b]) & set(band[a]) & set(band[b]))
    overlap_all = True
    gap_under_spread_all = True
    per_size = []
    for mb in common:
        amin, amax = band[a][mb]
        bmin, bmax = band[b][mb]
        overlap = not (amin > bmax or bmin > amax)
        gap = abs(by_fam[a][mb] - by_fam[b][mb])
        # "smaller than the spread within either family" = under the tighter
        # family's own min-max spread, the stricter reading.
        spread = min(amax - amin, bmax - bmin)
        overlap_all = overlap_all and overlap
        gap_under_spread_all = gap_under_spread_all and gap < spread
        per_size.append(f"{mb}MB(overlap={overlap},gap={gap:.0f},spread={spread:.0f})")
    check(
        "runtime families within noise (overlap + gap<spread)",
        overlap_all and gap_under_spread_all,
        f"{a} vs {b}: " + ", ".join(per_size),
        "'at every size their min-max bands overlap and the gap between medians stays "
        "smaller than the spread within either family ... within noise, not a runtime effect'",
    )

# --- Claim 6: Init Duration stays flat as artifact size grows. ---
# lifecycle.md: "the reported Init Duration stays flat ... while the dashed residual
# climbs by roughly an order of magnitude ... If code download were folded into init,
# that solid line would climb with the dashed one and a 200 MB package's init would
# be seconds." The residual grows by >1000 ms from 1->200 MB; if download were in
# init, init would track it. A flat init means the growth is outside the metric,
# which is the finding. Tolerance is generous (init noise is tens of ms; a real
# trend would be hundreds+).
for fam, inits in init_by_fam.items():
    if 1 in inits and 200 in inits:
        drift = inits[200] - inits[1]
        check(
            f"init flat vs size ({fam})",
            drift < 100,
            f"init 1MB={inits[1]:.0f} -> 200MB={inits[200]:.0f} ms (grew {drift:.0f}; flat expected, drift if > 100)",
            "'the reported Init Duration stays flat ... while the dashed residual "
            "climbs by roughly an order of magnitude'",
        )

# --- Claim 6b: the residual climbs ~an order of magnitude from 1 -> 200 MB (the
# ratio the prose states alongside the flat init). Guard the max family order of
# magnitude (measured ~8-14x run to run) so a shift that flattened the slope to a
# small factor trips. lifecycle.md: "roughly an order of magnitude (~8-14x run to
# run)".
resid_by_fam = {}
for s in ssamp:
    resid_by_fam.setdefault(s.get("family", "python"), {})[s["size_mb"]] = s["residual_p50"]
ratios = [r[200] / r[1] for r in resid_by_fam.values() if 1 in r and 200 in r and r[1] > 0]
if ratios:
    top = max(ratios)
    check(
        "residual climbs ~order of magnitude (1 -> 200 MB)",
        6.0 <= top <= 18.0,
        f"max family residual ratio 200MB/1MB = {top:.1f}x (prose says ~8-14x; expect 6-18)",
        "'the dashed residual climbs by roughly an order of magnitude (~8-14x run to run)'",
    )

# --- Claim 6c: the rust/hello wall-clock narrative (the argument that download is
# NOT in Init Duration). lifecycle.md derives the three ms it prints from this same
# cell ("reports an Init Duration of ~<init>, about what an empty main plus runtime
# start should cost, yet the caller's wall-clock ... is ~<wall> with the network
# path subtracted out ... leaves a residual (~<resid> here ...)"), so the printed
# numbers cannot drift. These come from the download-START probe (start["cells"]),
# not the scaling probe. What this guards is the REGIME the narrative assumes:
# a low-double-digit-ms init dwarfed by a ~100 ms-scale wall-clock and residual.
rh = [c for c in scells if c["lang"] == "rust" and c["scenario"] == "hello"]
if rh:
    # Guard the 128 MB cell: it is the exact one lifecycle.md renders inline
    # (its `c.memory_mb === 128` selector), so the numbers a reader sees are the
    # ones checked here. Its presence is asserted by the `require` above, so the
    # lookup below cannot miss silently; fall back to the first cell only if that
    # invariant is ever loosened.
    cell = next((c for c in rh if c["memory_mb"] == 128), rh[0])
    init_ms = cell["init_p50"]
    wall = cell["w_cold_p50"] - cell["warm_rtt_p50"]
    resid = cell["residual_p50"]
    check(
        "rust/hello init (low-double-digit ms)",
        10 <= init_ms <= 20,
        f"init_p50 = {init_ms:.0f} ms (expect 10-20)",
        "'a trivial Rust binary reports an Init Duration of ~<init>, about what an "
        "empty main plus runtime start should cost' (derived at build time)",
    )
    check(
        "rust/hello wall-clock (~100 ms scale)",
        105 <= wall <= 175,
        f"W_cold - warm_rtt = {wall:.0f} ms (expect 105-175)",
        "'the caller's wall-clock for the same cold invoke is ~<wall> with the "
        "network path subtracted out' (derived at build time)",
    )
    check(
        "rust/hello residual (~100 ms scale, ≫ init)",
        90 <= resid <= 160,
        f"residual_p50 = {resid:.0f} ms (expect 90-160)",
        "'leaves a residual (~<resid> here ...) that ... lands in neither Init "
        "Duration nor the invoke Duration' (derived at build time)",
    )

# --- Claims 7-10: the container-image "Zip vs container image" subsection. ---
# The image family is a first-class committed input the pipeline refreshes, like
# the zip families.
#
# The exact image magnitudes (touched-init endpoints, residual-floor range, the
# total-overhead table) are DERIVED from this JSON in lifecycle.md itself, so they
# cannot go stale against the chart and need no numeric guard here. What remains
# guardable is the QUALITATIVE finding the prose asserts in words: touched init
# climbs with size, both residuals stay flat, and the total-overhead ordering
# flips (zip lower when small, image lower when large). Those are what break if the
# platform behaviour changes, so those are what we check.
img_init = {}
img_resid = {}
for s in isamp:
    img_init.setdefault(s["family"], {})[s["size_mb"]] = s["init_p50"]
    img_resid.setdefault(s["family"], {})[s["size_mb"]] = s["residual_p50"]

# Claim 7: image-touched init GROWS with size (the size cost lands in the
# reported Init Duration). lifecycle.md: "its solid init is the line that
# climbs ... for the variant that actually loads the code at init".
ti = img_init.get("image-touched", {})
if 10 in ti and 200 in ti:
    grew = ti[200] - ti[10]
    check(
        "image-touched init grows with size",
        grew >= 80,
        f"touched init 10MB={ti[10]:.0f} -> 200MB={ti[200]:.0f} ms (grew {grew:.0f}; expect >= 80)",
        "'its solid init is the line that climbs, but only for the variant that "
        "reads the added padding at init'",
    )

# Claim 8: image residual is a FLAT floor across sizes for both variants (the
# unreported part does not grow). The floor's ms range is derived in the prose;
# what is guarded is that it stays FLAT (small spread). lifecycle.md: "the image's
# dashed residual stays a flat ~<lo>-<hi> ms floor at every size".
for fam, resid in img_resid.items():
    if len(resid) >= 3:
        spread = max(resid.values()) - min(resid.values())
        check(
            f"image residual flat ({fam})",
            spread < 150,
            f"residual spread across sizes = {spread:.0f} ms (expect < 150, flat floor)",
            "'the image's dashed residual stays a flat ~<lo>-<hi> ms floor at every "
            "size' (range derived at build time; the FLATNESS is what is guarded)",
        )

# Claim 9: image-untouched init stays flat with size (lazy loading skips
# untouched padding). lifecycle.md: "The variant that never touches the padding
# stays flat on both lines."
ui = img_init.get("image-untouched", {})
if 10 in ui and 200 in ui:
    udrift = abs(ui[200] - ui[10])
    check(
        "image-untouched init flat",
        udrift < 100,
        f"untouched init 10MB={ui[10]:.0f} -> 200MB={ui[200]:.0f} ms (drift {udrift:.0f}; flat expected, < 100)",
        "'The variant that never touches the padding stays flat on both lines'",
    )

# Claim 10: the TOTAL cold-overhead ORDERING flips across size (the crossover the
# prose and its derived table describe): the zip is lower when the artifact is
# small, the image far lower when it is large. The table's exact ms are derived
# from this JSON in lifecycle.md, so only the qualitative flip is guarded here.
# zip totals come from the image JSON's co-measured `zip_baseline` (the SAME
# source the prose table and the chart use), NOT the separately-written scaling
# JSON: the two are distinct probe invocations and can diverge. image totals come
# from the touched variant. lifecycle.md: "the .zip starts lower ... but the image
# pulls ahead as size grows".
zip_total = {}
for s in image.get("zip_baseline", []):
    zip_total[s["size_mb"]] = s["init_p50"] + s["residual_p50"]
img_total = {mb: img_init.get("image-touched", {}).get(mb, 0) + img_resid.get("image-touched", {}).get(mb, 0)
             for mb in img_init.get("image-touched", {})}
small = min(zip_total.keys() & img_total.keys(), default=None)
large = max(zip_total.keys() & img_total.keys(), default=None)
if small is not None:
    check(
        "total overhead: zip lower when small",
        zip_total[small] < img_total[small],
        f"{small}MB zip={zip_total[small]:.0f} vs image={img_total[small]:.0f} ms (zip expected lower)",
        "'the .zip starts lower (it has no base-layer floor)'",
    )
if large is not None:
    # "pulls ahead" needs a substantial, not hairline, margin at the large end, but
    # the exact margin swings run to run (measured ~15-40% lower). Guard image at
    # least ~15% below zip: passes ordinary noise, trips only if the crossover
    # margin collapses toward parity (or reverses), which is what would falsify the
    # prose.
    check(
        "total overhead: image lower when large",
        img_total[large] < zip_total[large] * 0.85,
        f"{large}MB zip={zip_total[large]:.0f} vs image={img_total[large]:.0f} ms (drift if image not < 85% of zip)",
        "'the image pulls ahead as size grows'",
    )

print("Cold Start Anatomy prose-vs-data check:")
print("\n".join(notes))
if failures:
    print(
        "\nDRIFT: the committed probe data no longer matches lifecycle.md's prose. "
        "Update the prose (or re-verify the run):",
        file=sys.stderr,
    )
    for f in failures:
        print(f"  - {f}", file=sys.stderr)
    sys.exit(1)
print("\nAll checked lifecycle.md claims still hold.")
