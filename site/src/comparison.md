---
title: "AWS Lambda runtimes compared: latency, cost, tail, and architecture"
toc: true
---

# AWS Lambda runtimes compared: latency, cost, tail, and architecture

```js
const stats = await FileAttachment("data/stats.json").json();
import { filterForm } from "./components/filters.js";
import { colorModel } from "./lib/series.js";
import * as C from "./components/charts.js";
import { archWinRate } from "./components/tables.js";
import { TIE } from "./components/theme.js";
import { scenarioLabel } from "./lib/format.js";
import { MIN_N_FOR_P99, MIN_N_FOR_P999 } from "./lib/stats.js";
const cm = colorModel(
  stats.dimensions.languages,
  stats.dimensions.architectures,
);
```

This page collects every cross-language chart, ordered from cold start through warm steady-state to the cost, packaging, and architecture trade-offs. The two Smithy scenarios run on Java, Java SnapStart, Node, and Rust only; Python and Go skip them, so their panels show fewer series.

```js
const sel = view(filterForm(stats, { colorModel: cm }));
```

```js
// Apply the selection once; every chart on this page reads this filtered view.
const v = C.makeView(stats, sel, invalidation);
```

## Cold start breakdown: init vs first request

<div class="chart-sub">The cold-start latency split into its two parts at the selected breakdown tier (1024 MB when selected, otherwise the largest selected tier): init (or restore) and the first request. Each segment is a true P50 of its own samples per architecture; with both architectures selected, each language row shows the median of the selected architectures' segment P50s. On the handler-shape scenarios the first request is much slower than a steady warm call (clients, connection pools, TLS, and the JIT all warm up on first use); the lighter segment is that first-request portion.</div>

