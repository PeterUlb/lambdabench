// Pure aggregation core shared by the data loader and its tests. Takes the
// already-projected rows plus the run meta and returns the compact payload the
// pages render client-side. No filesystem or process access, so the build-time
// transform runs unchanged against synthetic rows in a unit test. The I/O shell
// (src/data/stats.json.js) finds the input, streams rows into the projection
// these functions expect, and writes the result to stdout.

import {
  quantile,
  median,
  summarizeValues,
  cleanSort,
  midOf,
} from "./stats.js";
import { langKey, seriesOf, isSnapLang } from "./series.js";
import { KNOWN_SCENARIO_ORDER } from "./format.js";

// ---- Run completeness contract ---------------------------------------------
// A run that aborts mid-run is marked "failed" with fewer rows than planned,
// leaving a truncated .jsonl.gz (see bencher/src/record.rs). Percentiles over a
// partial matrix look authoritative but are wrong (missing cells, under-sampled
// tails), so refuse to publish a known-incomplete run. Older metas predate the
// `status`/`total_invocations_recorded` fields; absent, there is no signal, so
// proceed. Separate from aggregate() so the I/O shell can call it before
// streaming the (potentially huge, truncated) results file off disk.
export function assertRunComplete(meta) {
  if (meta.status === "failed") {
    throw new Error(
      `run ${meta.run_id ?? "?"} is marked status="failed": its results file is ` +
        `truncated (${meta.total_invocations_recorded ?? "?"} of ` +
        `${meta.total_invocations_planned ?? "?"} planned invocations recorded). ` +
        `Refusing to publish partial stats; investigate the run or point LAMBDABENCH_RESULTS ` +
        `at a complete one.`,
    );
  }
  // A meta still marked "running" never reached a terminal state (the harness
  // rewrites it to "ok"/"failed" only after the run loop returns, see
  // bencher/src/main.rs), so its results file is whatever the crash left behind.
  // Reject on status alone: the recorded<planned gate only catches a crash
  // because the initial meta happens to carry recorded=0, a coincidence.
  if (meta.status === "running") {
    throw new Error(
      `run ${meta.run_id ?? "?"} is still marked status="running": it never reached ` +
        `a terminal state (likely crashed mid-run), so its results file may be ` +
        `truncated. Refusing to publish; investigate the run or point LAMBDABENCH_RESULTS ` +
        `at a complete one.`,
    );
  }
  if (
    meta.total_invocations_recorded != null &&
    meta.total_invocations_planned != null &&
    meta.total_invocations_recorded < meta.total_invocations_planned
  ) {
    throw new Error(
      `run ${meta.run_id ?? "?"} recorded ${meta.total_invocations_recorded} of ` +
        `${meta.total_invocations_planned} planned invocations (incomplete dataset); ` +
        `refusing to publish partial stats.`,
    );
  }
}

// Cross-check the number of rows actually streamed off the data file against the
// count the run recorded in its meta. The meta and the .jsonl.gz are two
// independent files: assertRunComplete only compares fields WITHIN the meta, so
// it cannot detect a data file truncated after the run cleanly stamped itself
// complete (footer written, trailing rows lost to a partial copy of the results
// dir or a page-cache flush that never landed). meta.status would still read "ok"
// while the file holds a subset, publishing percentiles over a partial matrix.
// This is the one cross-file check that closes that window, using the field
// record.rs persists for exactly this purpose. Skipped for older metas that
// predate the field (`total_invocations_recorded == null`), consistent with
// assertRunComplete's older-meta handling. `inputPath` labels the file in the error.
export function assertRowsMatchMeta(rowCount, meta, inputPath) {
  if (meta.total_invocations_recorded == null) return;
  if (rowCount !== meta.total_invocations_recorded) {
    throw new Error(
      `results file ${inputPath} holds ${rowCount} rows but its meta records ` +
        `${meta.total_invocations_recorded} invocations (run ${meta.run_id ?? "?"}): ` +
        `the data file is truncated or mismatched. Refusing to publish partial stats; ` +
        `investigate the run or point LAMBDABENCH_RESULTS at a complete one.`,
    );
  }
}

