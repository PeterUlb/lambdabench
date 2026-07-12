// Client-side chart builders. Each takes a view (see `makeView`) and returns a
// DOM node rendered with Observable Plot, re-rendering reactively on filter
// changes without a rebuild.
//
// A chart never re-derives "what is selected". `makeView(stats, sel)` applies
// the selection once and returns the filtered dimensions, cells, color scale,
// and memory-axis builder, all mutually consistent. Every builder reads from
// that one object, so it cannot filter the data but not the axis (or row order,
// or legend).

import * as Plot from "npm:@observablehq/plot";

import { THEME, plotBase, PAIRS } from "./theme.js";
import {
  colorModel,
  archPair,
  isSnapLang,
  snapLangsToShow,
} from "../lib/series.js";
import { median, midOf } from "../lib/stats.js";
import {
  fmtMs,
  fmtBytes,
  fmtUsd,
  langLabel,
  seriesLabel,
  scenarioLabel,
  shortScenario,
} from "../lib/format.js";

const frame = () => Plot.frame({ stroke: THEME.frame });

// ---- The view: selection applied exactly once ------------------------------
// `sel` carries the active languages / scenarios / architectures / memories.
// A series ("lang arch") is active when its language and arch are both active.
function selectedSeries(stats, sel) {
  return stats.dimensions.languages
    .filter((l) => sel.languages.includes(l))
    .flatMap((l) => sel.architectures.map((a) => `${l} ${a}`));
}

// Build the filtered view. Pages compute this once per render (reactive on
// `sel`) and pass it to every chart/table so all consumers see the same filtered
// world. `stats` stays on the view as an escape hatch for charts that need the
// full dataset (a fixed normalization baseline, or the A/B cells outside `cells`).
//
// `invalidation` is Framework's per-block disposal promise, threaded through so
// the ResizeObserver-driven wrappers disconnect when the reactive block re-runs
// (a filter toggle re-runs makeView and every chart cell, minting fresh
// wrappers/observers). Without it the old observers keep firing on detached nodes
// and pin the filtered dataset they close over. Optional so a non-reactive caller
// (or a test) can omit it.
export function makeView(stats, sel, invalidation) {
  const series = new Set(selectedSeries(stats, sel));
  const full = colorModel(
    stats.dimensions.languages,
    stats.dimensions.architectures,
  );
  // Color domain restricted to active series, so the legend never lists a hidden
  // language; colors stay stable regardless of which subset is shown.
  const domain = full.domain.filter((d) => series.has(d));
  const range = domain.map((d) => full.color[d]);

  const languages = stats.dimensions.languages.filter((l) =>
    sel.languages.includes(l),
  );
  const architectures = stats.dimensions.architectures.filter((a) =>
    sel.architectures.includes(a),
  );
  const scenarios = stats.dimensions.scenarios.filter((s) =>
    sel.scenarios.includes(s),
  );
  const memories = stats.dimensions.memories.filter((m) =>
    sel.memories.includes(m),
  );

  const cells = stats.cells.filter(
    (c) =>
      series.has(c.series) &&
      scenarios.includes(c.scenario) &&
      memories.includes(c.memory_mb),
  );

  // Memory axis whose domain and ticks follow the selected tiers, so unticking
  // the top/bottom tiers shrinks the axis instead of leaving empty space. A
  // narrow panel fits few ticks, so show smallest / a middle tier / largest.
  const memX = (extra = {}, mems = memories) => {
    if (mems.length === 0)
      return { type: "log", label: "Memory (MB) →", ...extra };
    const lo = mems[0];
    const hi = mems[mems.length - 1];
    const mid = mems.includes(stats.dimensions.pivotMem)
      ? stats.dimensions.pivotMem
      : midOf(mems);
    return {
      type: "log",
      domain: [lo * 0.88, hi * 1.12],
      ticks: [...new Set([lo, mid, hi])],
      tickFormat: (d) => `${d}`,
      label: "Memory (MB) →",
      ...extra,
    };
  };

  return {
    stats, // escape hatch: full dataset for baselines / optCells / snapCells / etc.
    invalidation, // disposal promise for the observer-driven wrappers (see observeWidth)
    series, // Set of active "lang arch"
    languages,
    architectures,
    scenarios,
    memories,
    cells, // already filtered by series × scenario × memory
    color: { domain, range, langColor: full.langColor, color: full.color },
    memX,
  };
}

// ---- Width observation with disposal ---------------------------------------
// Every responsive wrapper below is driven by a ResizeObserver that measures the
// real column width and hands each chart an explicit width (Plot renders a
// fixed-width SVG and cannot fill a flex/grid cell on its own).
//
// The single place that creates such an observer, so the observe/disconnect
// pairing cannot drift between the three wrappers. `onWidth(width)` is called
// with the content width on each change; when `invalidation` resolves (the block
// re-ran, so a fresh wrapper has replaced this one) the observer disconnects,
// otherwise the stale observer keeps firing on a detached node and pins the
// filtered dataset it closes over. Without ResizeObserver (SSR / no-DOM) or an
// element, `onWidth` is invoked once with `fallbackWidth` so a build renders.
function observeWidth(el, onWidth, { invalidation, fallbackWidth } = {}) {
  if (typeof ResizeObserver !== "function") {
    if (fallbackWidth != null) onWidth(fallbackWidth);
    return;
  }
  // Fires once soon after `el` is inserted, performing the initial render (`el`
  // has no width until it is in the DOM).
  const ro = new ResizeObserver((entries) =>
    onWidth(entries[0]?.contentRect?.width ?? 0),
  );
  ro.observe(el);
  // Disconnect when the owning block re-runs or the page navigates away.
  // `invalidation` is optional; omitting it keeps the observer always on.
  invalidation?.then(() => ro.disconnect());
}

// ---- Small-multiples layout ------------------------------------------------
// One panel per active scenario in a fluid grid: panels fill the container
// width, wrapping into as many columns as fit at >= SM_MIN_PANEL each, so the
// chart reads well from a phone (one column) to an ultrawide desktop (five-plus).
// Width observation + disposal is handled by observeWidth above.

// Minimum readable panel width (below this the memory ticks and y labels
// collide), the cap for a lone panel so a single scenario does not stretch
// across the column, and the gutter between panels. `maxPanel` is overridable
// per chart (the cache tail panel runs wider than a normal small multiple).
const SM_MIN_PANEL = 224;
const SM_MAX_PANEL = 520;
const SM_GAP = 10;

// `plotFor(scenario, isFirst, panelWidth)` returns Plot options (or null to skip
// an empty panel), receiving the measured width to size its SVG. Each panel is
// independently scaled unless a chart opts into a shared domain: a shared Y would
// crush cheap scenarios (warm hello ~1 ms) against expensive ones (~40 ms).
function smallMultiples(
  v,
  { plotFor, scenarios = v.scenarios, maxPanel = SM_MAX_PANEL },
) {
  const wrap = document.createElement("div");
  wrap.className = "sm-grid";

  // Re-render all panels at `containerWidth`, guarded on the derived panel width
  // so a resize that does not change the column layout is a no-op (the observer
  // fires on every pixel; only breakpoints matter).
  let lastPanelW = -1;
  const render = (containerWidth) => {
    const n = scenarios.length;
    if (containerWidth <= 0 || n === 0) return;
    const cols = Math.max(
      1,
      Math.min(
        n,
        Math.floor((containerWidth + SM_GAP) / (SM_MIN_PANEL + SM_GAP)),
      ),
    );
    const raw = (containerWidth - (cols - 1) * SM_GAP) / cols;
    const panelW = Math.floor(Math.min(raw, maxPanel));
    if (panelW === lastPanelW) return;
    lastPanelW = panelW;

    while (wrap.firstChild) wrap.firstChild.remove();
    scenarios.forEach((scenario, i) => {
      const opts = plotFor(scenario, i === 0, panelW);
      if (!opts) return;
      const panel = document.createElement("div");
      panel.className = "sm-panel";
      panel.style.width = `${panelW}px`;
      const title = document.createElement("div");
      title.className = "sm-title";
      title.textContent = scenarioLabel(scenario);
      panel.append(title, Plot.plot(opts));
      wrap.append(panel);
    });
  };

  observeWidth(wrap, render, {
    invalidation: v.invalidation,
    fallbackWidth: 864,
  });
  return wrap;
}

