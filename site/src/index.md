---
title: "AWS Lambda cold start benchmark: Rust, Go, Node, Java, Python"
toc: false
---

```js
// Load the compact aggregates once; the head-to-head table reads from this.
const stats = await FileAttachment("data/stats.json").json();
```

```js
import { filterForm } from "./components/filters.js";
import { colorModel } from "./lib/series.js";
import * as C from "./components/charts.js";
import { makeView } from "./components/charts.js";
import { headToHead } from "./components/tables.js";
import {
  langLabel,
  scenarioLabel,
  fmtMs,
  fmtDateUtc,
  SCENARIO_BLURBS,
} from "./lib/format.js";
import { MIN_N_FOR_P99 } from "./lib/stats.js";
```

```js
const cm = colorModel(
  stats.dimensions.languages,
  stats.dimensions.architectures,
);
```

<div class="hero">
<div class="eyebrow">AWS Lambda · cold &amp; warm · scenario benchmark</div>

# AWS Lambda Runtime Benchmark: cold start, warm latency, memory & arm64

```js
// Languages rendered in their series color (Rust orange, Node teal, …) as a
// subtitle under the static keyword <h1> above. The static heading is the
// crawlable SEO anchor; this colored line stays a visual flourish, not an <h1>.
display(
  html`<div class="langs-title">
    ${stats.dimensions.languages.flatMap((l, i) => [
      i > 0 ? html`<span class="vs"> vs </span>` : "",
      html`<span style="color:${cm.langColor[l]}">${langLabel(l)}</span>`,
    ])} <span class="vs">across the benchmark</span>
  </div>`,
);
```

<div class="hero-sub">${stats.dimensions.scenarios.length} scenarios, each doing identical work across the languages that host it (Python and Go skip the two Smithy framework scenarios), swept across memory and CPU architecture, measured cold and warm. The matrix deploys every function as a <b>.zip file archive</b>; container-image cold-start behavior is explored separately on <a href="./lifecycle#zip-vs-container-image-where-the-size-cost-lands">Cold Start Anatomy</a>. The headline cold-start result and the full numeric summary are below; the <a href="./comparison">Comparison</a> page adds warm latency, cost, tail, and architecture, <a href="./lifecycle">Cold Start Anatomy</a> breaks down what the cold number does and doesn't include, and <a href="./rust">Rust</a> / <a href="./java-snapstart">Java SnapStart</a> cover their runtime-specific dimensions.</div>
<div class="hero-disclaimer">Independent personal project, not affiliated with or endorsed by AWS or any vendor. Best-effort numbers, provided as-is with no guarantee of accuracy. <a href="./appendix#disclaimer">Disclaimer →</a></div>
</div>

```js
// KPI cards from the loader-computed headline numbers.
const kpi = stats.kpi;
const collected = fmtDateUtc(stats.meta.started_at_unix_ms);
// KPI-only: values of 10 s and up read as seconds ("16.6s", not "16606ms").
// Table cells keep fmtMs so their columns stay in one unit.
const fmtMsKpi = (v) => (v >= 10_000 ? `${(v / 1000).toFixed(1)}s` : fmtMs(v));
const cards = [
  {
    v: stats.meta.total_functions ?? "—",
    k: "Functions",
    s: "full matrix: lang, scenario, arch, memory, plus Rust opt/jitter and Java SnapStart variants",
  },
  {
    v: kpi.totalInvocations.toLocaleString(),
    k: "Invocations",
    s: `${kpi.coldCount.toLocaleString()} cold · ${kpi.warmCount.toLocaleString()} warm`,
  },
  {
    v: kpi.coldRange
      ? `${fmtMsKpi(kpi.coldRange.min)} – ${fmtMsKpi(kpi.coldRange.max)}`
      : "—",
    k: "Cold start range (P50)",
    s: "init/restore + first request (excludes download + env setup)",
  },
  {
    v: stats.meta.region ?? "—",
    k: "Region",
    s: "single region; not measured cross-region",
    // Region ids ("eu-central-1") would otherwise wrap at a hyphen mid-token.
    vClass: "sm",
  },
  { v: collected ?? "—", k: "Data collected", s: "single run, UTC start date" },
];
display(
  html`<div class="kpis">
    ${cards.map(
      (c) =>
        html`<div class="kpi">
          <div class="v${c.vClass ? ` ${c.vClass}` : ""}">${c.v}</div>
          <div class="k">${c.k}</div>
          <div class="s">${c.s}</div>
        </div>`,
    )}
  </div>`,
);
```

<div class="cold-caveat">
<strong>Every cold-start number on this site is a floor, not the full wait.</strong> The reported cold figure is <code>init</code> (or SnapStart <code>Restore</code>) + the first request's <code>Duration</code>, which is all a <code>REPORT</code> line exposes. <b>Before</b> that clock even starts, the caller also waits through <b>code download + execution-environment setup</b>, latency that appears in <em>no</em> function metric: a roughly constant floor for small packages, growing to <b>a couple hundred ms</b> for the larger packages here, and to <b>of order a second</b> near Lambda's size limit. So the true cold start a caller feels is these numbers <b>plus</b> an unreported term. <a href="./lifecycle#the-hidden-steps-download-environment-start">Cold Start Anatomy measures it →</a>
</div>

<details class="scenarios">
<summary>What the ${stats.dimensions.scenarios.length} scenarios do &amp; what each one measures</summary>