// ---- Derived predicates ----------------------------------------------------
const REPRESENTATIVE_OPT = "o3"; // Rust-only dimension; o3 is the headline build.
// Representative set: collapses Rust opt-level to o3 and drops Rust jitter=On
// diagnostic cells. Those share series/scenario/memory keys with the standing
// jitter=Off cells, so keeping them would average two builds together and
// inflate Rust's cold-start on the two A/B scenarios. SnapStart is not collapsed
// (its own series via langKey), so it appears as a peer line.
const isRepresentative = (d) =>
  (d.opt == null || d.opt === REPRESENTATIVE_OPT) && d.jitter !== "on";
// Cold marker: Init Duration for a plain cold start, Restore Duration for a
// SnapStart restore. Either marks a cold start.
const coldMarkerMs = (d) => (d.init_ms != null ? d.init_ms : d.restore_ms);
// Requiring a finite marker (not merely `!= null`) makes a NaN-marker cold row
// fail the `coldWithoutMarker` gate loudly rather than sum to a NaN `coldTotal`
// that `cleanSort` silently discards.
const isColdSample = (d) => d.is_cold && Number.isFinite(coldMarkerMs(d));
// Cold total = marker + first-request duration. A null/non-numeric duration_ms
// would sum to NaN; validated loudly below (`coldWithoutDuration`).
const coldTotal = (d) =>
  isColdSample(d) ? coldMarkerMs(d) + d.duration_ms : null;

// Lambda on-demand pricing (eu-central-1). GB-second +
// per-request; arm64 is ~20% cheaper per GB-s, so each row is priced by its arch.
const PRICE_REQ = 0.2 / 1e6;
const PRICE_GBS = { arm64: 0.0000133334, x86_64: 0.0000166667 };
const dollarsPerM = (d) => {
  // Fail loud on an unrecognized arch rather than mispricing it: arch is a small
  // fixed set, so a miss is a real anomaly, and a wrong cost on a new arch's own
  // series would be hard to spot.
  const price = PRICE_GBS[d.arch];
  if (price == null) {
    throw new Error(
      `no GB-second price for arch "${d.arch}"; refusing to misprice it (known: ` +
        `${Object.keys(PRICE_GBS).join(", ")}). Add it to PRICE_GBS or inspect the run.`,
    );
  }
  const gbs = (d.memory_mb / 1024) * (d.billed_ms / 1000);
  return (gbs * price + PRICE_REQ) * 1e6;
};

// Distribution thinning knobs (see sampleGroup).
const MAX_BODY_DOTS = 220;
const TAIL_QUANTILE = 0.98;
// Thin a group's raw values, keeping the full tail: every point at/above P98
// survives (the rare outliers the chart exists to show), the dense body below is
// uniform-stride sampled in sorted order to preserve its density profile.
function sampleGroup(values) {
  if (values.length <= MAX_BODY_DOTS) return values;
  const sorted = values.slice().sort((a, b) => a - b);
  const cutoffIdx = Math.floor(TAIL_QUANTILE * (sorted.length - 1));
  const tail = sorted.slice(cutoffIdx);
  const body = sorted.slice(0, cutoffIdx);
  // When the body already fits, keep it whole: striding a body of <= MAX_BODY_DOTS
  // points would take a fractional step < 1 and re-emit points, so the output
  // could exceed the input and duplicate real values.
  if (body.length <= MAX_BODY_DOTS) return body.concat(tail);
  const out = [];
  const step = body.length / MAX_BODY_DOTS;
  for (let i = 0; i < MAX_BODY_DOTS; i++) out.push(body[Math.floor(i * step)]);
  out[0] = body[0];
  return out.concat(tail);
}