// ---- Fluid panel grid (arbitrary panel content) ---------------------------
// Like smallMultiples, but for panels whose body is not a single metric plot:
// the A/B dumbbell (one panel per language) and the jitter cliff (one per
// scenario, with a subtitle). Each item supplies a title, optional subtitle, and
// a `plot(width)` builder. Panels never shrink below `minPanel`: a dumbbell's row
// labels are fixed text, so on a narrower screen the grid renders at `minPanel`
// and the enclosing block scrolls it in-column rather than squashing the labels.
// `maxPanel` caps growth. `invalidation` is forwarded to observeWidth.
function panelGrid(items, { minPanel, maxPanel, invalidation }) {
  const wrap = document.createElement("div");
  // `pg-grid` (not `sm-grid`): same flex layout but keeps the default block
  // overflow-x:auto so a floored panel scrolls in-column, whereas `sm-grid`
  // blocks are forced overflow:visible for tips.
  wrap.className = "pg-grid";
  let lastPanelW = -1;
  const render = (containerWidth) => {
    if (containerWidth <= 0 || items.length === 0) return;
    const cols = Math.max(
      1,
      Math.min(
        items.length,
        Math.floor((containerWidth + SM_GAP) / (minPanel + SM_GAP)),
      ),
    );
    const raw = Math.floor((containerWidth - (cols - 1) * SM_GAP) / cols);
    // Clamp to [minPanel, maxPanel]; the floor forces the in-column scroll
    // instead of unreadable labels.
    const panelW = Math.max(minPanel, Math.min(raw, maxPanel));
    if (panelW === lastPanelW) return;
    lastPanelW = panelW;
    while (wrap.firstChild) wrap.firstChild.remove();
    for (const item of items) {
      const panel = document.createElement("div");
      panel.className = "sm-panel";
      panel.style.width = `${panelW}px`;
      const title = document.createElement("div");
      title.className = "sm-title";
      title.textContent = item.title;
      panel.append(title);
      if (item.subtitle != null) {
        const sub = document.createElement("div");
        sub.className = "sm-subtitle";
        sub.textContent = item.subtitle;
        panel.append(sub);
      }
      panel.append(Plot.plot(item.plot(panelW)));
      wrap.append(panel);
    }
  };
  observeWidth(wrap, render, { invalidation, fallbackWidth: maxPanel });
  return wrap;
}

// ---- Responsive single (wide) plot -----------------------------------------
// A ResizeObserver-driven wrapper for the one-plot charts (cold breakdown,
// artifact size, distribution scatter, A/B dumbbells). `build(width)` returns
// Plot options for a render width; the wrapper measures the container and
// re-renders at a width clamped to [minWidth, maxWidth]. When the content needs
// more than the container offers, it renders at `minWidth` and the enclosing
// `.observablehq--block { overflow-x: auto }` scrolls it in-column, so the page
// never gains a horizontal scrollbar. `maxWidth` caps growth on an ultrawide
// monitor. `invalidation` is forwarded to observeWidth.
function responsivePlot({
  build,
  minWidth = 320,
  maxWidth = 1180,
  invalidation,
}) {
  const wrap = document.createElement("div");
  wrap.className = "rp-wrap";
  let lastW = -1;
  const render = (containerWidth) => {
    if (containerWidth <= 0) return;
    const w = Math.round(
      Math.max(minWidth, Math.min(containerWidth, maxWidth)),
    );
    if (w === lastW) return;
    lastW = w;
    const opts = build(w);
    while (wrap.firstChild) wrap.firstChild.remove();
    if (opts) wrap.append(Plot.plot(opts));
  };
  observeWidth(wrap, render, { invalidation, fallbackWidth: maxWidth });
  return wrap;
}

// Standalone legend (Plot's built-in is per-plot; small multiples share one),
// honoring the active series order/colors. The label humanizes the language via
// langLabel ("java-snapstart" -> "Java SnapStart") so it matches the tables,
// tooltips, and prose rather than leaking the `-snapstart` encoding.
export function legend(v) {
  const pairs = v.color.domain.map((name, i) => {
    const sep = name.lastIndexOf(" ");
    const label =
      sep === -1
        ? langLabel(name)
        : `${langLabel(name.slice(0, sep))} ${name.slice(sep + 1)}`;
    return { label, color: v.color.range[i] };
  });
  return swatchLegend(pairs);
}

// Build a swatch legend from explicit {label, color} pairs, in the same style as
// legend(v). The shared replacement for Plot's built-in `legend: true`, whose
// raw-key styling differs from the site's humanized-label legend. Charts whose
// legend is not the series color scale (init/first-request, cold/warm,
// per-language artifacts, the dumbbell keys) build their pairs and pass them here.
function swatchLegend(pairs) {
  const el = document.createElement("div");
  el.className = "sm-legend";
  for (const { label, color } of pairs) {
    const item = document.createElement("span");
    item.className = "lg-item";
    item.innerHTML = `<span class="lg-swatch" style="background:${color}"></span>${label}`;
    el.append(item);
  }
  return el;
}

// Stack a legend above a chart node in one wrapper, so a single display() (or
// template interpolation) renders both together. Used by the charts that build
// their own swatch legend instead of relying on a separate display(C.legend(v)).
function withLegend(legendEl, chartEl) {
  const wrap = document.createElement("div");
  wrap.append(legendEl, chartEl);
  return wrap;
}

// The init / first-request cold-segment legend, shared by the cold-breakdown and
// jitter-cliff bar charts (both split a cold start into these two segments with
// the same PAIRS.initFirst colors).
const initFirstLegend = () =>
  swatchLegend([
    { label: "init", color: PAIRS.initFirst[0] },
    { label: "first request", color: PAIRS.initFirst[1] },
  ]);

const lineBandMarks = (data, yField) => [
  frame(),
  Plot.areaY(data, {
    x: "memory_mb",
    y1: (d) => d[yField]?.p10,
    y2: (d) => d[yField]?.p90,
    fill: "series",
    fillOpacity: 0.13,
    curve: "monotone-x",
    z: "series",
  }),
  Plot.lineY(data, {
    x: "memory_mb",
    y: (d) => d[yField]?.p50,
    stroke: "series",
    z: "series",
    strokeWidth: 2.5,
    curve: "monotone-x",
  }),
  Plot.dot(data, {
    x: "memory_mb",
    y: (d) => d[yField]?.p50,
    fill: "series",
    r: 3,
    channels: { series: "series" },
    tip: true,
    title: (d) => {
      const s = d[yField];
      // P99 is null on cold cells (n ≤ 50, below the reporting gate in
      // lib/stats.js); omit the term rather than print "P99 —".
      const pctl = [`P50 ${fmtMs(s?.p50)}`, `P90 ${fmtMs(s?.p90)}`];
      if (s?.p99 != null) pctl.push(`P99 ${fmtMs(s.p99)}`);
      return `${seriesLabel(d.series)} · ${scenarioLabel(d.scenario)} @ ${d.memory_mb}MB\n${pctl.join(" · ")}\nmin ${fmtMs(
        s?.min,
      )} · max ${fmtMs(s?.max)} · n=${s?.n}`;
    },
  }),
];

// Left margin sized to the widest tick label so values in the thousands aren't
// clipped while cheap panels don't waste space.
function panelMarginLeft(maxValue, isFirst, format) {
  const fmt =
    format ??
    ((v) =>
      maxValue < 10 ? v.toFixed(1) : Math.round(v).toLocaleString("en-US"));
  const chars = Math.max(1, fmt(maxValue || 0).length);
  return (isFirst ? 22 : 0) + 16 + chars * 7;
}