<div class="scenarios-intro">Every scenario is measured both cold and warm, but each is <em>designed</em> to be read on one axis: the handler-shape scenarios (Hello World through Smithy + write flow) on <b>cold start</b>, where setup cost lands; the CPU probes (Letter count onward) on <b>warm latency</b>, where the per-request work shows. The note after each scenario points at that axis.</div>

```js
display(
  html`<dl>
    ${stats.dimensions.scenarios.map((s) => {
      const b = SCENARIO_BLURBS[s];
      return b
        ? html`<div class="row">
            <dt>${scenarioLabel(s)}</dt>
            <dd>${b.does} <span class="why">${b.why}</span></dd>
          </div>`
        : "";
    })}
  </dl>`,
);
```

</details>

## Filters

```js
// One shared, reactive selection. `makeView` applies it once; the table reads
// the filtered view.
const sel = view(filterForm(stats, { colorModel: cm }));
```

```js
const v = makeView(stats, sel, invalidation);
```

## Cold start latency vs memory

<div class="chart-sub">Total cold-start latency (init or restore + first request) as the memory tier rises, one panel per scenario. Line = P50 (typical), shaded = P10–P90. Lower is better; each scenario has its own y-axis so its shape is readable. Each cell (one series at one memory tier) rests on few samples, one per cold cycle: the handler-shape cells run about 50, the SnapStart cells fewer (each needs a fresh version publish), and the CPU-probe cells as few as 5. A line can therefore vary between adjacent tiers; the appendix sample counts (n) give the basis for a small difference before it is read as real. Rust cells are built with <code>AWS_LC_SYS_NO_JITTER_ENTROPY=1</code>, a latency/security trade-off the <a href="./rust#the-aws-lc-rs-jitter-entropy-cold-start-tax">Rust page</a> covers with the on-vs-off A/B. The <a href="./comparison">Comparison</a> page covers warm latency, cost, tail, and architecture.</div>

```js
display(C.legend(v));
display(C.coldVsMemory(v));
```

## Full latency table: every language and memory tier

<div class="chart-sub">The numbers behind the chart: cold and warm latency across every memory tier, with a cell shown only where that runtime and scenario were deployed at that tier (some scenarios have a memory floor, so their lowest tiers are absent). Each phase shows <b>P50</b> (the typical case) plus the strongest tail its sample count supports: <b>cold is P50 / P90</b>, <b>warm is P50 / P99</b>. <b>Bold</b> = fastest P50; the <b>brightest tail</b> = fastest at that percentile (it can differ from the P50 winner). Spread = slowest ÷ fastest language on P50.</div>

```js
display(headToHead(v));
```

<div class="chart-sub read-me"><strong>Reading the cold spread:</strong> a large cold gap at low memory tiers is partly a <em>lifecycle</em> effect, not only a speed contest. The Lambda Init phase appears to run on boosted CPU, so runtimes that run their setup eagerly at init (e.g. Rust) collect that subsidy, while lazier ones (e.g. Go) defer the same work into the first request, at the memory tier's ordinary, much smaller CPU allocation; put Rust and Go on equal footing and <em>their</em> several-fold low-memory gap narrows to about 1.3x (rough context from a dated off-matrix probe, not this site's benchmark data) — a lifecycle-timing story specific to two runtimes with comparable raw speed, not a general rule that doing everything eagerly erases every cold gap. <a href="./lifecycle">Cold Start Anatomy →</a> explains the mechanism, what AWS does and doesn't document about it, and how to read these numbers because of it.</div>

<div class="caption">Cold percentiles rest on few samples (one per cold cycle, and most warm-axis CPU scenarios run fewer cold cycles than that), so the cold tail is reported as P90 (the same P10–P90 band the chart above shades) rather than a P99 that would sit on under one tail sample. The warm P99 passes the raw n&ge;${MIN_N_FOR_P99} gate but warm samples within a cold cycle are correlated, so read it as a cross-cell comparison; the <a href="./appendix">data appendix</a> carries the full statistical note and the exact per-cell sample counts (n) behind each phase.</div>

<style>
.hero { margin: 8px 0 4px; }
.hero .eyebrow { font-size: 12px; letter-spacing: .18em; text-transform: uppercase; color: var(--faint); }
.hero h1 { font-size: 34px; line-height: 1.1; letter-spacing: -0.025em; font-weight: 700; margin: 10px 0 4px; }
.hero .langs-title { font-size: 22px; line-height: 1.15; letter-spacing: -0.02em; font-weight: 700; margin: 0 0 8px; }
.hero-sub { color: var(--dim); font-size: 15px; max-width: 75ch; line-height: 1.5; margin-top: 6px; }
.hero-disclaimer {
  margin-top: 12px; max-width: 75ch; padding: 8px 12px;
  font-size: 13px; line-height: 1.45; color: var(--dim);
  background: color-mix(in srgb, var(--panel) 70%, transparent);
  border: 1px solid var(--frame); border-left: 3px solid var(--accent);
  border-radius: 6px;
}
.hero-disclaimer a { color: var(--accent); white-space: nowrap; font-weight: 600; }
.cold-caveat {
  margin: 16px 0 4px; padding: 12px 16px;
  font-size: 14.5px; line-height: 1.5; color: var(--text);
  background: color-mix(in srgb, #f0b429 12%, transparent);
  border: 1px solid color-mix(in srgb, #f0b429 40%, var(--frame));
  border-left: 4px solid #f0b429; border-radius: 6px;
}
.cold-caveat code { font-size: 0.92em; }
.cold-caveat a { color: var(--accent); font-weight: 600; white-space: nowrap; }
.scenarios { margin: 8px 0 12px; }
</style>