// Build the compact payload from the projected rows. `meta` may be empty for
// older runs; `inputBasename` is carried through to the payload provenance.
// Throws on a known-incomplete run via the cold-row gates below; the meta-level
// completeness gate lives in assertRunComplete, called by the I/O shell first.
export function aggregate(rows, meta = {}, { inputBasename = null } = {}) {
  if (rows.length === 0) throw new Error("no rows in input");

  // ---- Dimensions ----------------------------------------------------------
  const languages = [...new Set(rows.map(langKey))].sort();
  const rawLanguages = [...new Set(rows.map((r) => r.lang))].sort();
  // A real runtime whose name ended in `-snapstart` (the pseudo-language suffix,
  // see series.js) would be indistinguishable from a SnapStart variant once
  // collapsed into a series key, misclassifying its label, hue, and artifact
  // handling. `d.snapstart` is the source of truth, so fail loud here rather than
  // mislabel data.
  const collidingLang = rawLanguages.find(isSnapLang);
  if (collidingLang) {
    throw new Error(
      `runtime name "${collidingLang}" collides with the reserved SnapStart series ` +
        `suffix; rename the runtime (SnapStart is derived from the snapstart flag, ` +
        `not the runtime name).`,
    );
  }
  const architectures = [...new Set(rows.map((r) => r.arch))].sort();
  const memories = [...new Set(rows.map((r) => r.memory_mb))].sort(
    (a, b) => a - b,
  );
  const presentScenarios = [...new Set(rows.map((r) => r.scenario))];
  const scenarios = [
    ...KNOWN_SCENARIO_ORDER.filter((s) => presentScenarios.includes(s)),
    ...presentScenarios.filter((s) => !KNOWN_SCENARIO_ORDER.includes(s)).sort(),
  ];
  const hasOpt = rows.some((d) => d.opt === "oz");
  const hasSnapStart = rows.some((d) => d.snapstart);
  const hasJitter = rows.some((d) => d.jitter === "on");
  const pivotMem = memories.includes(1024) ? 1024 : midOf(memories);

  // ---- Main cells: one bundled stat object per representative series cell ---
  // Each cell carries every percentile family (warm / cold-total / cold-marker /
  // footprint / cost) so one filterable array drives all the headline charts.
  // Accumulate raw value arrays per cell, then summarize.
  const cellMap = new Map();
  function cellFor(d) {
    const key = `${seriesOf(d)}|${d.scenario}|${d.memory_mb}`;
    let c = cellMap.get(key);
    if (!c) {
      c = {
        series: seriesOf(d),
        lang: langKey(d),
        rawLang: d.lang,
        arch: d.arch,
        scenario: d.scenario,
        memory_mb: d.memory_mb,
        _warm: [],
        // Distinct cold cycles the warm samples span; emitted as `warmCycles`
        // (see DESIGN.md's "effective n for the tail" reading rule).
        _warmCycles: new Set(),
        _coldTotal: [],
        _coldMarker: [],
        _coldFirstReq: [],
        _foot: [],
        _cost: [],
      };
      cellMap.set(key, c);
    }
    return c;
  }
  // A cold-flagged row with no finite init/restore marker fits neither branch
  // and would vanish from every latency aggregate. Count and fail loud below.
  // Checked over every row, not just the representative ones: the Rust opt-level
  // A/B (`optCells`) consumes the non-representative `oz` rows too.
  let coldWithoutMarker = 0;
  // A cold sample with a marker but a null/non-finite duration_ms makes
  // `coldTotal` NaN, which `cleanSort` silently drops from the cold aggregate.
  let coldWithoutDuration = 0;
  // A warm sample with a null/non-finite duration_ms would be silently dropped
  // by cleanSort from the warm aggregate.
  let warmWithoutDuration = 0;
  // A footprint/cost input that is present (`!= null`) but non-finite passes the
  // `!= null` guard yet cleanSort drops it, so `footprint.n`/`cost.n` diverge
  // from `warm.n` with no trace. (A genuinely `null` field is legitimately
  // absent and stays out.)
  let footNonFinite = 0;
  let costNonFinite = 0;
  for (const d of rows) {
    if (d.is_cold && !isColdSample(d)) coldWithoutMarker++;
    else if (isColdSample(d) && !Number.isFinite(d.duration_ms))
      coldWithoutDuration++;
    else if (!d.is_cold && !Number.isFinite(d.duration_ms))
      warmWithoutDuration++;
  }
  for (const d of rows) {
    if (!isRepresentative(d)) continue;
    const c = cellFor(d);
    if (isColdSample(d)) {
      c._coldTotal.push(coldTotal(d));
      c._coldMarker.push(coldMarkerMs(d));
      // First-request component: the duration of the one request that runs on
      // the cold start (later requests on that instance are warm). On a cold
      // sample `coldTotal` is `marker + duration_ms`, so this is exactly
      // `duration_ms`. Accumulated raw so the cold breakdown plots a true
      // first-request P50, not the non-additive `cold.p50 - coldInit.p50`.
      c._coldFirstReq.push(d.duration_ms);
    } else if (d.is_cold) {
      // Counted in the all-rows pass above; nothing to accumulate here.
    } else {
      c._warm.push(d.duration_ms);
      if (d.cycle != null) c._warmCycles.add(d.cycle);
      if (d.max_memory_used_mb != null) {
        if (Number.isFinite(d.max_memory_used_mb))
          c._foot.push(d.max_memory_used_mb);
        else footNonFinite++;
      }
      if (d.billed_ms != null) {
        const cost = dollarsPerM(d);
        if (Number.isFinite(cost)) c._cost.push(cost);
        else costNonFinite++;
      }
    }
  }
  if (coldWithoutMarker > 0) {
    throw new Error(
      `${coldWithoutMarker} row(s) are flagged is_cold but carry no ` +
        `init_ms/restore_ms marker; they would be dropped from the cold/warm ` +
        `aggregates (and from the Rust opt-level A/B). Inspect the run output ` +
        `rather than rendering partial stats.`,
    );
  }
  if (coldWithoutDuration > 0) {
    throw new Error(
      `${coldWithoutDuration} cold sample(s) carry a marker but a null/non-finite ` +
        `duration_ms; their cold total would be NaN and silently dropped from the ` +
        `cold aggregate. Inspect the run output rather than rendering partial stats.`,
    );
  }
  if (warmWithoutDuration > 0) {
    throw new Error(
      `${warmWithoutDuration} warm sample(s) carry a null/non-finite duration_ms; ` +
        `they would be silently dropped from the warm aggregate. Inspect the run ` +
        `output rather than rendering partial stats.`,
    );
  }
  if (footNonFinite > 0) {
    throw new Error(
      `${footNonFinite} warm sample(s) carry a non-finite max_memory_used_mb; ` +
        `they would be silently dropped from the footprint aggregate (its n would ` +
        `diverge from the warm n). Inspect the run output rather than rendering ` +
        `partial stats.`,
    );
  }
  if (costNonFinite > 0) {
    throw new Error(
      `${costNonFinite} warm sample(s) yield a non-finite cost (non-finite ` +
        `billed_ms/memory_mb); they would be silently dropped from the cost ` +
        `aggregate. Inspect the run output rather than rendering partial stats.`,
    );
  }
  const cells = [];
  for (const c of cellMap.values()) {
    const warm = summarizeValues(c._warm);
    const coldTotalS = summarizeValues(c._coldTotal);
    const coldMarkerS = summarizeValues(c._coldMarker);
    const coldFirstReqS = summarizeValues(c._coldFirstReq);
    const footS = summarizeValues(c._foot);
    const costS = summarizeValues(c._cost);
    if (!warm && !coldTotalS) continue;
    cells.push({
      series: c.series,
      lang: c.lang,
      rawLang: c.rawLang,
      arch: c.arch,
      scenario: c.scenario,
      memory_mb: c.memory_mb,
      warm,
      // Distinct cold cycles behind the warm samples, the effective independent
      // count for the warm tail; null on older runs whose rows carry no `cycle`.
      warmCycles: c._warmCycles.size || null,
      cold: coldTotalS,
      coldInit: coldMarkerS,
      coldFirstReq: coldFirstReqS,
      footprint: footS,
      cost: costS,
    });
  }
  cells.sort((a, b) => a.memory_mb - b.memory_mb);

  // ---- Rust opt-level A/B cells --------------------------------------------
  // Per (arch, scenario, opt, memory): cold-init P50 + warm P50, plus the
  // representative artifact size. Drops jitter=On rows (as isRepresentative
  // does): the jitter A/B shares arch/scenario/opt/memory keys with the standing
  // jitter=Off cells, so keeping it would inflate the o3 cold-init on those two
  // scenarios and compare a jitter-mixed o3 against a jitter-off oz.
  let optCells = [];
  if (hasOpt) {
    const optMap = new Map();
    for (const d of rows) {
      if (d.lang !== "rust") continue;
      if (d.jitter === "on") continue;
      const key = `${d.arch}|${d.scenario}|${d.opt}|${d.memory_mb}`;
      let g = optMap.get(key);
      if (!g) {
        g = {
          arch: d.arch,
          scenario: d.scenario,
          opt: d.opt,
          memory_mb: d.memory_mb,
          _cold: [],
          _warm: [],
          unzipped: null,
          zip: null,
        };
        optMap.set(key, g);
      }
      if (isColdSample(d)) g._cold.push(coldMarkerMs(d));
      if (!d.is_cold) g._warm.push(d.duration_ms);
      if (g.unzipped == null && d.artifact_unzipped_bytes)
        g.unzipped = d.artifact_unzipped_bytes;
      if (g.zip == null && d.artifact_zip_bytes) g.zip = d.artifact_zip_bytes;
    }
    optCells = [...optMap.values()].map((g) => ({
      arch: g.arch,
      scenario: g.scenario,
      opt: g.opt,
      memory_mb: g.memory_mb,
      coldInitP50: median(g._cold),
      warmP50: median(g._warm),
      unzipped: g.unzipped,
      zip: g.zip,
    }));
  }

  // ---- Rust aws-lc-rs jitter-entropy A/B cells -----------------------------
  // Per (arch, scenario, memory, jitter): cold-init P50, cold-firstReq P50, and
  // cold-total P50. All three because which segment the tax lands in is the
  // story. Empirically Lambda's Init and Invoke phases behave as if on different
  // CPU envelopes (Init-phase boost from re:Invent 2019, undocumented elsewhere;
  // see the README Finding for the disclaimer):
  //   - oneclient builds the SDK in the Init phase but the first TLS handshake
  //     is in the Invoke phase, so the tax lands in firstReq, a cliff that grows
  //     steeply as memory shrinks.
  //   - lettercount does an `s3.get_object().send()` in the Init phase, which
  //     appears to run on more CPU than the tier alone provides, so the tax lands
  //     in init_ms and is roughly flat across tiers.
  // Restricted to Rust o3 cells with jitter ∈ {off, on}.
  let jitterCells = [];
  if (hasJitter) {
    const jitMap = new Map();
    for (const d of rows) {
      if (d.lang !== "rust") continue;
      if (d.opt !== REPRESENTATIVE_OPT) continue;
      if (!isColdSample(d)) continue;
      const key = `${d.arch}|${d.scenario}|${d.jitter}|${d.memory_mb}`;
      let g = jitMap.get(key);
      if (!g) {
        g = {
          arch: d.arch,
          scenario: d.scenario,
          jitter: d.jitter,
          memory_mb: d.memory_mb,
          _init: [],
          _first: [],
          _total: [],
        };
        jitMap.set(key, g);
      }
      g._init.push(coldMarkerMs(d));
      g._first.push(d.duration_ms);
      g._total.push(coldTotal(d));
    }
    jitterCells = [...jitMap.values()].map((g) => ({
      arch: g.arch,
      scenario: g.scenario,
      jitter: g.jitter,
      memory_mb: g.memory_mb,
      initP50: median(g._init),
      firstReqP50: median(g._first),
      totalP50: median(g._total),
    }));
  }

  // ---- SnapStart A/B cells -------------------------------------------------
  // Per (rawLang, arch, scenario, snapstart, memory): total cold-start P50
  // (marker + 1st req). The A/B compares plain vs SnapStart within one runtime,
  // so cells carry the raw runtime (`lang`) and are emitted only for runtimes
  // that ran a SnapStart variant, since plain rows alone never pair up.
  let snapCells = [];
  if (hasSnapStart) {
    const snapLangs = new Set(
      rows.filter((d) => d.snapstart).map((d) => d.lang),
    );
    const snapMap = new Map();
    for (const d of rows) {
      if (!snapLangs.has(d.lang) || !isColdSample(d)) continue;
      const key = `${d.lang}|${d.arch}|${d.scenario}|${d.snapstart}|${d.memory_mb}`;
      let g = snapMap.get(key);
      if (!g) {
        g = {
          lang: d.lang,
          arch: d.arch,
          scenario: d.scenario,
          snapstart: d.snapstart,
          memory_mb: d.memory_mb,
          _v: [],
        };
        snapMap.set(key, g);
      }
      g._v.push(coldMarkerMs(d) + d.duration_ms);
    }
    snapCells = [...snapMap.values()].map((g) => ({
      lang: g.lang,
      arch: g.arch,
      scenario: g.scenario,
      snapstart: g.snapstart,
      memory_mb: g.memory_mb,
      coldP50: median(g._v),
    }));
  }

  // ---- Artifact sizes (one per language × scenario) ------------------------
  // Representative build (arm64 when present, o3 for Rust). A SnapStart
  // pseudo-language ships its base runtime's artifact, so no `<runtime>-snapstart`
  // key is emitted here.
  const sizeArch = architectures.includes("arm64") ? "arm64" : architectures[0];
  const artifacts = [];
  {
    for (const d of rows) {
      if (!isRepresentative(d)) continue;
      if (!(d.artifact_unzipped_bytes || d.artifact_zip_bytes)) continue;
      const lang = langKey(d);
      if (isSnapLang(lang)) continue;
      const key = `${lang}|${d.scenario}`;
      // Prefer the representative arch; only overwrite a non-preferred earlier pick.
      const existing = artifacts.find((a) => a._key === key);
      if (existing && existing.arch === sizeArch) continue;
      const rec = {
        _key: key,
        lang,
        scenario: d.scenario,
        arch: d.arch,
        unzipped: d.artifact_unzipped_bytes || null,
        zip: d.artifact_zip_bytes || null,
      };
      if (existing) {
        Object.assign(existing, rec);
      } else {
        artifacts.push(rec);
      }
    }
    for (const a of artifacts) delete a._key;
  }
  const hasUnzippedSize = artifacts.some((a) => a.unzipped);

  // ---- Distribution points (pre-sampled per memory tier) -------------------
  // One dot per invocation would bloat the payload to tens of MB; sampleGroup
  // thins each group. Columnar shape: one {series, scenario, kind} group with a
  // flat `values` array, not a labeled object per dot. Repeating the three labels
  // on ~180K points would dominate the payload (~90% of each record); carrying
  // them once per group shrinks the JSON ~5x. The consumer re-expands to per-dot
  // records for Plot (see components/charts.js distribution()).
  const dist = {}; // memory_mb -> [{series, scenario, kind, values}]; medians from FULL data
  const distMedians = {}; // memory_mb -> [{series, scenario, kind, median}]
  {
    // Bucket every representative cold/warm sample by memory tier and
    // `series|scenario|kind` in one pass, not once per memory tier.
    const byMem = new Map(); // mem -> Map(`series|scenario|kind` -> values[])
    const push = (mem, series, scenario, kind, value) => {
      let groups = byMem.get(mem);
      if (!groups) byMem.set(mem, (groups = new Map()));
      const k = `${series}|${scenario}|${kind}`;
      let arr = groups.get(k);
      if (!arr) groups.set(k, (arr = []));
      arr.push(value);
    };
    for (const d of rows) {
      if (!isRepresentative(d)) continue;
      if (isColdSample(d))
        push(d.memory_mb, seriesOf(d), d.scenario, "cold", coldTotal(d));
      else if (!d.is_cold)
        push(d.memory_mb, seriesOf(d), d.scenario, "warm", d.duration_ms);
    }
    for (const mem of memories) {
      const groups = byMem.get(mem) ?? new Map();
      const sampled = [];
      const medians = [];
      for (const [k, values] of groups) {
        const [series, scenario, kind] = k.split("|");
        // cleanSort drops null/NaN before quantile (a stray non-finite would
        // sort unpredictably and corrupt the median).
        const vals = cleanSort(values);
        // quantile([]) is NaN, but median()/summarizeValues() return null for an
        // empty set; emit null here too so a consumer sees a missing tick, not a
        // NaN. Not reachable today (the gates above ensure n>=1), but keeps the
        // contract consistent if a future dist `kind` bypasses them.
        medians.push({
          series,
          scenario,
          kind,
          median: vals.length ? quantile(vals, 0.5) : null,
          n: vals.length,
        });
        sampled.push({ series, scenario, kind, values: sampleGroup(values) });
      }
      dist[mem] = sampled;
      distMedians[mem] = medians;
    }
  }

  // ---- KPIs ----------------------------------------------------------------
  // Cold-start P50 range across all representative cells: lightest to heaviest,
  // naming no series. Uses the cold total (`cold` = init/restore + first
  // request), the laziness-neutral metric the headline chart plots, not
  // `coldInit` alone, which rewards runtimes that defer setup to the first
  // invocation (see DESIGN.md #3).
  let coldMin = null;
  let coldMax = null;
  for (const c of cells) {
    if (!c.cold) continue;
    if (coldMin == null || c.cold.p50 < coldMin) coldMin = c.cold.p50;
    if (coldMax == null || c.cold.p50 > coldMax) coldMax = c.cold.p50;
  }
  const kpiColdRange = coldMin == null ? null : { min: coldMin, max: coldMax };
  const totalInvocations = rows.length;
  const coldCount = rows.filter((r) => r.is_cold).length;

  // ---- Assemble payload ----------------------------------------------------
  return {
    meta: {
      // No account_id: this payload ships to a public site. `region` is public
      // and `run_id` is a timestamp+random hash with no account information.
      run_id: meta.run_id ?? null,
      // Wall-clock bounds (UTC unix ms), so the site can show when the data was
      // collected without parsing run_id. A full run spans hours and can straddle
      // UTC days; the start is the canonical "collected on" date.
      started_at_unix_ms: meta.started_at_unix_ms ?? null,
      finished_at_unix_ms: meta.finished_at_unix_ms ?? null,
      region: meta.region ?? null,
      // The iteration-count profile (`full` / `smoke`). Per-cell sample counts
      // vary by scenario/dimension and are not carried here: the pages cite the
      // actual per-cell `n` from the data (each cell's `warm.n` / `cold.n`), and
      // the resolved breakdown lives in the on-disk meta's `iteration_buckets`.
      // null on older metas.
      profile: meta.profile ?? null,
      total_functions: meta.total_functions ?? null,
      // Completeness signals, validated as a hard gate in assertRunComplete.
      // Carried through so a consumer can display "complete dataset"
      // provenance; null on older metas that predate these fields.
      status: meta.status ?? null,
      total_invocations_planned: meta.total_invocations_planned ?? null,
      total_invocations_recorded: meta.total_invocations_recorded ?? null,
      build_tools: meta.build_manifest?.tools ?? null,
      input_basename: inputBasename,
    },
    dimensions: {
      languages,
      rawLanguages,
      architectures,
      scenarios,
      memories,
      pivotMem,
      hasOpt,
      hasSnapStart,
      hasJitter,
      sizeArch,
      hasUnzippedSize,
    },
    kpi: {
      coldRange: kpiColdRange,
      totalInvocations,
      coldCount,
      warmCount: totalInvocations - coldCount,
    },
    cells,
    optCells,
    snapCells,
    jitterCells,
    artifacts,
    // The distribution scatter points are the bulk of the payload, stored
    // columnarly (see the dist section above). Kept in the one payload, which the
    // browser HTTP-caches across navigations, to avoid streaming the 1.4M-row
    // source twice.
    dist,
    distMedians,
  };
}