// ---- Generic metric small-multiples ----------------------------------------
// One independently-scaled panel per scenario, sharing the panel scaffold (size,
// memory x-axis, color scale, left-margin sizing). Consumers differ only in the
// cell sub-object read (`field`), the per-panel y-max (`yMax`), the y-axis
// label/tick format, and the marks, so cold/warm/footprint/tail/cost stay
// structurally identical.
function metricSmallMultiples(
  v,
  {
    field,
    yMax,
    yLabel,
    marks,
    height = 320,
    tickFormat,
    headroom = 1,
    scenarios,
    maxPanel,
    log = false,
    scopeXToData = false,
  },
) {
  return smallMultiples(v, {
    scenarios,
    maxPanel,
    plotFor: (scenario, isFirst, panelWidth) => {
      const data = v.cells.filter((c) => c.scenario === scenario && c[field]);
      if (!data.length) return null;
      // Some scenarios have a memory floor (e.g. `cache` >= 512 MB), so the full
      // selected x-domain leaves the sub-floor tiers as dead space.
      // `scopeXToData` rebuilds the x-axis from this panel's present tiers.
      const panelMemX = scopeXToData
        ? v.memX(
            { label: null },
            [...new Set(data.map((d) => d.memory_mb))].sort((a, b) => a - b),
          )
        : v.memX({ label: null });
      // `headroom` lifts the y-domain top above the tallest value. Charts whose
      // max-driving value is a line (e.g. the tail P99.9) need it so the peak is
      // not drawn flush against the frame and clipped; band charts leave it at 1.
      const max = Math.max(...data.map(yMax), 0) * headroom;
      // Log y-axis for metrics spanning orders of magnitude across series (the
      // warm tail: a few ms to ~1 s in one panel), where a zero-based linear
      // scale crushes fast series into a sliver. Log cannot include zero, so it
      // drops the `zero`/`domain:[0,max]` pinning and lets Plot frame the data.
      const yScale = log
        ? {
            label: isFirst ? yLabel : null,
            labelAnchor: "center",
            grid: true,
            type: "log",
            tickFormat,
          }
        : {
            label: isFirst ? yLabel : null,
            labelAnchor: "center",
            grid: true,
            zero: true,
            tickFormat,
            // Pin the domain only with headroom requested and a positive max;
            // else leave it to Plot's auto-scaling. Guarding on `max > 0` avoids
            // collapsing to [0,0] when every visible series has a null tail
            // percentile (`yMax` returns null -> 0).
            ...(headroom !== 1 && max > 0 ? { domain: [0, max] } : {}),
          };
      return {
        ...plotBase,
        width: panelWidth,
        height,
        marginLeft: panelMarginLeft(max, isFirst, tickFormat),
        marginBottom: 50,
        marginTop: 16,
        x: panelMemX,
        y: yScale,
        color: { domain: v.color.domain, range: v.color.range },
        marks: marks(data),
      };
    },
  });
}

// ---- Headline: cold / warm / footprint vs memory ---------------------------
const lineBandSmallMultiples = (v, { yField, yLabel }) =>
  metricSmallMultiples(v, {
    field: yField,
    yMax: (d) => d[yField].p90,
    yLabel,
    marks: (data) => lineBandMarks(data, yField),
  });

export const coldVsMemory = (v) =>
  lineBandSmallMultiples(v, {
    yField: "cold",
    yLabel: "Cold start total (ms)",
  });
export const warmVsMemory = (v) =>
  lineBandSmallMultiples(v, { yField: "warm", yLabel: "Warm P50 (ms)" });
export const footprintVsMemory = (v) =>
  lineBandSmallMultiples(v, {
    yField: "footprint",
    yLabel: "Max memory used (MB)",
  });

// ---- Tail latency (P99 solid, P99.9 dashed), warm --------------------------
export const tailLatency = (v) =>
  metricSmallMultiples(v, {
    field: "warm",
    // P99.9 is the tallest mark but null below the n>=1000 threshold (e.g.
    // low-sample SnapStart cells). Fall back to P99 so the y-domain is always
    // driven by a real value, else a panel with every P99.9 null collapses to
    // [0,0] and draws the P99 line against a zero-height axis.
    yMax: (d) => d.warm.p999 ?? d.warm.p99,
    yLabel: "Warm latency (ms)",
    height: 300,
    // 5% headroom or the tallest (P99.9) peak sits flush on the top frame.
    headroom: 1.05,
    marks: (data) => [
      frame(),
      Plot.lineY(data, {
        x: "memory_mb",
        y: (d) => d.warm.p99,
        stroke: "series",
        z: "series",
        strokeWidth: 2,
        curve: "monotone-x",
      }),
      Plot.lineY(data, {
        x: "memory_mb",
        y: (d) => d.warm.p999,
        stroke: "series",
        z: "series",
        strokeWidth: 1.25,
        strokeDasharray: "4,3",
        curve: "monotone-x",
      }),
      Plot.dot(data, {
        x: "memory_mb",
        y: (d) => d.warm.p99,
        fill: "series",
        r: 2.5,
        channels: { series: "series" },
        tip: true,
        title: (d) =>
          `${seriesLabel(d.series)} · ${scenarioLabel(d.scenario)} @ ${d.memory_mb}MB\nP99 ${fmtMs(d.warm.p99)} · P99.9 ${fmtMs(d.warm.p999)} · max ${fmtMs(d.warm.max)}`,
      }),
    ],
  });

// ---- Warm tail vs median: where the P99 pulls away -------------------------
// Scoped to `cache`, the one scenario whose tail separation is cleanly a tracing
// GC's signature (a large retained live heap, re-traced every invoke, on a
// CPU-starved tier). The headline charts can't show this on one panel: the
// warm-vs-memory band caps at P90 (clipping the stalls) and the tail chart draws
// P99/P99.9 with no median baseline, so a high tail is indistinguishable from a
// uniformly slow runtime. Here both share a panel: solid = warm P50 (baseline),
// shaded = P10-P90 (body), dashed = P99 (the stall tail). The solid-to-dashed
// gap is the tail-over-median spread; on `cache` it widens at low memory on the
// GC'd runtimes and stays pinned on the non-GC one (Rust).
//
// Not scoped to `batch`: its warm tail sits only modestly above its median on
// every runtime including Rust, so it is uniform jitter on a slow median, not a
// GC signature (README: batch's GC tail is "secondary"). Including it would
// invite the high-tail-means-GC overread this chart avoids.
//
// Log y-axis: cache warm P99 spans ~5 ms (Rust) to ~970 ms (Java) in one panel,
// which a zero-based linear scale would flatten into a sliver. The cause (GC) is
// asserted in prose only; the chart shows tail-vs-median behavior, not a cause.
const TAIL_SPREAD_SCENARIOS = ["cache"];
export const tailVsMedian = (v) => {
  const scenarios = TAIL_SPREAD_SCENARIOS.filter((s) =>
    v.scenarios.includes(s),
  );
  // When `cache` is filtered out, show a note rather than returning null (which
  // display() renders as the literal string "null").
  if (scenarios.length === 0) {
    const note = document.createElement("div");
    note.className = "sm-empty";
    note.textContent = "Select the cache scenario to see this panel.";
    return note;
  }
  return metricSmallMultiples(v, {
    field: "warm",
    scenarios,
    yMax: (d) => d.warm.p99,
    yLabel: "Warm latency (ms, log)",
    height: 340,
    // Only `cache` is shown, so let it run wider than a normal small multiple.
    maxPanel: 760,
    log: true,
    // `cache` is floored at 512 MB; scope the x-axis to its present tiers so the
    // lines fill the panel.
    scopeXToData: true,
    marks: (data) => [
      frame(),
      Plot.areaY(data, {
        x: "memory_mb",
        y1: (d) => d.warm.p10,
        y2: (d) => d.warm.p90,
        fill: "series",
        fillOpacity: 0.13,
        curve: "monotone-x",
        z: "series",
      }),
      Plot.lineY(data, {
        x: "memory_mb",
        y: (d) => d.warm.p99,
        stroke: "series",
        z: "series",
        strokeWidth: 1.25,
        strokeDasharray: "4,3",
        curve: "monotone-x",
      }),
      Plot.lineY(data, {
        x: "memory_mb",
        y: (d) => d.warm.p50,
        stroke: "series",
        z: "series",
        strokeWidth: 2.5,
        curve: "monotone-x",
      }),
      Plot.dot(data, {
        x: "memory_mb",
        y: (d) => d.warm.p50,
        fill: "series",
        r: 3,
        channels: { series: "series" },
        tip: true,
        title: (d) => {
          // P99 is null below the n>=200 threshold; guard the ratio/delta so a
          // low-sample cell shows "—" rather than "NaNx" / "NaNms".
          const ratio =
            d.warm.p99 != null
              ? `${(d.warm.p99 / d.warm.p50).toFixed(1)}x`
              : "—";
          const delta =
            d.warm.p99 != null ? fmtMs(d.warm.p99 - d.warm.p50) : "—";
          return `${seriesLabel(d.series)} · ${scenarioLabel(d.scenario)} @ ${d.memory_mb}MB\nP50 ${fmtMs(d.warm.p50)} · P90 ${fmtMs(
            d.warm.p90,
          )} · P99 ${fmtMs(d.warm.p99)}\nP99 ÷ P50 ${ratio} · P99 − P50 ${delta} · n=${d.warm.n}`;
        },
      }),
    ],
  });
};

