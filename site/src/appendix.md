---
title: "AWS Lambda benchmark data appendix: percentiles and distributions"
toc: false
---

# Data appendix

```js
const stats = await FileAttachment("data/stats.json").json();
import { filterForm } from "./components/filters.js";
import { colorModel } from "./lib/series.js";
import * as C from "./components/charts.js";
import { percentileTable } from "./components/tables.js";
import { scenarioLabel } from "./lib/format.js";
import { MIN_N_FOR_P99, MIN_N_FOR_P999 } from "./lib/stats.js";
const cm = colorModel(
  stats.dimensions.languages,
  stats.dimensions.architectures,
);
```

```js
const sel = view(filterForm(stats, { colorModel: cm }));
```

```js
const v = C.makeView(stats, sel, invalidation);
```

## Cold vs warm distribution

<div class="chart-sub">Each dot is one invocation (the dense body is down-sampled, but every tail outlier at or above P98 is kept); ticks mark each series' cold and warm medians, computed from the full data. Selecting a memory tier rescales the x-axis, since latency ranges differ widely across memory.</div>

```js
// One tier at a time: the scatter is the heaviest view, so render only the
// selected tier rather than all six at once.
const distMem = view(
  Inputs.radio(stats.dimensions.memories, {
    label: "Memory tier (MB)",
    value: stats.dimensions.pivotMem,
  }),
);
```

```js
display(C.distribution(v, distMem));
```

## Full percentiles, by scenario

<div class="chart-sub">Exact warm and cold percentiles (min / P10 / P50 / P90 / P99 / P99.9 / max, R-7 linear interpolation) for every measured series × memory cell in the current selection, with the sample count (n) behind each phase. How to read the cells:
<ul>
<li><b>Coverage.</b> A cell absent because that scenario has a memory floor (e.g. batch, cache) is skipped. Rust is shown for <code>opt-level=3</code> only (the o3-vs-oz A/B is on the <a href="./rust">Rust</a> page). The two Smithy scenarios list Java, Java SnapStart, Node, and Rust only; Python and Go skip them.</li>
<li><b>What "cold" is.</b> Cold = init (or restore) + first-request duration, both taken from the fields the Lambda <code>REPORT</code> line records. It excludes the code download + execution-environment provisioning that happens before <code>init</code> and appears in no function metric (measured separately on <a href="./lifecycle">Cold Start Anatomy</a>).</li>
<li><b>The cold tail is P90.</b> Cold n is one sample per cold cycle and varies by scenario (see the per-cell n column). Even the fullest cold cell is too few for a stable P99/P99.9, so those cold cells read <code>—</code>; P90 is the cold tail to read, and <code>max</code> the worst observed.</li>
<li><b>Warm tail gates.</b> The warm tail carries a P99 where the raw sample count reaches ${MIN_N_FOR_P99}, and a P99.9 where it reaches ${MIN_N_FOR_P999}; below those floors the cell is blank.</li>
<li><b>The warm P99/P99.9 is a cross-cell comparison, not an i.i.d.-robust point estimate.</b> Warm samples within one cold cycle share a sandbox, so they are correlated: the effective number of independent observations behind a warm tail is closer to that cell's <em>cold</em> n (its cold-cycle count) than to the much larger warm n, so a large gap is best confirmed with re-runs.</li>
</ul>
</div>

```js
for (const scenario of v.scenarios) {
  const table = percentileTable(v, scenario);
  if (table) {
    display(html`<h3>${scenarioLabel(scenario)}</h3>`);
    display(table);
  }
}
```

## Disclaimer

This is an independent personal project. It is **not affiliated with, endorsed, sponsored, or reviewed by Amazon Web Services or any of the companies whose products it measures** (programming-language vendors, runtime maintainers, or anyone else). No company has tested, certified, or approved it, and nothing here should be read as an official statement from any of them. All trademarks belong to their respective owners.

The site is provided **as-is, with no warranty of any kind**. The numbers are best-effort measurements from a specific setup at a specific time: they can be wrong, misconfigured, skewed by the measurement harness, or simply out of date as runtimes, SDKs, and the platform change. **No claim is made that any figure here is factually correct.** Nothing on this site is advice; do not make production or purchasing decisions based on it. If a result matters to you, reproduce and verify it against your own testing in your own environment.