<div class="chart-sub"><strong>The total, not init alone, is the comparable quantity.</strong> The split is the same one-time setup cost landing in one segment or the other depending on whether a runtime does it <em>eagerly</em> (outside the handler, so it counts as init) or <em>lazily</em> (deferred to the first invocation, so it counts as that request's duration). A low init bar with a large first-request bar is therefore not faster than the reverse. The bar total is the sum of the two segment P50s, which is close to but not identical to the cold-total P50 on the Overview's cold-start chart (percentiles are not additive). Which phase a runtime places that setup in is the subject of <a href="./lifecycle">Cold Start Anatomy</a>.</div>

```js
display(C.coldBreakdown(v));
```

## Warm invocation latency vs memory

<div class="chart-sub">Line = P50, shaded = P10–P90. Steady-state request cost once warm. Each scenario has its own y-axis (independent scale), so compare magnitudes in the <a href="./">full latency table</a> on the Overview page rather than by panel height.</div>

```js
display(C.legend(v));
display(C.warmVsMemory(v));
```

## Tail latency: P99 & P99.9, warm

<div class="chart-sub">Two lines per series: SOLID = P99 (drawn where n &ge; ${MIN_N_FOR_P99}), DASHED = P99.9 (where n &ge; ${MIN_N_FOR_P999}); a line is omitted where its gate is not met. The tail captures the rare slow paths (likely collector pauses on garbage-collected runtimes, scheduling delays). Warm only: cold cells carry too few samples (one per cold cycle) to support a stable P99/P99.9, so there is no cold tail to plot here.</div>

<div class="chart-sub">Warm samples within one cold cycle share a sandbox and are correlated, so read these tails as cross-cell comparisons, not i.i.d.-robust point estimates (the <a href="./appendix">appendix</a> carries the full statistical note). For the AWS-client scenarios (1 AWS client, 3 AWS clients) and the realistic Smithy write flow, the tail mixes runtime variance with downstream DynamoDB/KMS/S3 latency. The remaining scenarios make no per-invoke downstream call, so their tails exclude per-invoke downstream AWS latency and mainly reflect runtime, platform, and handler-work variance: ${["hello", "smithy", "lettercount", "authz", "batch", "cache"].map(scenarioLabel).join(", ")}.</div>

```js
display(C.legend(v));
display(C.tailLatency(v));
```

## Warm tail vs median: the retained-heap GC signature (`cache`)

<div class="chart-sub"><code>cache</code> on one log-y panel: SOLID = warm P50, shaded = P10–P90, DASHED = P99. <strong>Absolute height is the signal here, not the line-to-line gap.</strong> On a log scale the line-to-line distance is the <em>ratio</em>, which a fast median inflates: a runtime can post a large P99/P50 ratio yet sit only a few ms above its median, while a slow one shows a small ratio over a far bigger absolute pause. The GC signal is in where the lines <em>sit</em>: at the low, CPU-starved tiers a tracing GC drives the P99 up. The non-GC runtime holds the lowest tail at every tier and stays close to its own median above the floor; the tracing-GC runtimes sit higher, with their largest tails at or near the low tiers. How much additional vCPU reduces a tail, and whether the lowest tier is the worst one, varies by runtime and is visible per line.</div>

<div class="chart-sub"><strong>"GC" here is inferred from the latency shape; it is not directly measured</strong> (we record the platform's <code>REPORT</code> line, not collector events). Three observations support it, and only for a tracing GC: the tail often shrinks with added vCPU (though not monotonically for every runtime), it tracks the collector (clearest on the generational stop-the-world runtimes, while the reference-counting and concurrent-collector runtimes sit closer to the non-GC floor), and the resident set rises with managed-heap overhead where the collector retains it. A high tail alone is not GC, since allocator jitter, JIT, and scheduling raise a P99 as well, so this interpretation is scoped to <code>cache</code>, the one scenario whose retained heap meets the conditions.</div>

```js
display(C.legend(v));
display(C.tailVsMedian(v));
```

## Warm CPU-sensitivity: how much does low memory slow each runtime?

<div class="chart-sub">Memory = CPU on Lambda. Warm P50 normalized to each series' own value at the max memory tier (1.0 = the series' P50 at the largest memory tier; values below 1.0 mean that tier measured faster than the baseline). FLAT means CPU-insensitive (runs at the lowest tier with no penalty); a CLIMB to the left means CPU-bound (requires more memory to reach full speed).</div>

```js
display(C.legend(v));
display(C.cpuSensitivity(v));
```

## Memory footprint: resident working set vs allocated memory

<div class="chart-sub">Max memory used (P50 line, P10–P90 band) while warm, vs the memory tier allocated. A flat, low line is a compact resident set. A line that drifts upward but stays well below the ceiling is retaining more resident memory at larger tiers, often heap or allocator headroom rather than a hard capacity limit. A line that approaches the ceiling (used ≈ allocated) at low tiers is the case that requires a larger tier. Each scenario has its own y-axis.</div>

```js
display(C.legend(v));
display(C.footprintVsMemory(v));
```

## Cost: $ per million warm invokes vs memory

<div class="chart-sub">Priced from billed duration × memory (GB-seconds) + the per-request fee, arm64/x86 rates applied per series. Line = MEAN cost (the bill for N invokes is N × the mean, not the median). For CPU-bound work the cheapest tier is often not the smallest: more memory adds CPU, which can complete the work fast enough to lower GB-seconds, so the cost curve dips before it rises. The tier at which that dip occurs (or whether a runtime is too CPU-starved for it to appear at all) is per-runtime and visible per line. Rates: a fixed eu-central-1 on-demand reference rate, free tier and volume discounts excluded. This run executed in eu-central-1 and is priced at eu-central-1 rates; a region that prices Lambda above eu-central-1 would bill more than shown.</div>

```js
display(C.legend(v));
display(C.costPerMillion(v));
```

## Artifact size across languages: unzipped deployment package

<div class="chart-sub">The on-disk (unzipped) size of the package each runtime deploys, per scenario, on a log x-axis. This is the unzipped deployment package size captured for the run, one possible input to cold-start work; the run does not separately measure extraction, loading, or linking time. Size grows with what each scenario bundles. One artifact per language (Rust shown for ${stats.dimensions.sizeArch}/<code>opt-level=3</code>; Java SnapStart omitted because it ships the same jar as plain Java). Exact unzipped and zip sizes appear on hover.</div>

```js
display(C.artifactSize(v));
```

## Architecture: arm64 (Graviton) vs x86_64

<div class="chart-sub">Per language and metric, how many of the scenario × memory cells each architecture wins on P50, with cells within ${TIE.pct}% counted as "too close to call" (for cold init, a cell within ${TIE.ms} ms also counts, even past that percentage). The last column gives the median margin of the wins that count. The win counts are best read directly rather than reduced to a single verdict: <strong>cold and warm differ</strong>, and the two columns carry different weight. The cold wins rest on few samples per cell (one per cold cycle) and are suggestive; the warm P50 comparison has far more raw invokes per cell and is more stable than cold, but it is still a thresholded comparison rather than a formal significance test. Warm differences are often sub-millisecond, so the win <em>margin</em> (last column) matters more than the count alone.</div>

```js
display(archWinRate(v));
```

```js
// The per-cell detail behind the win-rate above: one dumbbell row per scenario ×
// memory. It is dense (the verdict is the table), so it is collapsed by default
// and available on demand. Built into a <details> so it stays on this page in
// context rather than living on a separate one.
const archDetail = html`<details class="chart-details">
  <summary>
    Per-cell detail: arm64 vs x86_64 dumbbells (cold &amp; warm)
  </summary>
  <div class="chart-sub">
    Each row is a scenario × memory; the two dots are the architectures (left =
    faster). One panel per language, each on its OWN x-scale (this compares
    arm-vs-x86 within a language, not across languages). Values appear on hover.
  </div>
  <h3>Cold init P50</h3>
  ${C.archDumbbell(v, { metric: "cold" })}
  <h3>Warm P50</h3>
  ${C.archDumbbell(v, { metric: "warm" })}
</details>`;
display(archDetail);
```