// ---- Warm CPU-sensitivity (small multiples, shared Y) ----------------------
// One panel per scenario over the memory axis, so the layout and legend match
// the other headline charts. The y-axis is shared across panels: the chart
// compares how far each runtime climbs off its 1.0 floor at low memory, which a
// per-panel scale would destroy by rescaling each panel to its own peak.
export function cpuSensitivity(v) {
  const m = v.stats.dimensions.memories;
  const maxMem = m[m.length - 1];
  // Baseline is the max-memory P50, a fixed normalization reference, read from
  // the full dataset so unticking the top memory tier does not erase it.
  const baseline = new Map();
  for (const c of v.stats.cells) {
    if (c.memory_mb === maxMem && c.warm)
      baseline.set(`${c.series}|${c.scenario}`, c.warm.p50);
  }
  const norm = v.cells
    .filter((c) => c.warm)
    .map((c) => {
      const b = baseline.get(`${c.series}|${c.scenario}`);
      return b
        ? {
            series: c.series,
            scenario: c.scenario,
            memory_mb: c.memory_mb,
            ratio: c.warm.p50 / b,
            p50: c.warm.p50,
          }
        : null;
    })
    .filter(Boolean);
  // Shared y-domain from the global max ratio, with headroom so the tallest
  // climb is not flush against the frame.
  const yMax = Math.max(1, ...norm.map((d) => d.ratio)) * 1.05;
  // Data-driven lower bound, not a fixed floor: a series can measure slightly
  // faster at an intermediate tier than at max memory (ratio < 1) from jitter,
  // and on this log scale a hard floor would clip such a point off the axis (Plot
  // drops out-of-domain points). Anchor at 0.9 but drop below it when data does.
  const yMin = Math.min(0.9, ...norm.map((d) => d.ratio)) * 0.98;
  const byScenario = new Map();
  for (const d of norm) {
    if (!byScenario.has(d.scenario)) byScenario.set(d.scenario, []);
    byScenario.get(d.scenario).push(d);
  }
  return smallMultiples(v, {
    plotFor: (scenario, isFirst, panelWidth) => {
      const data = byScenario.get(scenario);
      if (!data || !data.length) return null;
      return {
        ...plotBase,
        width: panelWidth,
        height: 320,
        marginLeft: panelMarginLeft(yMax, isFirst),
        marginBottom: 50,
        marginTop: 16,
        x: v.memX({ label: null }),
        y: {
          label: isFirst ? "↑ Warm P50 ÷ P50 at max memory (log)" : null,
          labelAnchor: "center",
          grid: true,
          type: "log",
          domain: [yMin, yMax],
        },
        color: { domain: v.color.domain, range: v.color.range },
        marks: [
          frame(),
          Plot.ruleY([1], {
            stroke: THEME.frame,
            strokeDasharray: "3,3",
            strokeOpacity: 0.6,
          }),
          Plot.lineY(data, {
            x: "memory_mb",
            y: "ratio",
            stroke: "series",
            z: "series",
            strokeWidth: 2.5,
            curve: "monotone-x",
          }),
          Plot.dot(data, {
            x: "memory_mb",
            y: "ratio",
            fill: "series",
            r: 3.5,
            channels: { series: "series" },
            tip: true,
            title: (d) =>
              `${seriesLabel(d.series)} · ${scenarioLabel(d.scenario)} @ ${d.memory_mb}MB\n${d.ratio.toFixed(2)}x its max-memory P50 latency\n(P50 ${fmtMs(d.p50)})`,
          }),
        ],
      };
    },
  });
}

// ---- Cost frontier ($ / 1M warm invokes) -----------------------------------
export function costPerMillion(v) {
  // Two decimals throughout (not 1 above $1): the left margin is sized from the
  // max value's formatted width (see panelMarginLeft), so a sub-$1 tick like
  // "$0.83" must not be wider than the max label, or it clips.
  const costTick = (x) => `$${x.toFixed(2)}`;
  return metricSmallMultiples(v, {
    field: "cost",
    yMax: (d) => d.cost.mean,
    yLabel: "↑ $ / 1M invokes (mean)",
    tickFormat: costTick,
    marks: (data) => [
      frame(),
      Plot.lineY(data, {
        x: "memory_mb",
        y: (d) => d.cost.mean,
        stroke: "series",
        z: "series",
        strokeWidth: 2.5,
        curve: "monotone-x",
      }),
      Plot.dot(data, {
        x: "memory_mb",
        y: (d) => d.cost.mean,
        fill: "series",
        r: 3.5,
        channels: { series: "series" },
        tip: true,
        title: (d) =>
          `${seriesLabel(d.series)} · ${scenarioLabel(d.scenario)} @ ${d.memory_mb}MB\n${fmtUsd(d.cost.mean)} per 1M invokes (mean)\n${fmtUsd(d.cost.p50)} per 1M invokes at P50`,
      }),
    ],
  });
}

// ---- Cold breakdown: init vs first-request, at one memory tier -------------
export function coldBreakdown(v) {
  // Single-tier view. Prefer the pivot tier, falling back to the largest
  // selected tier if it is unticked, so the chart follows the memory filter.
  if (!v.memories.length) return document.createElement("div");
  const pivot = v.memories.includes(v.stats.dimensions.pivotMem)
    ? v.stats.dimensions.pivotMem
    : v.memories[v.memories.length - 1];
  // Split cold latency into init + first-request. Both segments are true P50s of
  // their own raw samples (`coldInit.p50`, `coldFirstReq.p50`), not the
  // non-additive `cold.p50 - coldInit.p50` difference. The bar total is the sum
  // of the two segment P50s, which is not the cold-total P50 (`cold.p50`) since
  // percentiles are not additive; the total tick is labelled as the summed
  // segments.
  const breakdown = [];
  const rowOrder = [];
  for (const scenario of v.scenarios) {
    for (const lang of v.languages) {
      // Aggregate across the selected architectures for this lang at the pivot.
      const cs = v.cells.filter(
        (c) =>
          c.lang === lang &&
          c.scenario === scenario &&
          c.memory_mb === pivot &&
          c.coldInit &&
          c.coldFirstReq,
      );
      if (!cs.length) continue;
      // Median across the selected arches of each segment's per-cell P50 (the
      // exact per-cell P50 with one arch selected). Shared NaN-safe median (R-7,
      // the same convention as d3.quantile).
      const init = median(cs.map((c) => c.coldInit.p50));
      const first = median(cs.map((c) => c.coldFirstReq.p50));
      const row = `${shortScenario(scenario)} · ${langLabel(lang)}`;
      rowOrder.push(row);
      breakdown.push({
        row,
        part: "init",
        order: 0,
        ms: init,
        total: init + first,
      });
      breakdown.push({
        row,
        part: "first request",
        order: 1,
        ms: first,
        total: init + first,
      });
    }
  }
  if (!breakdown.length) return document.createElement("div");
  // Fluid width with a fixed 220px label margin and 26px row height. Floors at
  // 480 (below that the bars are too short to read the init/first split) and
  // scrolls in-column on narrower screens.
  const chart = responsivePlot({
    invalidation: v.invalidation,
    minWidth: 480,
    build: (width) => ({
      ...plotBase,
      width,
      height: 80 + rowOrder.length * 26,
      marginLeft: 220,
      marginRight: 40,
      marginTop: 20,
      marginBottom: 44,
      x: {
        label: `Cold latency P50 (ms) @ ${pivot}MB →`,
        grid: true,
        zero: true,
      },
      y: { domain: rowOrder, label: null, tickSize: 0 },
      color: { domain: ["init", "first request"], range: PAIRS.initFirst },
      marks: [
        frame(),
        Plot.barX(breakdown, {
          y: "row",
          x: "ms",
          fill: "part",
          order: "order",
          tip: true,
          title: (d) =>
            `${d.row}\n${d.part}: ${fmtMs(d.ms)}\nsum of segment P50s: ${fmtMs(d.total)}`,
        }),
        Plot.text(
          breakdown.filter((d) => d.part === "first request"),
          {
            y: "row",
            x: "total",
            text: (d) => ` ${fmtMs(d.total)}`,
            textAnchor: "start",
            fill: THEME.dim,
            fontSize: 11,
          },
        ),
      ],
    }),
  });
  return withLegend(initFirstLegend(), chart);
}

