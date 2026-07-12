---
title: "AWS Lambda cold start anatomy: zip vs container, hidden latency"
toc: true
---

# Anatomy of an AWS Lambda cold start

A cold start is not one thing. AWS's own
[execution-environment lifecycle docs](https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtime-environment.html)
break the first invocation of a fresh environment into four sequential steps:

1. **Download your code** into a new execution environment.
2. **Start the execution environment** (the microVM + runtime bootstrap).
3. **Run initialization code** (the Init phase: your static/global code outside the handler).
4. **Run the handler code** (the Invoke phase).

This page walks that timeline from two angles: what the `REPORT` line lets a caller measure
(steps 3 and 4) and what it does not (steps 1 and 2). It then turns to a second effect inside the
measured part, the Init phase running on boosted CPU, which decides how much a cold start costs and
can make two runtimes doing identical work report cold numbers that differ by multiples. Everything
here separates what AWS documents from the observed behavior it rests on.

<div class="tldr">
<div class="tldr-label">The short version</div>

- **Two of the four cold-start steps are invisible to every metric.** Code download and environment start finish _before_ any `REPORT` clock starts, so they land in no `Init Duration`, no `Duration`, and no CloudWatch function metric. [The hidden steps →](#the-hidden-steps-download-environment-start)
- **For a `.zip`, that invisible cost grows with package size:** a flat floor of ~${Math.round(dlFloor)} ms up to a few MB, then a near-linear climb (very roughly 4-8 ms/MB) reaching of order a second and up near Lambda's size limit. A container image inverts this, moving the size cost into the _reported_ `Init Duration` instead. [Zip vs container image →](#zip-vs-container-image-where-the-size-cost-lands)
- **The Init phase appears to run on boosted, roughly full-vCPU CPU; the handler runs at the configured tier's fraction.** Below ~1.8 GB, setup done _at init_ is effectively subsidized. This is observed, not a documented contract, so [don't build on it →](#inside-the-visible-part-init-runs-on-boosted-cpu).
- **Where two runtimes do comparable work at comparable speed, _which phase_ they run it in can dominate their cold gap.** Rust and Go (both fast, compiled, near-equal at the raw setup) are the clean example: Rust front-loads SDK/TLS setup into the boosted init while Go defers the identical work to the metered first request, so their several-fold low-memory gap narrows to ~1.3x once both run it at the same CPU (a dated off-matrix probe, not this site's benchmark data; see below). This is a specific eager-vs-lazy story, not a general rule: a gap rooted in the execution model itself (e.g. JVM startup vs a native binary) persists in any phase. [Same work, different phase →](#the-cross-language-consequence-same-work-different-phase)
- **A crash or timeout re-runs Init, invisibly, on the _next_ invocation.** If a failure ends the runtime process (OOM, timeout, process exit on every runtime tested), the following invocation pays a _suppressed init_ whose duration hides inside its reported `Duration`; a failure the runtime catches (an ordinary handler exception) stays warm. The mapping is runtime-specific at the edges: a Go panic re-inits while a Rust one stays warm, and a Node stack overflow stays warm while it re-inits elsewhere. [When a crash or timeout re-runs Init →](#when-a-crash-or-timeout-re-runs-init-the-suppressed-init)

</div>

<style>
/* "The short version" summary box: the page is long, so its payoff findings
   are surfaced up front as a scannable, linked list. Styled as a quiet
   panel with an accent rail (matching the site's .read-me note), not an alarm. */
.tldr {
  margin: 20px 0 8px; padding: 14px 18px 6px;
  background: color-mix(in srgb, var(--panel) 60%, transparent);
  border: 1px solid var(--frame); border-left: 4px solid var(--accent);
  border-radius: 0 8px 8px 0; max-width: 80ch;
}
.tldr-label { font-size: 12px; letter-spacing: .14em; text-transform: uppercase; color: var(--faint); margin-bottom: 4px; }
.tldr ul { margin: 0; padding-left: 20px; }
.tldr li { margin: 6px 0; line-height: 1.5; }
.tldr a { color: var(--accent); font-weight: 600; white-space: nowrap; }
</style>

## What counts as a "cold start"? AWS's own materials draw the line differently

The term is overloaded, and two AWS diagrams label its boundary differently, so the boundary is
pinned down here before any numbers.

- The [lifecycle docs](https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtime-environment.html)
  color only steps 1-2 (download + environment start) as **"cold start duration"** and put the
  init code _and_ the handler under **"invocation duration."**
- The cold-start engineering blog
  ([Understanding and remediating cold starts](https://aws.amazon.com/blogs/compute/understanding-and-remediating-cold-starts-an-aws-lambda-perspective/))
  draws an **"Initialization Phase"** box that _contains_ container provisioning, runtime
  initialization, function-code loading, and dependency resolution, and calls that whole box the
  latency "commonly referred to as the INIT duration."

So this page pins the boundary to what the work actually does rather than adopting either label.
Two facts do that:

1. **Init code runs only on a cold start.** A warm invoke reuses the initialized environment and
   pays _zero_ init; step 3 has no warm counterpart. So filing init under "invocation duration"
   (as if it were an ordinary per-request cost) is misleading, and it is counted as part of cold
   start almost universally, AWS's own `Init Duration` metric included.
2. **The genuinely separate, unmeasurable-from-the-function part is steps 1-2**: code download and
   environment provisioning. These finish _before_ the reported `Init Duration` value accounts for
   anything and appear in no function metric at all, a claim this page establishes by
   [measurement below](#what-the-report-line-measures-and-what-it-cant). That measurement is also
   what adjudicates the two labels: the blog's "container provisioning ... commonly referred to as
   the INIT duration" folds in cost the reported `Init Duration` metric excludes.

So this site defines its cold-start metric explicitly:
**`init` + the first request's `duration`** (steps 3-4), because that is what the `REPORT` line
reports and what a caller feels once the environment exists, and because runtimes divide one-time
setup between init and the first request differently (see the
[cold start breakdown](./comparison#cold-start-breakdown-init-vs-first-request)).

|                                 | steps 1-2 (download + start) | step 3 (init code) | step 4 (first handler run)     |
| ------------------------------- | ---------------------------- | ------------------ | ------------------------------ |
| runs only when cold?            | yes                          | **yes**            | yes (this run is the cold one) |
| a `REPORT` line reports it?     | **no**                       | `Init Duration`    | `Duration`                     |
| this site's "cold start" metric | not included                 | **included**       | **included**                   |

This split is for a `.zip` archive (this benchmark's packaging): its code is downloaded and unpacked
in steps 1-2, in full, before init begins, so "download" sits squarely in the unreported first
column. A container image splits the cost differently, and gets its own section:
[Zip vs container image](#zip-vs-container-image-where-the-size-cost-lands).

The rest of the page is about the first column, the download + environment start that no
function-side signal reports, and then, inside steps 3-4, why _where_ setup runs changes what it
costs.

```js
import { median } from "./lib/stats.js";
const dl = await FileAttachment("data/lifecycle-download-start.json").json();
// The canonical rust/hello@128 probe row, quoted inline in the section below so
// its magnitudes track the committed data rather than being hand-typed.
const dlHello = dl.cells.find(
  (c) => c.lang === "rust" && c.scenario === "hello" && c.memory_mb === 128,
);
// Floor + large-artifact lift for the "Artifact size" bullet below, DERIVED from
// the SAME committed probe (dl) the table renders, so the stated floor, the
// bundle sizes, and their lift above the floor cannot drift from the table two
// paragraphs down. The floor is the median of the hello-cell residuals (the
// small-artifact rows where download is lost in provisioning), matching how the
// prose-drift guard (scripts/check-lifecycle-prose.py) computes it.
const dlFloor = median(
  dl.cells.filter((c) => c.scenario === "hello").map((c) => c.residual_p50),
);
// The large real bundles the bullet points at: the @512 MB rows whose zip is
// genuinely big (> 13 MB), i.e. Java smithyfull and Python oneclient. Sizes and
// their lift above the floor are read off those cells directly.
const dlBig = dl.cells.filter((c) => c.zip_bytes > 13e6 && c.memory_mb === 512);
const dlBigZipMB = dlBig.map((c) => c.zip_bytes / 1e6).sort((a, b) => a - b);
const dlBigLift = dlBig
  .map((c) => c.residual_p50 - dlFloor)
  .sort((a, b) => a - b);
// The matrix's fattest real artifact, read off the same probe rows (the probe
// deliberately samples the largest bundles), so the two "~N MB" mentions on
// this page track dependency bumps instead of being hand-typed.
const dlMaxZipMB = Math.round(
  Math.max(...dl.cells.map((c) => c.zip_bytes)) / 1e6,
);
```

## What the REPORT line measures, and what it can't

Every other page on this site is built from the Lambda `REPORT` line (Init and Invoke are distinct
lifecycle phases, per the [lifecycle docs](https://docs.aws.amazon.com/lambda/latest/dg/runtimes-context.html); Init
is limited to 10 seconds, and since
[August 1, 2025](https://aws.amazon.com/blogs/compute/aws-lambda-standardizes-billing-for-init-phase/)
is billed uniformly like Invoke time: managed-runtime ZIP functions (Node/Java/Python here) previously
had Init excluded, while custom runtimes on `provided.al2023` (Rust/Go here) already billed it). That
line exposes only steps 3 and 4:

- **`Init Duration`** is step 3, from the start of the function's init code up to the moment the runtime
  signals readiness to the Lambda Runtime API.
- **`Duration`** on the cold invoke is step 4, the handler itself.

The reported `Init Duration` does not fold in steps 1-2; the measured numbers settle that,
independent of how either diagram (above) labels the boundary. Take `rust/hello` (the row in the next
section): a trivial Rust binary reports an `Init Duration` of \~${ms(dlHello.init_p50, 1)}, about what an empty `main` plus
runtime start should cost, yet the caller's wall-clock for the same cold invoke is \~${ms(dlHello.w_cold_p50 - dlHello.warm_rtt_p50)} with the
network path subtracted out. If download + provisioning were counted inside the reported
`Init Duration`, that init would instead carry the whole wall-clock. Netting the reported `Init` and
the cold invoke's own `Duration` out of that wall-clock leaves a residual (\~${ms(dlHello.residual_p50)} here, computed
per-sample rather than by subtracting these medians, see the next section) that, by construction, lands
in **neither** `Init Duration` nor the invoke `Duration`, and for a `.zip` it _grows with artifact size_
(next section) while `init_ms` does not. So the reported `Init Duration`
measures step 3 only; steps 1-2 finish before its clock starts, appear in no `REPORT` field and no
CloudWatch _function_ metric, and surface only in the **caller's wall-clock** around the `Invoke`
call.

The residual is not _purely_ download + environment start: it also carries any cold-specific
scheduling/placement remainder the pre-warm did not cancel. The steady-state per-invoke service
processing (IAM auth, throttle/concurrency check, request routing) is _not_ in it: that work runs on
every warm invoke too, so the `warm_rtt` subtraction removes it, leaving only the work a warm
invoke never pays. What the measurement establishes rigorously is that this residual sits _outside_
the reported `Init Duration`.

<style>
/* The download+start probe table: numeric columns right-aligned with
   tabular figures (matching the site's other data tables), and each cell kept
   on one line so a value never wraps away from its unit. */
table.dlstart td.num { white-space: nowrap; text-align: right; font-variant-numeric: tabular-nums; }
table.dlstart td.resid { text-align: left; }
</style>

## The hidden steps: download + environment start

To put a number on steps 1-2 (which the `REPORT` line can't see), a probe times the `Invoke` call
from the caller's side and subtracts everything that _is_ accounted for. With the SDK's HTTPS
connection to the Lambda data plane already warm (an earlier invoke opened it), it times a
freshly-cold invoke, `W_cold`, then subtracts the in-Lambda cost the `REPORT` line reports and the
warm round-trip:

```
residual  =  W_cold  −  init  −  cold_duration  −  warm_rtt
```

where `warm_rtt` is the median network + invoke-API overhead of subsequent warm invokes of the same
function, each warm invoke's wall-clock minus its **own** `REPORT` `Duration`, so the handler's
processing time is _not_ included. That subtlety matters: `cold_duration` already nets out the cold
invoke's handler work, so netting the warm handler out of `warm_rtt` too means the handler cancels on
both sides and `residual` is the download + start cost whether the handler does ~1 ms or ~1 s of
work. What is left is steps 1 + 2, the code download and environment start, plus a small
provisioning/scheduling remainder that no function-side timing can see. Routine per-invoke
control-plane overhead (auth, throttling) does not inflate this: it is present in both `W_cold` and
`warm_rtt`, so the subtraction removes it, and only the cold-specific placement work survives.

```js
// Value + unit joined with a non-breaking space so a cell never splits
// "225" from "ms" across two lines.
const ms = (x, d = 0) => `${x.toFixed(d)} ms`;
display(
  dl.cells.length === 0
    ? html`<p class="caption">
        No samples yet for this off-matrix download probe.
      </p>`
    : html`<table class="dlstart">
          <thead>
            <tr>
              <th>scenario</th>
              <th>mem</th>
              <th>zip</th>
              <th>W_cold</th>
              <th>init</th>
              <th>cold dur</th>
              <th>warm RTT</th>
              <th><strong>residual (download + start)</strong></th>
            </tr>
          </thead>
          <tbody>
            ${dl.cells.map(
              (c) =>
                html`<tr>
                  <td><code>${c.lang}/${c.scenario}</code></td>
                  <td class="num">${c.memory_mb} MB</td>
                  <td class="num">${(c.zip_bytes / 1e6).toFixed(1)} MB</td>
                  <td class="num">${ms(c.w_cold_p50)}</td>
                  <td class="num">${ms(c.init_p50, 1)}</td>
                  <td class="num">${ms(c.cold_duration_p50, 1)}</td>
                  <td class="num">${ms(c.warm_rtt_p50)}</td>
                  <td class="num resid">
                    <strong>${ms(c.residual_p50)}</strong>
                    <span class="caption"
                      >(${c.residual_min.toFixed(0)}–${c.residual_max.toFixed(0)})</span
                    >
                  </td>
                </tr>`,
            )}
          </tbody>
        </table>
        <div class="caption">
          Median of ${dl.cells[0]?.n_samples ?? 0} cold samples per row (each
          followed by ${dl.n_warm_per_sample} warm invokes whose net round-trip
          is <code>warm_rtt</code>); the residual range in parentheses is
          min–max across those cold samples. Each column is the median of that
          term taken independently, and the <code>residual</code> is the median
          of the per-sample residuals (each computed with the formula above on
          its own sample), so the four left columns need not subtract exactly to
          <code>residual</code>: a median is not additive, and only
          <code>residual</code> is the authoritative decomposition. Measured
          against deployed functions in ${dl.region} (off-matrix probe).
        </div>`,
);
```

The table spans three axes, and the pattern is consistent: **for a `.zip` artifact, artifact size
is the clearest systematic mover of the residual; memory tier is essentially flat, and runtime
family shows no separate fixed floor once size and the wide per-sample range are accounted for.**
(Packaging matters here: a container image splits this cost differently, covered in
[Zip vs container image](#zip-vs-container-image-where-the-size-cost-lands) below.) The zip rows read this way:

- **Memory / vCPU** (the `rust/hello` rows at 128 / 512 / 3008 MB, and the `@512` vs `@3008` pairs):
  the residual is essentially **flat**, consistent with steps 1-2 running before the configured CPU
  allocation takes effect, so provisioning appears not to get the low-tier penalty that the _handler_
  pays. Unlike the init/first-request cost, this part is neither subsidized nor penalized by the
  memory tier.
- **Artifact size** (the `@512 MB` rows, ordered by `zip`): a **fixed floor of ~${Math.round(dlFloor)} ms**
  (the median of the `hello` cells) holds through the low-MB packages, and a **download term only becomes
  visible once the artifact is genuinely large** (the ~${Math.round(dlBigZipMB[0])}-${Math.round(dlBigZipMB[dlBigZipMB.length - 1])} MB Java/Python bundles sit ~${Math.round(dlBigLift[0])}-${Math.round(dlBigLift[dlBigLift.length - 1])} ms
  above the floor). Below a few MB, download is lost in the fixed provisioning cost. The synthetic
  probe below pushes this axis much further.
- **Runtime family / language** (the small-artifact rows, and the realistic SDK-heavy rows across
  the languages, using `smithyfull` where supported and `oneclient`/`authz` for Python and Go):
  `rust` and `go` run on the custom `provided.al2023` runtime while
  `node`/`python`/`java` are managed runtimes, yet at a given artifact size the residuals land on
  the **same floor**, so the provisioning cost looks family- and language-independent, it is the
  download and the environment start, not "which runtime."

The takeaway across all the rows: most of what a caller waits through before the handler begins is
fixed environment-provisioning cost, plus a download term that only bites for large packages, and
none of it appears in any `REPORT` line.

<div class="warning" label="Illustrative magnitudes, measured outside the matrix">

These come from an off-matrix probe timing a handful of already-deployed functions from a **single
client, single account**. The `warm_rtt` subtraction removes the client→region path from the residual
_median_, so the residual is vantage-independent (a caller's own network round-trip sits _on top of_
it, not inside it). It does not remove it per-sample, so the range in parentheses widens with client
distance and control-plane jitter. These are **illustrative magnitudes, not part of the benchmark
matrix**, the same way the [cross-language probes](#the-cross-language-consequence-same-work-different-phase) below are framed.

</div>

<!-- Regenerate the table's data with:
     cargo run -p bencher -- probe download-start   (matrix must be deployed; dist/ must be built)
     which writes a run-scoped results/lifecycle-download-start-<run_id>.json (gitignored);
     this page's data loader discovers the newest one at build time. -->

## How far does download scale?

The table above tops out at the matrix's fattest real artifact (~${dlMaxZipMB} MB), right where the download
term starts to show. To see where it _leads_, a companion probe deploys **synthetic** functions
padded to 1 / 10 / 50 / 100 / 200 MB (a minimal base plus incompressible filler), measures the
same residual, and tears them down. It runs **two runtime families** at each size, Python on the
managed `python3.14` runtime and Rust on the custom `provided.al2023` runtime (reusing the real
`hello` bootstrap), because download and environment start are platform-level work that should not
depend on the runtime: the two series landing on the same curve is the check. Padding with inert
bytes is something the benchmark matrix deliberately never does, because for a _loaded-code_ cold
start (init + first request) filler would measure download rather than code being brought up. But
that is exactly why it is the right instrument _here_: the residual subtraction removes init and the
first request entirely, leaving only the download + environment-start term that padding is meant to grow.

The residual grows with artifact _bytes_ and sits before `init`, so it is byte transfer, "download"
in the loose sense, whatever the platform's caching does underneath. If a replacement environment
reads a locally cached copy instead of re-fetching from origin, the measured slope is only a lower
bound on a true origin download, never an overstatement, so the climb is real either way.

```js
const dscale = await FileAttachment(
  "data/lifecycle-download-scaling.json",
).json();
import * as C from "./components/charts.js";
```

```js
display(C.syntheticDownloadScaling(dscale, invalidation));
```

```js
// Per-run magnitudes quoted in the prose below are DERIVED from the committed
// dscale JSON, never hand-typed, so the "this run measured …" values always match
// the chart the pipeline just refreshed. Shape claims (floor, slope range, ~Nx)
// stay as prose: they are the robust cross-run findings, not one run's numbers.
const dscaleByFam = new Map();
for (const s of dscale.samples ?? []) {
  if (!dscaleByFam.has(s.family)) dscaleByFam.set(s.family, new Map());
  dscaleByFam.get(s.family).set(s.size_mb, s);
}
const dscaleSizes = [
  ...new Set((dscale.samples ?? []).map((s) => s.size_mb)),
].sort((a, b) => a - b);
const dscaleMax = dscaleSizes[dscaleSizes.length - 1];
// Residual p50 at the largest size, formatted as seconds (the "~1.x s" endpoint).
const residSecAt = (fam, mb) => {
  const s = dscaleByFam.get(fam)?.get(mb);
  return s ? (s.residual_p50 / 1000).toFixed(1) : "?";
};
// Init p50 at the smallest size for a family (the flat-init reference the prose cites).
const initMsAt = (fam, mb) => {
  const s = dscaleByFam.get(fam)?.get(mb);
  return s ? Math.round(s.init_p50) : "?";
};
const dscaleMin = dscaleSizes[0];
```

<div class="caption">Two lines per runtime family: the <b>dashed</b> line is the download + start residual (p50, with a min–max band across ${dscale.samples?.[0]?.n_samples ?? 0} cold samples per size), the <b>solid</b> line is the reported <code>Init Duration</code> over the same runs. The dashed residual climbs while the solid <code>Init Duration</code> stays flat. Log-x, at ${dscale.memory_mb} MB / ${dscale.arch}; off-matrix probe in ${dscale.region}.</div>

The shape (for `.zip` archives): a **flat floor of ~${Math.round(dlFloor)} ms up to a few MB** (the same provisioning
floor the table above shows, measured here by an independent probe, download is lost in it), then,
once the package grows past that, a **near-linear climb** of very roughly **4-8 ms per MB**, reaching
of order **a second and up at 200 MB** (the exact slope and endpoint move run to run; this run measured
Python ~${residSecAt("python", dscaleMax)} s, Rust ~${residSecAt("rust", dscaleMax)} s at ${dscaleMax} MB). Both runtime families show the **same shape**: the flat
floor up to a few MB, then the near-linear climb. That shared shape (managed Python and custom-runtime
Rust rising the same way) is the signature of platform-level byte transfer, not a per-runtime code path.
The two lines are not identical run to run, but at every size their min–max bands overlap and the gap
between their medians stays smaller than the spread _within_ either family, so with only
${dscale.samples?.[0]?.n_samples ?? 0} cold samples per point that gap is within noise, not a runtime effect. The robust, reproducible
result is the shared flat-floor→linear-climb shape, and which family reads slightly higher is not
stable enough to attribute to the runtime. So for a large `.zip` package the download term dominates cold start
outright, and it is latency **no `REPORT` line, no `Init Duration`, no CloudWatch function metric
ever shows**, a concrete reason to keep deployment packages lean. (Keeping packages lean helps a
container image too, but for a different reason: there the cost lands in the _reported_
`Init Duration`, not this invisible residual, as the next section shows.)

The **solid line on the chart above** makes this concrete: across the whole 1-to-200 MB sweep the
reported **`Init Duration` stays flat** (Rust ~${initMsAt("rust", dscaleMin)} ms, Python ~${initMsAt("python", dscaleMin)} ms, no upward trend, hover any
point to read it) while the dashed residual climbs by roughly an order of magnitude (~8-14x run to
run). If code download were folded into init, that
solid line would climb with the dashed one and a 200 MB package's init would be seconds; it does not,
so the growth lands entirely outside the metric.

Same "illustrative, environment-dependent, single-account" caveat as the table above; the _shape_
(flat floor → ~linear climb, family-independent) is the robust result, the exact ms are
environment-specific.

<!-- Regenerate with: cargo run -p bencher -- probe download-scaling
     which deploys ephemeral padded functions (--sizes defaults to 1,10,50,100,200),
     measures, tears them down, and writes results/lifecycle-download-scaling-<run_id>.json
     (gitignored; the newest is discovered at build time). -->

## Zip vs container image: where the size cost lands

Everything above is measured on `.zip` archives (this benchmark's matrix is zip-only). But the
`.zip`-vs-container-image choice changes _where in the timeline_ artifact size is paid, and it is a
clean inversion. The mechanism behind it is documented by AWS in Brooker et al.,
[_On-demand Container Loading in AWS Lambda_](https://arxiv.org/abs/2305.13162): unlike a `.zip`
(downloaded and unpacked in full before the microVM starts), a container image is chunked, cached,
and **demand-loaded lazily at block level**, so only the blocks actually touched are fetched (the
figures below are from that paper). The same download-scaling instrument, re-run with a
container-image family (padded images on the managed Python base, deployed two ways from one image:
one that never reads the padding, one that reads it all at init), draws the picture. The convention:
**solid line = reported `Init Duration`, dashed line = the unreported pre-init residual.**

```js
const dimg = await FileAttachment(
  "data/lifecycle-download-scaling-image.json",
).json();
```

```js
display(C.zipVsImageDownloadScaling(dimg, invalidation));
```

<div class="caption">Reported <code>Init Duration</code> (solid) and unreported pre-init residual (dashed) vs added artifact size, log-x, at ${dimg.memory_mb} MB / ${dimg.arch}, p50 of ${dimg.samples?.[0]?.n_samples ?? 0} cold samples per point. The zip series here is the managed-Python <code>.zip</code> family measured in the <em>same run</em> as the image series (so all lines share one session, client vantage, and date, making them directly comparable). The publish pipeline writes this chart's data and the chart above in one invocation, so the shared zip family matches. Image series are one container image per size (base ≈${(dimg.base_image_bytes_est / 1e6).toFixed(0)} MB + padding), deployed with the added padding read at init or left untouched. Off-matrix probe in ${dimg.region}.</div>

```js
// Magnitudes quoted in the prose + table below are DERIVED from the committed
// probe JSON (dimg), never hand-typed, so they cannot drift from the chart when
// the publish pipeline refreshes the data. All are p50s of the two series the
// chart draws: image-touched (init that climbs) and the co-measured zip baseline.
const imgTouched = new Map(
  (dimg.samples ?? [])
    .filter((s) => s.family === "image-touched")
    .map((s) => [s.size_mb, s]),
);
const zipBaseline = new Map(
  (dimg.zip_baseline ?? []).map((s) => [s.size_mb, s]),
);
const imgSizes = [...imgTouched.keys()].sort((a, b) => a - b);
const minSize = imgSizes[0];
const maxSize = imgSizes[imgSizes.length - 1];
const mround = (x) => Math.round(x);
// Touched Init Duration p50 at a given added size (the "solid line that climbs").
const touchedInit = (mb) => mround(imgTouched.get(mb)?.init_p50 ?? 0);
// Summed cold overhead (init + residual p50) for one size, both series.
const totalRow = (mb) => {
  const z = zipBaseline.get(mb);
  const t = imgTouched.get(mb);
  return {
    mb,
    zip: z ? mround(z.init_p50 + z.residual_p50) : null,
    img: t ? mround(t.init_p50 + t.residual_p50) : null,
  };
};
// Image residual floor (the "dashed line that stays flat"): min–max p50 across
// all sizes/variants, so the "flat floor" range is read off the data.
const imgResiduals = (dimg.samples ?? []).map((s) => s.residual_p50);
const residFloorLo = mround(Math.min(...imgResiduals));
const residFloorHi = mround(Math.max(...imgResiduals));
```

Across the four lines the inversion is clear:

- **Zip hides its size cost.** The zip's _dashed_ line (the unreported download) climbs ~linearly to
  of order a second at 200 MB, while its _solid_ `Init Duration` stays flat. A `.zip` is downloaded
  and unpacked in full _before_ the microVM does anything, so the added bytes land in the phase **no
  metric reports** (above the fixed provisioning floor).
- **An image surfaces its size cost in `Init Duration`.** The image's _dashed_ residual stays a flat
  ~${residFloorLo}-${residFloorHi} ms floor at every size, and its _solid_ init is the line that climbs, but only for the
  variant that **reads the added padding at init** (touched: ~${touchedInit(minSize)} ms at ${minSize} MB → ~${touchedInit(maxSize)} ms at ${maxSize} MB).
  The variant that never touches the padding stays flat on both lines. So for an image the
  size-dependent cost is **reported** in `Init Duration`, to the extent the code is loaded at
  startup, and a bigger image realistically does load more.
- **The invisible part never goes away, it just stops growing.** Even the image pays a flat
  ~${residFloorLo}-${residFloorHi} ms unreported pre-init residual (microVM start + the minimal boot blocks). The difference
  from `.zip` is that this floor is _flat_ with size rather than climbing: the hidden cost is bounded,
  not proportional to the package.
- **Total cold overhead (init + residual), the two lines summed** (each point's value is available on hover): the
  `.zip` starts lower (it has no base-layer floor) but the image pulls ahead as size grows, with the
  crossover in the low tens of MB. The table below is built from the same committed p50s the chart
  draws (the smallest- and largest-size rows are bold, the two ends of the crossover):

```js
// Total-overhead table, DERIVED from dimg (never hand-typed): summed init+residual
// p50 per size for the zip baseline and the touched image. Bolds the crossover
// extremes (smallest size, where zip wins; largest, where the image wins).
display(
  html`<table class="dlstart">
    <thead>
      <tr>
        <th>Added size</th>
        <th><code>.zip</code> total</th>
        <th>image total</th>
      </tr>
    </thead>
    <tbody>
      ${imgSizes.map((mb) => {
        const r = totalRow(mb);
        const edge = mb === minSize || mb === maxSize;
        const cell = (v) =>
          edge ? html`<strong>~${v} ms</strong>` : html`~${v} ms`;
        return html`<tr>
          <td class="num">${mb} MB</td>
          <td class="num">${cell(r.zip)}</td>
          <td class="num">${cell(r.img)}</td>
        </tr>`;
      })}
    </tbody>
  </table>`,
);
```

### The image download is cheaper because loading is lazy

The [Brooker et al.](https://arxiv.org/abs/2305.13162) paper gives the mechanism in detail: images are
flattened to a filesystem, chunked into 512 KiB content-addressed blocks, deduplicated (convergent
encryption), cached in a three-tier cache, and **demand-loaded lazily at block level** through a
virtio block device. That lazy, block-level loading is what flattens the _residual_: microVM start
pulls only the blocks needed to boot, and the probe's padding is not among them regardless of size, so the
pre-init term stays flat. In the touched variant the handler reads `padding.bin` at import time, so
those blocks fault in _while the Init Duration clock is running_, so the image's download of that code
is counted **inside** `Init Duration`, which is exactly why touched-init climbs with size
while the residual does not. (This is the mechanism behind the table's `.zip`-vs-image footnote near
the top of the page: for an image, part of the code "download" is reported, in step 3.) A `.zip`, by
contrast (the paper's "first generation" architecture, still how `.zip` functions work), has the
worker **"[download] the function image (a `.zip` file up to 250MiB in size) from Amazon S3, and
[unpack] it"**, and it **"requires the full archive to be downloaded and unpacked before the new
MicroVM can do any work."** There is no lazy path: the whole artifact is on the critical path every
cold start.

The chunks the image _does_ fetch come from a cache close to the worker, not S3, most of the time.
The paper measures the gap directly: an availability-zone-level cache hit takes **"a median time of
550µs, versus 36ms for a fetch from the origin in S3,"** and, populated on demand (a worker miss pulls
from the AZ cache; an AZ miss downloads from S3 and uploads into the cache), the cache carries the
vast majority of production traffic: **"a median of 67% of chunks were loaded from the on-worker
cache, 32% from the AZ-level distributed cache, and the remaining 0.06% from the backing store"**
(S3). So an image's blocks are usually served from a warm nearby cache at ~550µs, where a `.zip`'s
bytes are a cold S3 download on the pre-init path. That cache-vs-S3 gap, plus the image fetching only
the blocks it touches, is the documented mechanism; it is consistent with the flat image residual
against the `.zip`'s climb. (The cache tier that served the probe's chunks was not measured, so the
exact split for these runs is not something this data can attribute.)

### The padding is a deliberate worst case, so a real image differs

The probe pads with _incompressible,
unique_ bytes and the touched variant reads all of them, which defeats the two properties that make
real images cheap to transfer: those bytes are ~100% unique chunks (they deduplicate against nothing)
and 100% of them are read at startup. A real image is the opposite, which is where two figures from
the paper apply. On sparsity it quotes prior work (Harter et al):
**"on average only 6.4% of container data is needed at startup."** On deduplication it reports its own
Lambda measurement: **"[a]pproximately 80% of newly uploaded Lambda functions result in zero unique
chunks"** (the paper attributes this to CI/CD systems re-uploading images that were already uploaded
before, so every chunk is already stored). So on the _transfer/download_ axis a real large image should move far less
than this worst-case padding, which is why the flat image residual here is a conservative result for
that term. It does **not** follow that a real image's _total_ cold start is lower: a real image still
runs its actual init code (module loads, client construction) in the Init phase, which this benchmark's trivial
handler does not. This run did not measure a large _real_ container image (the biggest real artifact in
this benchmark is ~${dlMaxZipMB} MB), so treat "real images transfer less" as reasoning from the cited paper,
not a measurement here. The two effects pull in opposite directions and which dominates depends on
the specific image, so the total-overhead numbers above are **not** a prediction for any real
function: a real workload should be **measured both ways** rather than transferring these padding
numbers onto it.

### At a realistic ~11 MB size, the two are within noise

The chart uses _inert padding_
to push size far past any real artifact; below ~10-20 MB there is little to win, and an image carries
a fixed base-layer floor (≈${(dimg.base_image_bytes_est / 1e6).toFixed(0)} MB here) a lean `.zip` does not.

> **Dated one-off characterization (taken 2026-07, arm64/512 MB, single account; not committed
> data).** For a real Rust `smithyfull` binary (~10.6 MB uncompressed, ~4.9 MB zip; not
> padding), zip and image cold-started within noise of each other (~250-270 ms total). The image
> advantage is a _large-artifact_ effect, not a universal win.

### Memory tier moves neither phase's size cost

Raising the memory tier does not rescue the `.zip` either: its size cost is the _unreported_
download residual, which the table above shows is flat across tiers, consistent with steps 1-2
running before the configured CPU allocation takes effect, so a bigger tier does not shrink it. The image's size cost is the opposite phase, the
_reported_ init growth, but that runs in the Init phase, which (next section) runs on a boosted,
roughly full-vCPU allocation regardless of the configured memory tier, so it too is tier-independent.
The committed data above is a single tier (${dimg.memory_mb} MB), so it does not itself establish
that; a separate check did.

> **Dated one-off characterization (taken 2026-07, arm64, single account; not committed data).** A
> memory sweep of the 200 MB touched image across 128 MB → 3 GB found its `Init Duration` essentially
> flat (~400 ms at every tier; a separate run from the committed 200 MB chart point above, which
> reflects its own run's magnitude), and a `.zip`'s init flat too once the artifact is not
> pathologically large. Consistent with the Init-phase CPU boost the next section documents: memory
> tier does not move either phase's size cost; only packaging does.

<div class="caption">Same off-matrix caveat as the zip probes above (single client, single account, illustrative magnitudes, not matrix data). Both series are measured in the same in-region run, so the two are directly comparable. The <em>shapes and direction</em> are the robust result (zip residual climbs, image init climbs when loaded, image residual flat), not the exact ms.</div>

<!-- Refreshed by the publish pipeline (deploy/run-benchmark.sh's download-scaling step) alongside the zip families:
     cargo run -p bencher -- probe download-scaling --with-image
     which additionally assembles+pushes one padded container image per size with crane (base pull +
     one-layer append + CMD + push to ECR, no container-build daemon), deploys untouched+touched
     functions from each, measures, tears them down, and writes
     results/lifecycle-download-scaling-image-<run_id>.json (gitignored; newest discovered at build). -->

## Inside the visible part: Init runs on boosted CPU

The remaining sections stay inside steps 3-4, the part the `REPORT` line _does_ report. Here the
decisive effect is not _whether_ work runs but _which phase_ it runs in. Two facts frame it, both in
AWS's docs:

- **CPU scales with memory.** Lambda allocates CPU **proportionally to the configured memory**: a
  full vCPU at 1,769 MB, a fraction of a vCPU below that
  ([memory/CPU docs](https://docs.aws.amazon.com/lambda/latest/dg/configuration-memory.html)). At
  128 MB that fraction is roughly 0.07 vCPU, so CPU-bound work runs on the order of 14x slower than
  on a full core.
- **Init and Invoke are distinct phases** (above).

What the docs do **not** state is that the two phases run on _different_ amounts of CPU.
Measurements on this site and elsewhere are consistent with the Init phase running on a
**boosted CPU, roughly a full vCPU regardless of the configured tier**, dropping to the tier's
fractional allocation only when the handler starts. The clearest statement of it comes from
AWS itself: in [re:Invent 2019, "Best practices for AWS Lambda and Java" (SVS403-R1)](https://www.youtube.com/watch?v=ddg1u5HLwg8&t=718s),
a slide labels the runtime-start and handler-class-init steps **"Boosted host CPU access (up to
10 seconds)"** and the handler-method step **"Throttled CPU access,"** with the presenter
noting they do this "because we want the functions to start faster."

The tier's fractional allocation is the **baseline the function is configured for and billed
at**, not a penalty. The Init phase is the anomaly, running _above_ that baseline; the handler
simply runs _at_ it.

In these measurements, work done _before_ the handler is reached, resolving credentials and
configuration, running a first TLS handshake, or any CPU-heavy static initialization, behaves as if
it runs on the boosted CPU; the same work moved _into_ the handler behaves as if it runs at the
tier's ordinary allocation, which below ~1.8 GB is a fraction of a vCPU and therefore much slower.

<div class="warning" label="Observed behavior, not a contract">

The boost is not in the official
[execution-environment docs](https://docs.aws.amazon.com/lambda/latest/dg/runtimes-context.html);
the primary source is a 2019 conference talk, and the "up to 10 seconds" it cites lines up
with the documented 10-second Init limit. It has held consistently in measurement, but AWS
does not commit to it, so **a system should not depend on it**. What follows is a
way to read the measured numbers, not advice to build around an undocumented mechanism.

</div>

## Why it matters: work placed at init is subsidized

Below ~1.8 GB, moving one-time setup into the Init phase makes it faster, because it runs on
the boosted CPU rather than the tier's ordinary fraction. The effect is largest at the smallest
tiers, where the gap between the boosted and baseline allocations is widest, and shrinks toward
zero as the configured memory approaches a full vCPU (at which point init and invoke run on
roughly the same CPU and placement stops mattering).

This site already shows the effect _within a single runtime_: the [Rust jitter-entropy
panel](./rust#the-aws-lc-rs-jitter-entropy-cold-start-tax) measures the identical one-time
`aws-lc-rs` seeding cost landing in two different phases. When the first TLS handshake happens
at init (`lettercount`), the cost is a **flat bump** in `init_ms` that stays roughly constant
across tiers, paid cheaply on the boosted CPU. When it happens in the handler (`oneclient`), the
same cost becomes a **cliff** in the invoke duration that grows steeply as memory shrinks, consistent with the phase CPU difference.

## The cross-language consequence: same work, different phase

This effect is largest _across_ languages, and is subtler than
"one runtime is faster." Take **Rust and Go**, two of the runtimes measured here. Their AWS SDKs
can do the **same** expensive one-time setup, building an HTTPS client: load the operating
system's root-certificate store, assemble the TLS stack, open the first connection, and still
post very different cold starts, purely because they run that setup in **different lifecycle
phases**. The boost covers one and not the other.

Constructing the SDK client objects is not the difference (that costs well under 1 ms in both
Rust and Go). The expensive part is standing up the HTTPS machinery (building the connector and
loading the OS certificate store), and the two SDKs schedule it in opposite phases. A dated one-off
set of isolating probes (taken 2026-07, single account, 128 MB arm64, timed internally, not committed
data) makes this concrete; the ms below are rough magnitudes, not reproducible from the committed
dataset.

**Rust resolves it eagerly, at init.** The probes show `aws_config::load().await` building the default HTTPS client
while wiring up the credential providers, so the cost lands in the boosted Init phase:

| Rust `aws_config::load()` at init           | median   |
| ------------------------------------------- | -------- |
| default (builds the HTTPS client)           | ~63 ms   |
| with a no-op HTTP client supplied           | ~0.1 ms  |
| with static credentials (no provider chain) | ~0.03 ms |

Swapping in a no-op HTTP client collapses `load()` from ~63 ms to ~0.1 ms, so essentially the
entire cost _is_ building that client. About ~18 ms of it is loading and parsing the OS root
certificates; the rest is the TLS/connector assembly. Constructing the `aws-lc-rs` crypto
provider is negligible (~0.001 ms); that is separate from the one-time RNG **seeding** the
[jitter-entropy panel](./rust#the-aws-lc-rs-jitter-entropy-cold-start-tax) measures, which fires
later on the first TLS handshake and is switched off in these probes via
`AWS_LC_SYS_NO_JITTER_ENTROPY=1` (the site default). Run the same `load()` in the handler instead,
on the tier's ordinary CPU rather than the init boost, and it rises to ~725 ms, an ~11x jump.

**Go resolves it lazily, on the first request.** The probes show `LoadDefaultConfig` and `NewFromConfig` doing almost
nothing eagerly (~0.3 ms); the HTTP transport (built once via `sync.Once`), the OS cert pool
(loaded once via the standard library's `crypto/x509`), and the first credential retrieval all
fire on the **first service call**, in the Invoke phase at the tier's ordinary CPU. Timing the
first AWS call against the second on a cold sandbox:

| First AWS call on a cold sandbox                                | Go           | Rust    |
| --------------------------------------------------------------- | ------------ | ------- |
| 1st call: one-time setup **plus** the request round trip        | ~1290 ms     | ~113 ms |
| 2nd call: round trip only, everything now warm                  | ~13 ms       | ~6 ms   |
| one-time setup alone (what the 1st call pays and the 2nd skips) | **~1277 ms** | ~107 ms |

The last row is the first two subtracted: the 2nd call does only the request round trip (transport,
cert store, TLS session and credentials are all warm by then), so the 1st call minus the 2nd
isolates the one-time setup the first request had to do. For Go that is ~1.3 s, paid in the handler
at the tier's ordinary CPU. Rust's first call is only ~113 ms because it already built the client
and cert store at init; the ~107 ms it still pays on call one is mostly the unavoidable first TLS
handshake, which neither SDK performs before the first request.

So the boost does not make Rust cheaper _at the work_; it lets Rust do the same expensive setup in
the one window where 128 MB gets a full vCPU, while Go does near-identical work an order of
magnitude slower at the tier's ordinary allocation. It is a **lifecycle-placement effect**: the
total one-time work is comparable between the two, but Rust front-loads it into the boosted phase and Go defers
it into the metered one. This is also why forcing Rust's setup _into_ the handler erased most of
its cold advantage in a companion test: Rust's cold rose sharply while Go's barely moved.

Two caveats bound this. First, even on fully equal footing (all setup forced into the
handler, so both run at the tier's ordinary CPU), Rust still finished ahead: in that companion
test its cold total was ~1150 ms against Go's ~1540 ms, about 1.3x, versus a several-fold
gap in the idiomatic init-placement case. So Rust has a _separate_, smaller per-request efficiency
edge on top of the placement effect, but placement is the larger term. (Those figures are from
the same single-account probes, not the site's benchmark data; treat them as rough magnitudes.)
Second, the eager-init pattern is a genuine production advantage for Rust, not merely a
benchmarking artifact: an idiomatic Rust
handler really does get its setup subsidized on every cold start below ~1.8 GB. The point is only
that a raw cold number folds together _where_ the work runs and _how fast_ it runs, and at low
memory tiers the placement term is large.

## When a crash or timeout re-runs Init: the suppressed init

Everything above is about the _first_ cold start of an environment. There is a second way to pay
init, and the `REPORT` line hides it too, though differently: not by omitting it (as with the
download + start cost above) but by folding it into an ordinary invoke's `Duration`. **An invocation
that kills the sandbox forces the next invocation to re-run the whole Init phase.** AWS documents this in the
[lifecycle docs](https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtime-environment.html)
under _Failures during the invoke phase_: on an invoke failure Lambda "performs a reset" that
"behaves like a `Shutdown` event", and "if this environment is used for a new invocation, Lambda
re-initializes the extension and runtime together with the next invocation." That re-initialization
is called a **suppressed init**, and the docs are explicit that it gets no separate log line: the
`REPORT` line reads as one slow invoke rather than an init plus an invoke.

The trigger is narrower than "the invocation failed": it is not the error, it is whether the
**runtime process survives**. If the failure ends the OS process, the next invoke re-runs Init; if the
runtime catches it and reports it over the Runtime API, the process stays alive and the environment
stays warm. Some failures land the same way on every runtime:
an **OOM**, a **function timeout**, and an explicit **process exit** end the process (so they re-init)
on all five runtimes tested here (Java, Rust, Node, Python, Go), while an **ordinary handler
exception or returned error** stays warm on all five. This was confirmed by direct test: a probe
carrying a static id generated at init reports an unchanged id when the same warm process served the
next invoke, and a changed id when the process was replaced and Init re-ran.

The language-level mapping to those two buckets is otherwise **runtime-specific**, and two cases
diverge from the rest:

- **A panic re-inits on Go but stays warm on Rust.** The Rust runtime wraps the handler in
  `catch_unwind`, reports the panic as an invocation error, and loops for the next event; `aws-lambda-go`
  recovers a panic only to report it (with its stack trace) over the Runtime API, then deliberately
  exits the process, so the next invoke pays a suppressed init.
- **A stack overflow stays warm on Node but re-inits everywhere else.** In Node it surfaces as a
  catchable `RangeError` ("Maximum call stack size exceeded") that leaves the process alive; on Java
  (a `StackOverflowError`, one of the `VirtualMachineError`s that always kill the runtime alongside
  `OutOfMemoryError`), Python, Rust, and Go a genuine stack overflow aborts the process, so it
  re-inits. Python needs the qualifier "genuine": ordinary deep recursion raises a catchable
  `RecursionError` and stays warm; only an overflow that defeats the interpreter's recursion guard
  (C-level recursion, or a recursion limit raised past the real stack) aborts the process.

<div class="warning" label="The re-init is boosted, but its budget differs from a cold start's">

A first cold start's Init phase gets its own budget, separate from the function timeout (10 seconds
for a standard function; up to 130 seconds or the configured timeout for provisioned-concurrency and
SnapStart functions). Verified off-matrix: a function with an **8-second init and a 3-second timeout**
still cold-started successfully, its first invoke reporting `Init Duration: ~8.1 s` with no timeout.
The suppressed init that follows a crash is different in _where_ it is accounted, not in _speed_:
even though it happens on the invoke path and bills as `Duration`, it still gets the
[Init-phase CPU boost](#inside-the-visible-part-init-runs-on-boosted-cpu) (the same CPU workload
re-run in a post-crash init finished within a few percent of a normal cold init, and roughly an order
of magnitude faster, ~13-16x across runs, than the throttled handler at 128 MB), so it runs at the
function's normal cold `Init Duration`. The lever for either is the same one the rest of this page is about: a
lighter init.

Note that catching downstream errors _inside_ the handler only helps for the recoverable-error class
above; it cannot save a failure that ends the process, such as an actual OOM or a hard timeout, both
of which re-init on every runtime regardless.

</div>

A warm sandbox pays init once and serves many invocations, so on the mean the suppressed init is
diluted. But it does not disappear, it moves into the **latency tail (P90/P99)**: the invocation that
follows a crash or timeout pays init **again**, and that init hides inside a `Duration` that looks
like a merely-slow invoke. Unlike a genuine cold start, no `REPORT` field marks it as init: it emits no
`Init Duration` (or `Restore Duration`) line, so it is not even reported as a cold start, just a slow
invoke. (The one signal that does mark it: an extension consuming the
[Telemetry API](https://docs.aws.amazon.com/lambda/latest/dg/telemetry-api.html) receives
`INIT_START`/`INIT_REPORT` events with `phase=invoke` for a suppressed init, so it is observable, but
only with that instrumentation in place.) For SnapStart the same
crash has a sharper edge, covered on the [SnapStart page](./java-snapstart#a-snap-start-crash-or-timeout-re-runs-init-it-does-not-re-restore): the recovery is a full JVM init, **not** a snapshot restore, so the crash silently drops the function off the SnapStart fast path.

## How the cold numbers here should be read

- **What the site's "cold start" includes.** It is `init` + first-request `duration` (steps 3-4),
  the part the `REPORT` line reports. It does **not** include the download + environment-start
  (steps 1-2) measured above; a caller waits through those too, but no function metric exposes them,
  so they are documented here separately rather than folded into the headline number.
- **What a low-tier cold gap reflects.** Every function on this site builds its clients at init in
  each language's idiomatic style, so the published cold starts already embed the placement effect.
  That is representative of well-written handlers, but it means a cold gap at 128 or 256 MB
  partly reflects _where_ each runtime places its setup, not only how fast it runs. The gap is
  expected to narrow at higher tiers.
- **For a handler where cold start matters below ~1.8 GB.** Setup work runs faster at init
  than on the first request (for a lazy SDK, that can require a small real call, not just
  constructing a client), and a higher memory tier runs the phases on comparable CPU. Neither
  substitutes for measuring the specific workload, and neither is a guarantee given the
  mechanism is undocumented.