// ---- Distribution scatter (one memory tier) --------------------------------
// `memory` is a single tier chosen by the appendix's own radio (independent of
// the memory filter, which is a per-tier picker by design).
export function distribution(v, memory) {
  const series = v.series;
  // `dist` is columnar: each group carries a flat `values` array (labels stored
  // once per group, not per dot; see data/stats.json.js). Re-expand the in-view
  // groups to per-dot records for Plot.dot.
  const pts = (v.stats.dist[memory] ?? [])
    .filter((g) => series.has(g.series) && v.scenarios.includes(g.scenario))
    .flatMap((g) =>
      g.values.map((value) => ({
        series: g.series,
        scenario: g.scenario,
        kind: g.kind,
        value,
      })),
    );
  const medians = (v.stats.distMedians[memory] ?? []).filter(
    (p) => series.has(p.series) && v.scenarios.includes(p.scenario),
  );
  // No points for this tier + filters: show a note rather than an empty framed
  // plot with a degenerate x-domain.
  if (!pts.length) {
    const note = document.createElement("div");
    note.className = "sm-empty";
    note.textContent = `No distribution samples at ${memory} MB for the current selection.`;
    return note;
  }
  const seriesDomain = v.color.domain;
  const height = 90 + v.scenarios.length * seriesDomain.length * 16;
  const chart = responsivePlot({
    invalidation: v.invalidation,
    minWidth: 460,
    build: (width) => ({
      ...plotBase,
      width,
      height,
      marginLeft: 150,
      marginRight: 80,
      marginBottom: 50,
      marginTop: 30,
      x: { label: "Latency (ms, √ scale) →", type: "sqrt", grid: true },
      y: { domain: seriesDomain, label: null },
      fy: {
        domain: v.scenarios,
        label: null,
        tickFormat: (s) => shortScenario(s),
      },
      color: { domain: ["cold", "warm"], range: PAIRS.coldWarm },
      marks: [
        frame(),
        // Low-opacity overlapping dots on each series' row centerline. No
        // vertical jitter (Plot's `dy` is a constant offset, not a channel);
        // density reads from opacity stacking in the saturated body.
        Plot.dot(pts, {
          fy: "scenario",
          y: "series",
          x: "value",
          fill: "kind",
          r: 1.4,
          fillOpacity: 0.32,
        }),
        Plot.tickX(medians, {
          fy: "scenario",
          y: "series",
          x: "median",
          stroke: "kind",
          strokeWidth: 2.5,
        }),
      ],
    }),
  });
  return withLegend(
    swatchLegend([
      { label: "cold", color: PAIRS.coldWarm[0] },
      { label: "warm", color: PAIRS.coldWarm[1] },
    ]),
    chart,
  );
}

// ---- Artifact size (faceted dots, log x) -----------------------------------
export function artifactSize(v) {
  // A SnapStart pseudo-language ships its base runtime's artifact, so every
  // `<runtime>-snapstart` key is dropped here (the base runtime carries it).
  const langs = v.languages.filter((l) => !isSnapLang(l));
  // Plot one physical quantity, chosen run-wide: unzipped on-disk size when the
  // run captured it, else the uploaded zip size. Mixing the two per-point would
  // put different quantities on one axis; the axis label names what is plotted.
  const useUnzipped = v.stats.dimensions.hasUnzippedSize;
  const sizeOf = (a) => (useUnzipped ? a.unzipped : a.zip);
  const sizeLabel = useUnzipped
    ? "Unzipped artifact size"
    : "Zip (uploaded) artifact size";
  const data = v.stats.artifacts
    .filter(
      (a) =>
        langs.includes(a.lang) &&
        v.scenarios.includes(a.scenario) &&
        sizeOf(a) != null,
    )
    .map((a) => ({
      series: langLabel(a.lang),
      scenario: a.scenario,
      scenarioLabel: scenarioLabel(a.scenario),
      arch: a.arch,
      unzipped: a.unzipped,
      zip: a.zip,
      size: sizeOf(a),
    }));
  if (!data.length) return document.createElement("div");
  const seriesDomain = langs.map(langLabel);
  const seriesRange = langs.map((l) => v.color.langColor[l]);
  // Log axis: a 0 (or negative) size collapses the domain bound to 0 and blanks
  // the chart, so the domain is derived only from positive values.
  const vals = data.map((d) => d.size).filter((s) => s > 0);
  if (!vals.length) return document.createElement("div");
  const longest = Math.max(...v.scenarios.map((s) => scenarioLabel(s).length));
  // Right margin is sized to the longest facet label, so it is dynamic. The
  // minWidth floor is derived from the margins (not a constant a long label
  // could overrun), or the inner width goes negative and Plot emits
  // `<rect width="-1">`.
  const marginLeft = 150;
  const marginRight = 24 + longest * 7;
  const chart = responsivePlot({
    invalidation: v.invalidation,
    minWidth: marginLeft + marginRight + 140,
    build: (width) => ({
      ...plotBase,
      width,
      height: 46 + v.scenarios.length * (seriesDomain.length * 24 + 36),
      marginLeft,
      marginRight,
      marginTop: 20,
      marginBottom: 46,
      x: {
        type: "log",
        domain: [Math.min(...vals) / 1.5, Math.max(...vals) * 1.8],
        label: `${sizeLabel} (log) →`,
        grid: true,
        tickFormat: fmtBytes,
      },
      y: { domain: seriesDomain, label: null, tickSize: 0 },
      fy: { domain: v.scenarios.map((s) => scenarioLabel(s)), label: null },
      color: { domain: seriesDomain, range: seriesRange },
      marks: [
        frame(),
        Plot.dot(data, {
          x: "size",
          y: "series",
          fy: "scenarioLabel",
          fill: "series",
          r: 5,
          tip: true,
          title: (d) =>
            `${d.series} · ${d.scenarioLabel} (${d.arch})\nunzipped ${fmtBytes(d.unzipped)} · zip ${fmtBytes(d.zip)} (uploaded)`,
        }),
        Plot.text(data, {
          x: "size",
          y: "series",
          fy: "scenarioLabel",
          text: (d) => fmtBytes(d.size),
          dx: 10,
          textAnchor: "start",
          fill: THEME.dim,
          fontSize: 10,
        }),
      ],
    }),
  });
  // Per-language legend (one artifact per language, not per series).
  return withLegend(
    swatchLegend(
      seriesDomain.map((label, i) => ({ label, color: seriesRange[i] })),
    ),
    chart,
  );
}

// ---- Synthetic download-scaling probe --------------------------------------
// Reads the standalone probe JSON directly (passed as `data`, not via v.stats),
// since this is an off-matrix probe. Plots residual (download + environment start,
// ms) against artifact size on a log x-axis, one series per runtime family
// (python = managed, rust = custom provided.al2023): a median line + dot per
// family with a min-max band. Two families on the same curve confirms the
// download term is family-independent. `data.samples` items are
// {family, size_mb, residual_p50, residual_min, residual_max, zip_bytes, ...}.
export function syntheticDownloadScaling(data, invalidation) {
  const all = (data?.samples ?? [])
    .filter((s) => s.size_mb > 0)
    .slice()
    .sort((a, b) => a.size_mb - b.size_mb);
  if (!all.length) {
    const el = document.createElement("p");
    el.className = "caption";
    el.textContent =
      "No synthetic download-scaling samples yet. Run `bencher probe download-scaling`.";
    return el;
  }
  // Family -> color, matching the site's language hues (rust orange, python
  // blue); falls back to the accent for other/legacy single-series data.
  const FAMILY_COLOR = { rust: "#ea9953", python: "#539eea" };
  const families = [...new Set(all.map((s) => s.family ?? "python"))];
  const colorOf = (f) => FAMILY_COLOR[f] ?? THEME.accent;
  const familyLabel = (f) =>
    f === "rust"
      ? "Rust (custom runtime)"
      : f === "python"
        ? "Python (managed)"
        : f;
  const sizes = [...new Set(all.map((s) => s.size_mb))].sort((a, b) => a - b);
  const chart = responsivePlot({
    invalidation,
    minWidth: 320,
    maxWidth: 760,
    build: (width) => ({
      ...plotBase,
      width,
      height: 360,
      marginLeft: 56,
      marginRight: 24,
      marginTop: 20,
      marginBottom: 46,
      x: {
        type: "log",
        domain: [Math.min(...sizes) / 1.4, Math.max(...sizes) * 1.4],
        ticks: sizes,
        tickFormat: (d) => `${d} MB`,
        label: "Deployed artifact size (log) →",
        grid: true,
      },
      y: {
        domain: [0, Math.max(...all.map((s) => s.residual_max)) * 1.08],
        label:
          "↑ ms (dashed: download + start residual · solid: Init Duration)",
        grid: true,
      },
      color: {
        domain: families.map(familyLabel),
        range: families.map(colorOf),
      },
      marks: [
        frame(),
        ...families.flatMap((f) => {
          const s = all.filter((d) => (d.family ?? "python") === f);
          const c = colorOf(f);
          return [
            // Dashed = the unreported download + start residual (the claim's subject).
            Plot.areaY(s, {
              x: "size_mb",
              y1: "residual_min",
              y2: "residual_max",
              fill: c,
              fillOpacity: 0.1,
              curve: "monotone-x",
            }),
            Plot.lineY(s, {
              x: "size_mb",
              y: "residual_p50",
              stroke: c,
              strokeWidth: 2.5,
              strokeDasharray: "4 3",
              curve: "monotone-x",
            }),
            Plot.dot(s, { x: "size_mb", y: "residual_p50", fill: c, r: 4 }),
            // Solid = the reported Init Duration, plotted so its flatness against
            // the climbing residual is visible, not just asserted in prose.
            Plot.lineY(s, {
              x: "size_mb",
              y: "init_p50",
              stroke: c,
              strokeWidth: 2.5,
              curve: "monotone-x",
            }),
            Plot.dot(s, {
              x: "size_mb",
              y: "init_p50",
              fill: c,
              r: 3,
              symbol: "square",
            }),
          ];
        }),
        // One shared tip over every plotted point (see the dumbbell builder), so
        // the two families' near-overlapping lines can't fire multiple boxes.
        Plot.tip(
          all.flatMap((d) => {
            const f = d.family ?? "python";
            const t = `${familyLabel(f)} · ${d.size_mb} MB (zip ${(d.zip_bytes / 1e6).toFixed(1)} MB)\nresidual ${d.residual_p50.toFixed(0)} ms (${d.residual_min.toFixed(0)}–${d.residual_max.toFixed(0)})\ninit ${d.init_p50.toFixed(0)} ms`;
            return [
              { size_mb: d.size_mb, y: d.residual_p50, title: t },
              { size_mb: d.size_mb, y: d.init_p50, title: t },
            ];
          }),
          Plot.pointer({ x: "size_mb", y: "y", title: (d) => d.title }),
        ),
      ],
    }),
  });
  const seriesLegend = swatchLegend(
    families.map((f) => ({ label: familyLabel(f), color: colorOf(f) })),
  );
  const styleLegend = document.createElement("div");
  styleLegend.className = "caption";
  styleLegend.innerHTML =
    "- - - dashed: <b>download + start residual</b> (unreported) &nbsp;&nbsp; &#9472;&#9472;&#9472; solid: <b>Init Duration</b> (reported)";
  const legends = document.createElement("div");
  legends.append(seriesLegend, styleLegend);
  return withLegend(legends, chart);
}

// ---- Zip vs container-image download-scaling -------------------------------
// Overlays the zip family (python series) against the container-image family
// (image-touched + image-untouched) on one size axis, to show where the
// artifact-size cost lands. Two lines per series with a fixed convention:
//   SOLID  = reported Init Duration (init_p50)
//   DASHED = unreported pre-init residual (residual_p50; download + environment start)
// The story reads off the chart: the zip's dashed line (its hidden download)
// climbs ~linearly while its solid init stays flat; the image's dashed line
// stays a flat floor while its touched solid init climbs. `imageData` carries
// {family: image-touched|image-untouched, size_mb, init_p50, residual_p50}.
export function zipVsImageDownloadScaling(imageData, invalidation) {
  // Zip baseline is the python family sliced from the same `--with-image` run
  // that produced the image samples (imageData.zip_baseline), so it shares one
  // session/vantage/date with them. Reading it here rather than the
  // separately-written lifecycle-download-scaling.json keeps this chart a
  // single-session snapshot.
  const zip = (imageData?.zip_baseline ?? [])
    .filter((s) => s.size_mb > 0)
    .map((s) => ({
      series: "zip",
      size_mb: s.size_mb,
      init: s.init_p50,
      resid: s.residual_p50,
    }));
  const img = (imageData?.samples ?? [])
    .filter((s) => s.size_mb > 0)
    .map((s) => ({
      series: s.family,
      size_mb: s.size_mb,
      init: s.init_p50,
      resid: s.residual_p50,
    }));
  const all = [...zip, ...img];
  if (!all.length) {
    const el = document.createElement("p");
    el.className = "caption";
    el.textContent =
      "No zip-vs-image samples yet. Run `bencher probe download-scaling --with-image`.";
    return el;
  }

  // Series -> color. Zip reuses the python hue (both are the managed python
  // runtime); the two image variants get distinct hues.
  const SERIES = [
    { key: "zip", label: "Zip (.zip archive)", color: "#539eea" },
    {
      key: "image-touched",
      label: "Image, padding read at init",
      color: "#ea9953",
    },
    {
      key: "image-untouched",
      label: "Image, padding not read",
      color: "#8f8f8f",
    },
  ].filter((s) => all.some((d) => d.series === s.key));
  const seriesLabel = (k) => SERIES.find((s) => s.key === k)?.label ?? k;
  const sizes = [...new Set(all.map((s) => s.size_mb))].sort((a, b) => a - b);
  const yMax = Math.max(...all.map((d) => Math.max(d.init, d.resid))) * 1.08;

  const chart = responsivePlot({
    invalidation,
    minWidth: 320,
    maxWidth: 760,
    build: (width) => ({
      ...plotBase,
      width,
      height: 380,
      marginLeft: 56,
      marginRight: 24,
      marginTop: 20,
      marginBottom: 46,
      x: {
        type: "log",
        domain: [Math.min(...sizes) / 1.4, Math.max(...sizes) * 1.4],
        ticks: sizes,
        tickFormat: (d) => `${d} MB`,
        label: "Added artifact size (log) →",
        grid: true,
      },
      y: { domain: [0, yMax], label: "↑ ms (cold-start cost)", grid: true },
      marks: [
        frame(),
        ...SERIES.flatMap((s) => {
          const rows = all
            .filter((d) => d.series === s.key)
            .slice()
            .sort((a, b) => a.size_mb - b.size_mb);
          const c = s.color;
          return [
            // Dashed = unreported pre-init residual (download + environment start).
            Plot.lineY(rows, {
              x: "size_mb",
              y: "resid",
              stroke: c,
              strokeWidth: 2.5,
              strokeDasharray: "4 3",
              curve: "monotone-x",
            }),
            Plot.dot(rows, {
              x: "size_mb",
              y: "resid",
              fill: c,
              r: 3.5,
              symbol: "diamond",
            }),
            // Solid = reported Init Duration.
            Plot.lineY(rows, {
              x: "size_mb",
              y: "init",
              stroke: c,
              strokeWidth: 2.5,
              curve: "monotone-x",
            }),
            Plot.dot(rows, { x: "size_mb", y: "init", fill: c, r: 4 }),
          ];
        }),
        // One shared tip over every plotted point (both lines, all series), so
        // the pointer surfaces only the nearest dot where the lines nearly
        // overlap (see the dumbbell builder). Each point carries its series' full
        // init+residual so either dot shows the complete picture.
        Plot.tip(
          all.flatMap((d) => [
            {
              size_mb: d.size_mb,
              y: d.init,
              label: seriesLabel(d.series),
              init: d.init,
              resid: d.resid,
            },
            {
              size_mb: d.size_mb,
              y: d.resid,
              label: seriesLabel(d.series),
              init: d.init,
              resid: d.resid,
            },
          ]),
          Plot.pointer({
            x: "size_mb",
            y: "y",
            title: (d) =>
              `${d.label} · ${d.size_mb} MB\ninit (reported) ${d.init.toFixed(0)} ms\nresidual (unreported) ${d.resid.toFixed(0)} ms\ntotal cold overhead ${(d.init + d.resid).toFixed(0)} ms`,
          }),
        ),
      ],
    }),
  });

  // Two legends: the series colors, and the solid/dashed line convention.
  const seriesLegend = swatchLegend(
    SERIES.map((s) => ({ label: s.label, color: s.color })),
  );
  const styleLegend = document.createElement("div");
  styleLegend.className = "caption";
  styleLegend.innerHTML =
    "&#9472;&#9472;&#9472; solid: <b>Init Duration</b> (reported) &nbsp;&nbsp; - - - dashed: <b>pre-init residual</b> (download + environment start, unreported)";
  const legends = document.createElement("div");
  legends.append(seriesLegend, styleLegend);
  return withLegend(legends, chart);
}

// ---- Shared dumbbell builder -----------------------------------------------
// A dumbbell row carries a label (`row`), the two endpoint values (`x0`/`x1`),
// and a precomputed tooltip per endpoint (`title0`/`title1`). All three A/B
// charts (arch, opt-level, SnapStart) share this scaffold: a log x with a
// self-scaled [min*0.85, max*1.15] domain, one connecting rule, and one dot per
// endpoint. Only the row construction, labels, and colors are passed in.
// Returns a `build(width)` function for responsivePlot rather than a rendered
// plot, so every dumbbell is fluid. The label margin and row height stay fixed;
// only the plotting area flexes.
function dumbbellBuild({
  rows,
  rowOrder,
  keys,
  colors,
  xLabel,
  marginLeft,
  rowHeight,
}) {
  const [k0, k1] = keys;
  // Log axis: keep only positive endpoints so a 0/negative value cannot collapse
  // the domain bound to 0 and blank the chart.
  const vals = rows.flatMap((d) => [d.x0, d.x1]).filter((x) => x > 0);
  // One record per endpoint, read by both the dots and the single shared tip so
  // the pointer picks the one nearest endpoint. Separate `tip:true` dot marks
  // would each carry their own pointer and both fire when the two endpoints
  // nearly overlap on a row.
  const points = rows.flatMap((d) => [
    { row: d.row, x: d.x0, key: k0, title: d.title0 },
    { row: d.row, x: d.x1, key: k1, title: d.title1 },
  ]);
  return (width) => ({
    ...plotBase,
    width,
    height: 100 + rowOrder.length * rowHeight,
    marginLeft,
    marginRight: 30,
    marginTop: 20,
    marginBottom: 44,
    x: {
      type: "log",
      domain: [Math.min(...vals) * 0.85, Math.max(...vals) * 1.15],
      label: xLabel,
      grid: true,
    },
    y: { domain: rowOrder, label: null, tickSize: 0 },
    color: { domain: keys, range: colors },
    marks: [
      frame(),
      Plot.ruleY(rows, {
        y: "row",
        x1: "x0",
        x2: "x1",
        stroke: THEME.frame,
        strokeWidth: 2,
      }),
      Plot.dot(points, { y: "row", x: "x", fill: "key", r: 4 }),
      // Single shared tip so two near-overlapping dots can't fire two boxes.
      Plot.tip(
        points,
        Plot.pointer({ y: "row", x: "x", title: (d) => d.title }),
      ),
    ],
  });
}

// The two-endpoint legend for a dumbbell chart, in the shared swatch style.
// `legendFormat` maps each raw key to its human label (e.g. "o3" ->
// "opt-level=3"); absent, the key is shown verbatim (the arch dumbbell, whose
// keys are already "arm64"/"x86_64").
function dumbbellLegend(keys, colors, legendFormat) {
  return swatchLegend(
    keys.map((k, i) => ({
      label: legendFormat ? legendFormat(k) : k,
      color: colors[i],
    })),
  );
}

// ---- Architecture A/B dumbbell (per language, self-scaled) -----------------
export function archDumbbell(v, { metric }) {
  const pair = archPair(v.stats);
  if (!pair) return document.createElement("div");
  const { a0, a1, archs, p50 } = pair;
  // Cold uses `coldInit` (the init/restore marker only), not `cold` (init + first
  // request), so this reads the same field the arch win-rate tallies; otherwise a
  // cell where arm64 wins on init but loses on the total would contradict it.
  const yField = metric === "cold" ? "coldInit" : "warm";
  const rowOrder = [];
  for (const scenario of v.scenarios)
    for (const m of v.memories)
      rowOrder.push(`${shortScenario(scenario)} · ${m}MB`);

  const items = [];
  for (const lang of v.languages) {
    const rows = [];
    for (const scenario of v.scenarios) {
      for (const m of v.memories) {
        // Read from the full dataset: this chart shows both architectures per row
        // regardless of the arch filter (which would otherwise leave a one-ended
        // dumbbell). `archPair.p50` enforces that.
        const x0 = p50(lang, scenario, m, a0, yField);
        const x1 = p50(lang, scenario, m, a1, yField);
        if (x0 == null || x1 == null) continue;
        const where = `${lang} ${scenario} @ ${m}MB`;
        rows.push({
          row: `${shortScenario(scenario)} · ${m}MB`,
          x0,
          x1,
          title0: `${where}\n${a0}: ${fmtMs(x0)}`,
          title1: `${where}\n${a1}: ${fmtMs(x1)}`,
        });
      }
    }
    if (!rows.length) continue;
    items.push({
      title: langLabel(lang),
      plot: dumbbellBuild({
        rows,
        rowOrder,
        keys: archs,
        colors: PAIRS.arch,
        xLabel: `${metric === "cold" ? "cold init" : "warm"} P50 (ms, log) →`,
        marginLeft: 150,
        rowHeight: 13,
      }),
    });
  }
  return withLegend(
    dumbbellLegend(archs, PAIRS.arch),
    panelGrid(items, {
      minPanel: 420,
      maxPanel: 640,
      invalidation: v.invalidation,
    }),
  );
}

// ---- Rust opt-level A/B dumbbell -------------------------------------------
export function optDumbbell(v, { metric }) {
  const stats = v.stats;
  if (!stats.dimensions.hasOpt) return document.createElement("div");
  const field = metric === "cold" ? "coldInitP50" : "warmP50";
  const archs = [...new Set(stats.optCells.map((c) => c.arch))]
    .filter((a) => v.architectures.includes(a))
    .sort();
  const sizeKind = stats.dimensions.hasUnzippedSize
    ? "unzipped binary"
    : "zip package";
  const rows = [];
  const rowOrder = [];
  const find = (arch, scenario, opt, m) =>
    stats.optCells.find(
      (c) =>
        c.arch === arch &&
        c.scenario === scenario &&
        c.opt === opt &&
        c.memory_mb === m,
    );
  for (const scenario of v.scenarios) {
    for (const arch of archs) {
      for (const m of v.memories) {
        const o3 = find(arch, scenario, "o3", m);
        const oz = find(arch, scenario, "oz", m);
        const o3v = o3?.[field];
        const ozv = oz?.[field];
        // Skip before building the row label, so a missing cell never leaves a
        // labeled y-row with no dumbbell.
        if (o3v == null || ozv == null) continue;
        const o3MB = o3.unzipped
          ? o3.unzipped / 1e6
          : o3.zip
            ? o3.zip / 1e6
            : null;
        const ozMB = oz.unzipped
          ? oz.unzipped / 1e6
          : oz.zip
            ? oz.zip / 1e6
            : null;
        const sizeTag =
          o3MB != null && ozMB != null
            ? ` · ${o3MB.toFixed(1)}/${ozMB.toFixed(1)}MB`
            : "";
        const row = `${shortScenario(scenario)} · ${arch} · ${m}MB${sizeTag}`;
        // Append the size to the tooltip only when known; a null would
        // interpolate the literal "undefinedMB" (as the row label already guards).
        const sizeNote = (mb) =>
          mb != null ? ` · ${mb.toFixed(2)}MB ${sizeKind}` : "";
        rowOrder.push(row);
        rows.push({
          row,
          x0: o3v,
          x1: ozv,
          title0: `${row}\nopt-level=3: ${fmtMs(o3v)}${sizeNote(o3MB)}`,
          title1: `${row}\nopt-level=z: ${fmtMs(ozv)}${sizeNote(ozMB)}`,
        });
      }
    }
  }
  if (!rows.length) return document.createElement("div");
  const keys = ["o3", "oz"];
  const optLabel = (o) => (o === "o3" ? "opt-level=3" : "opt-level=z");
  const chart = responsivePlot({
    invalidation: v.invalidation,
    minWidth: 520,
    build: dumbbellBuild({
      rows,
      rowOrder,
      keys,
      colors: PAIRS.opt,
      xLabel: `${metric === "cold" ? "Cold init" : "Warm"} P50 (ms, log) →`,
      marginLeft: 290,
      rowHeight: 11,
    }),
  });
  return withLegend(dumbbellLegend(keys, PAIRS.opt, optLabel), chart);
}

// ---- Rust aws-lc-rs jitter-entropy A/B (cold breakdown, jitter on vs off) --
// Two scenarios, both Rust o3, side-by-side panels, each a stacked horizontal
// bar chart with two rows per (arch, memory): jitter=off (the standing matrix
// baseline) and jitter=on (the diagnostic build), split into init vs
// first-request segments.
//
// Showing the two segments, not just the total, is the point: which segment the
// tax lands in is the story. Empirically Lambda's two cold-start phases appear to
// run on different CPU envelopes: Init phase ~ full vCPU regardless of tier;
// Invoke phase = the tier's fractional vCPU. (Init-phase boost from re:Invent
// 2019, not a contract; the caption above the chart carries the disclaimer.)
//   - oneclient: tax lands in firstReq (TLS handshake in the Invoke phase) -> a
//     cliff that grows steeply as memory shrinks.
//   - lettercount: tax lands in init (TLS handshake in the Init phase,
//     measured-cheaper today) -> a roughly flat bump across tiers.
// The chart shape (cliff vs flat bump) is what is measured; the phase-CPU
// asymmetry is the most parsimonious explanation. Same one-time CPU cost, two
// wall-clock outcomes.
//
// Per-panel subtitle: the scenario IDs name the handler shape, not the phase the
// TLS handshake lands in, yet the phase is the entire reason the two panels
// differ. Pin the phase under each panel title.
const JITTER_PANEL_SUBTITLES = {
  oneclient:
    "First TLS handshake in the Invoke phase, on the configured tier's vCPU",
  lettercount:
    "First TLS handshake in the Init phase, where the Init phase appears to run on more CPU than the tier alone provides",
};

export function jitterCliff(v) {
  const stats = v.stats;
  if (!stats.dimensions.hasJitter) return document.createElement("div");
  // Scenarios that carry the A/B in the run. Filter against the active selection
  // but never widen: jitter rows only exist for these scenarios.
  const candidates = ["oneclient", "lettercount"];
  const scenarios = candidates.filter(
    (s) =>
      v.scenarios.includes(s) &&
      stats.jitterCells.some((c) => c.scenario === s),
  );
  if (!scenarios.length) return document.createElement("div");

  const archs = [...new Set(stats.jitterCells.map((c) => c.arch))]
    .filter((a) => v.architectures.includes(a))
    .sort();
  if (!archs.length) return document.createElement("div");

  // Ascending memory order (128 at top -> 3008 at bottom) so the eye travels in
  // the direction of the story: the cliff is worst at the lowest tier and shrinks
  // as memory grows. Matches the small->large convention on the other x-axes.
  const memories = v.memories.slice().sort((a, b) => a - b);

  // Build the per-panel data first to derive a shared x-domain across both
  // panels. Independent x-scales would stretch each panel to its own range and
  // hide the equal-sized "tax" segment, which is the headline of this chart.
  const panels = [];
  let sharedMax = 0;
  for (const scenario of scenarios) {
    const data = [];
    const deltas = [];
    const rowOrder = [];
    for (const arch of archs) {
      for (const m of memories) {
        const off = stats.jitterCells.find(
          (c) =>
            c.scenario === scenario &&
            c.arch === arch &&
            c.memory_mb === m &&
            c.jitter === "off",
        );
        const on = stats.jitterCells.find(
          (c) =>
            c.scenario === scenario &&
            c.arch === arch &&
            c.memory_mb === m &&
            c.jitter === "on",
        );
        const offTotal =
          off && off.initP50 != null && off.firstReqP50 != null
            ? off.initP50 + off.firstReqP50
            : null;
        const onTotal =
          on && on.initP50 != null && on.firstReqP50 != null
            ? on.initP50 + on.firstReqP50
            : null;
        for (const [jitter, cell] of [
          ["off", off],
          ["on", on],
        ]) {
          if (!cell || cell.initP50 == null || cell.firstReqP50 == null)
            continue;
          const row = `${arch} · ${m}MB · jitter ${jitter}`;
          if (!rowOrder.includes(row)) rowOrder.push(row);
          data.push({
            row,
            jitter,
            part: "init",
            order: 0,
            ms: cell.initP50,
            total: cell.initP50 + cell.firstReqP50,
          });
          data.push({
            row,
            jitter,
            part: "first request",
            order: 1,
            ms: cell.firstReqP50,
            total: cell.initP50 + cell.firstReqP50,
          });
        }
        // The on-row carries the headline +Δ tax; render it inline at the end of
        // the on bar so readers don't subtract two totals to see the cost.
        if (offTotal != null && onTotal != null) {
          deltas.push({
            row: `${arch} · ${m}MB · jitter on`,
            total: onTotal,
            delta: onTotal - offTotal,
          });
        }
      }
    }
    if (!data.length) continue;
    const panelMax = Math.max(...data.map((d) => d.total));
    if (panelMax > sharedMax) sharedMax = panelMax;
    panels.push({ scenario, data, deltas, rowOrder });
  }
  if (!panels.length) return document.createElement("div");

  // Headroom for the +Δms annotations after the longest bar.
  const sharedDomain = [0, sharedMax * 1.18];

  const items = panels.map(({ scenario, data, deltas, rowOrder }) => ({
    title: scenarioLabel(scenario),
    // Subtitle pinning when the first TLS handshake happens: the only thing
    // that distinguishes the two panels' shapes. See `JITTER_PANEL_SUBTITLES`.
    subtitle: JITTER_PANEL_SUBTITLES[scenario] ?? "",
    plot: (width) => ({
      ...plotBase,
      width,
      height: 80 + rowOrder.length * 22,
      // Sized for the longest label (`x86_64 · 1024MB · jitter off` ~ 28 chars,
      // ~7px/char ~ 196px); narrower clips the x86_64 rows on a 4-digit tier.
      marginLeft: 196,
      marginRight: 60,
      marginTop: 20,
      marginBottom: 44,
      // Shared so the equal-sized jitter tax reads equal-sized in both panels.
      x: { label: "Cold latency P50 (ms) →", grid: true, domain: sharedDomain },
      y: { domain: rowOrder, label: null, tickSize: 0 },
      color: { domain: ["init", "first request"], range: PAIRS.initFirst },
      marks: [
        frame(),
        Plot.barX(data, {
          y: "row",
          x: "ms",
          fill: "part",
          order: "order",
          // Lower opacity on the off (baseline) rows so the on (taxed) rows read
          // as the foreground; otherwise off/on differs only in the y-label text.
          fillOpacity: (d) => (d.jitter === "off" ? 0.65 : 1),
          tip: true,
          title: (d) =>
            `${d.row}\n${d.part}: ${fmtMs(d.ms)}\nsum of segment P50s: ${fmtMs(d.total)}`,
        }),
        // Jitter-tax delta at the end of each on bar, with a leading "+" so the
        // sign reads even when the value is small.
        Plot.text(deltas, {
          y: "row",
          x: "total",
          text: (d) => `  +${fmtMs(d.delta)}`,
          textAnchor: "start",
          fill: THEME.text,
          fontSize: 11,
          fontWeight: 600,
        }),
      ],
    }),
  }));
  return withLegend(
    initFirstLegend(),
    panelGrid(items, {
      minPanel: 440,
      maxPanel: 640,
      invalidation: v.invalidation,
    }),
  );
}

// ---- SnapStart A/B dumbbell ------------------------------------------------
// Plain vs SnapStart cold-start P50, one row per runtime × scenario × arch ×
// memory. `only` scopes to specific runtimes: a per-runtime page passes
// `only: ["java"]` so it never sprouts rows for another runtime that later gains
// a SnapStart variant; omitting it renders every runtime in snapCells. The
// runtime appears in the row label only when more than one is shown.
export function snapDumbbell(v, { only = null } = {}) {
  const stats = v.stats;
  if (!stats.dimensions.hasSnapStart) return document.createElement("div");
  const langs = snapLangsToShow(stats.snapCells, v.languages, only);
  const archs = [...new Set(stats.snapCells.map((c) => c.arch))]
    .filter((a) => v.architectures.includes(a))
    .sort();
  const find = (lang, arch, scenario, snap, m) =>
    stats.snapCells.find(
      (c) =>
        c.lang === lang &&
        c.arch === arch &&
        c.scenario === scenario &&
        c.snapstart === snap &&
        c.memory_mb === m,
    )?.coldP50;
  const showLang = langs.length > 1;
  const rows = [];
  const rowOrder = [];
  for (const lang of langs) {
    for (const scenario of v.scenarios) {
      for (const arch of archs) {
        for (const m of v.memories) {
          const plain = find(lang, arch, scenario, false, m);
          const snap = find(lang, arch, scenario, true, m);
          if (plain == null || snap == null) continue;
          const prefix = showLang ? `${langLabel(lang)} · ` : "";
          const row = `${prefix}${shortScenario(scenario)} · ${arch} · ${m}MB`;
          rowOrder.push(row);
          rows.push({
            row,
            x0: plain,
            x1: snap,
            title0: `${row}\nplain cold: ${fmtMs(plain)}`,
            title1: `${row}\nSnapStart cold: ${fmtMs(snap)}`,
          });
        }
      }
    }
  }
  if (!rows.length) return document.createElement("div");
  const keys = ["plain", "snap"];
  const snapLabel = (o) => (o === "plain" ? "plain" : "SnapStart");
  const chart = responsivePlot({
    invalidation: v.invalidation,
    minWidth: 480,
    build: dumbbellBuild({
      rows,
      rowOrder,
      keys,
      colors: PAIRS.plainSnap,
      xLabel: "Cold start P50 (ms, log) →",
      marginLeft: 250,
      rowHeight: 13,
    }),
  });
  return withLegend(dumbbellLegend(keys, PAIRS.plainSnap, snapLabel), chart);
}
