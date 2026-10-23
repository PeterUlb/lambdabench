---
title: "AWS Lambda cold start anatomy: zip vs container, hidden latency"
toc: true
---

# Anatomy of an AWS Lambda cold start

AWS's own
[execution-environment lifecycle docs](https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtime-environment.html)
break the first invocation of a fresh environment into four sequential steps:

1. **Download your code** into a new execution environment.
2. **Start the execution environment** (the microVM + runtime bootstrap).
3. **Run initialization code** (the Init phase: your static/global code outside the handler).
4. **Run the handler code** (the Invoke phase).

This page walks that timeline from two angles: what the `REPORT` line lets a caller measure
(steps 3 and 4) and what it does not (steps 1 and 2). It then turns to a second effect inside the
measured part, the Init phase running on boosted CPU, which decides how much a cold start costs and
can make two runtimes doing identical work report cold numbers that differ by multiples. Throughout,
the page marks which statements AWS documents and which are observed behavior.

<div class="tldr">
<div class="tldr-label">The short version</div>

- **Two of the four cold-start steps are invisible to every metric.** Code download and environment start finish _before_ any `REPORT` clock starts, so they land in no `Init Duration`, no `Duration`, and no CloudWatch function metric. [The hidden steps →](#the-hidden-steps-download-environment-start)
- **Two terms turn any number on this site into an end-to-end caller wait.** No metric itemizes a "latency excluding network" number (network and Lambda's per-invoke front-end share one un-itemized wall-clock), but for a same-region caller: cold wait ≈ the cold start + the ~100 ms-scale residual + a small front-end lump, and warm wait ≈ the warm duration + the same lump, both terms measured below. [What a caller waits for end to end →](#what-a-caller-waits-for-end-to-end)
- **For a `.zip`, the invisible download+start cost grows with package size:** a flat floor of ~${Math.round(dlFloor)} ms up to a few MB, then a near-linear climb (very roughly 3-8 ms/MB) reaching of order a second and up near Lambda's size limit. A container image inverts this, moving the size cost into the _reported_ `Init Duration` instead. [Zip vs container image →](#zip-vs-container-image-where-the-size-cost-lands)
- **The Init phase appears to run on boosted, roughly full-vCPU CPU; the handler runs at the configured tier's fraction.** Below ~1.8 GB, setup done _at init_ is effectively subsidized. This is observed, not a documented contract, so [don't build on it →](#init-runs-on-boosted-cpu).
- **Where two runtimes do comparable work at comparable speed, _which phase_ they run it in can dominate their cold gap.** Rust and Go (both fast, compiled, near-equal at the raw setup) are the clean example: Rust front-loads SDK/TLS setup into the boosted init while Go defers the identical work to the metered first request, so their several-fold low-memory gap narrows to ~1.3x once both run it at the same CPU (a dated off-matrix probe, not this site's benchmark data; see below). This is a specific eager-vs-lazy effect, not a general rule: a gap rooted in the execution model itself (e.g. JVM startup vs a native binary) persists in any phase. [Same work, different phase →](#rust-vs-go-same-work-different-phase)
- **A crash or timeout re-runs Init, invisibly, on the _next_ invocation.** If a failure ends the runtime process (OOM, timeout, process exit on every runtime tested), the following invocation pays a _suppressed init_ whose duration hides inside its reported `Duration`; a failure the runtime catches (an ordinary handler exception) stays warm. The mapping is runtime-specific at the edges: a Go panic re-inits while a Rust one stays warm, and a Node stack overflow stays warm while it re-inits elsewhere. That re-init has its own limit, the function timeout plus up to 10 s (at most 20 s, or the timeout if longer), and an init longer than that leaves the environment failing every invoke; with provisioned concurrency or SnapStart that limit is far below what their first init allows. [When a crash or timeout re-runs Init →](#when-a-crash-or-timeout-re-runs-init-the-suppressed-init)

</div>

<style>
/* "The short version" summary box: the page is long, so its main findings
   are listed up front, each with a link. Styled as a muted panel with an
   accent rail, matching the site's .read-me note. */
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

## What "cold start" means here

The term is overloaded: two AWS diagrams label its boundary differently.

- The [lifecycle docs](https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtime-environment.html)
  color only steps 1-2 (download + environment start) as **"cold start duration"** and put the
  init code _and_ the handler under **"invocation duration."**
- The cold-start engineering blog
  ([Understanding and remediating cold starts](https://aws.amazon.com/blogs/compute/understanding-and-remediating-cold-starts-an-aws-lambda-perspective/))
  draws an **"Initialization Phase"** box that _contains_ container provisioning, runtime
  initialization, function-code loading, and dependency resolution, and calls that whole box the
  latency "commonly referred to as the INIT duration."

So this page adopts neither label and pins the boundary to what the work does, using two
facts:

1. **Init code runs only on a cold start.** A warm invoke reuses the initialized environment and
   pays _zero_ init; step 3 has no warm counterpart. So filing init under "invocation duration"
   (as if it were an ordinary per-request cost) is misleading, and it is counted as part of cold
   start almost universally, AWS's own `Init Duration` metric included.
2. **The separate part, which the function cannot measure, is steps 1-2**: code download and
   environment provisioning. These finish _before_ the reported `Init Duration` value accounts for
   anything and appear in no function metric at all, a claim this page establishes by
   [measurement below](#what-the-report-line-measures-and-what-it-cant). The same measurement shows
   where the blog's label goes wrong: its "container provisioning ... commonly referred to as the
   INIT duration" includes cost that the reported `Init Duration` metric excludes.

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

```js
import { median } from "./lib/stats.js";
const dl = await FileAttachment("data/lifecycle-download-start.json").json();
// The canonical rust/hello@128 probe row, quoted inline in the section below so
// its magnitudes track the published data rather than being hand-typed.
const dlHello = dl.cells.find(
  (c) => c.lang === "rust" && c.scenario === "hello" && c.memory_mb === 128,
);
// Floor + large-artifact lift for the "Artifact size" bullet below, DERIVED from
// the SAME published probe (dl) the table renders, so the stated floor, the
// bundle sizes, and their lift above the floor cannot drift from the table two
// paragraphs down. The floor is the median of the hello-cell residuals (the
// small-artifact rows where download is lost in provisioning), matching how the
// prose-drift guard (scripts/check-lifecycle-prose.py) computes it.
const dlFloor = median(
  dl.cells.filter((c) => c.scenario === "hello").map((c) => c.residual_p50),
);
// The large real bundles the bullet points at: the @512 MB rows whose zip is
// big (> 13 MB), i.e. Java smithyfull and Python oneclient. Sizes and
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

Every other page on this site is built from the Lambda `REPORT` line, and that line exposes only
steps 3 and 4:

- **`Init Duration`** is step 3, from the start of the function's init code up to the moment the runtime
  signals readiness to the Lambda Runtime API.
- **`Duration`** on the cold invoke is step 4, the handler itself.

Init and Invoke are distinct lifecycle phases, per the
[lifecycle docs](https://docs.aws.amazon.com/lambda/latest/dg/runtimes-context.html). Init is limited
to 10 seconds, and since
[August 1, 2025](https://aws.amazon.com/blogs/compute/aws-lambda-standardizes-billing-for-init-phase/)
it is billed uniformly like Invoke time. Before that, managed-runtime ZIP functions (Node/Java/Python
here) had Init excluded, while custom runtimes on `provided.al2023` (Rust/Go here) already billed it.

The reported `Init Duration` does not fold in steps 1-2; the measured numbers settle that,
independent of how either diagram (above) labels the boundary. Take `rust/hello` (the row in the next
section): a trivial Rust binary reports an `Init Duration` of \~${ms(dlHello.init_p50, 1)}, about what an empty `main` plus
runtime start should cost, yet the caller's wall-clock for the same cold invoke is \~${ms(dlHello.w_cold_p50 - dlHello.warm_rtt_p50)} with the
warm round-trip subtracted out. If download + provisioning were counted inside the reported
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
table.dlstart th.num { text-align: right; }
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
processing time is _not_ included. `cold_duration` already nets out the cold invoke's handler work,
so netting the warm handler out of `warm_rtt` too means the handler cancels on both sides and
`residual` is the download + start cost whether the handler does ~1 ms or ~1 s of work. What is left
is steps 1 + 2, the code download and environment start, plus a small provisioning/scheduling
remainder that no function-side timing can see. Routine per-invoke control-plane overhead (auth,
throttling) does not inflate this: it is present in both `W_cold` and `warm_rtt`, so the subtraction
removes it, and only the cold-specific placement work survives.

`warm_rtt` is deliberately **not** split into the network path and Lambda's per-invoke front-end
processing (request auth, the throttle/concurrency check, routing to the sandbox). Both fall inside
the same caller-side wall-clock, and no service-side metric itemizes either, so they stay one lump.
The method never needs the split, only that the lump repeats identically on cold and warm invokes
so it cancels.
[What a caller waits for end to end](#what-a-caller-waits-for-end-to-end)
below is where that lump stops being subtracted and becomes a cost in its own right.

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
              <th class="num">mem</th>
              <th class="num">zip</th>
              <th class="num">W_cold</th>
              <th class="num">init</th>
              <th class="num">cold dur</th>
              <th class="num">warm RTT</th>
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
          followed by ${dl.n_warm_per_sample} warm invokes whose net round-trip,
          each invoke's wall-clock minus its own handler <code>Duration</code>,
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

The table spans three axes. **For a `.zip` artifact, artifact size
is the variable that most clearly moves the residual; memory tier is flat within noise, and runtime
family shows no separate fixed floor once size and the wide per-sample range are accounted for.**
(Packaging matters here: a container image splits this cost differently, covered in
[Zip vs container image](#zip-vs-container-image-where-the-size-cost-lands) below.) The zip rows read this way:

- **Memory / vCPU** (the `rust/hello` rows at 128 / 512 / 3008 MB, and the `@512` vs `@3008` pairs):
  the residual is **flat within noise**, consistent with steps 1-2 running before the configured CPU
  allocation takes effect, so provisioning appears not to slow down at low tiers the way the _handler_
  does. Unlike the init/first-request cost, this part does not depend on the memory tier.
- **Artifact size** (the `@512 MB` rows, ordered by `zip`): a **fixed floor of ~${Math.round(dlFloor)} ms**
  (the median of the `hello` cells) holds through the low-MB packages, and a **download term only becomes
  visible once the artifact is large** (the ~${Math.round(dlBigZipMB[0])}-${Math.round(dlBigZipMB[dlBigZipMB.length - 1])} MB Java/Python bundles sit ~${Math.round(dlBigLift[0])}-${Math.round(dlBigLift[dlBigLift.length - 1])} ms
  above the floor). Below a few MB, download is lost in the fixed provisioning cost. The synthetic
  probe below pushes this axis much further.
- **Runtime family / language** (the small-artifact rows, and the realistic SDK-heavy rows across
  the languages, using `smithyfull` where supported and `oneclient`/`authz` for Python and Go):
  `rust` and `go` run on the custom `provided.al2023` runtime while
  `node`/`python`/`java` are managed runtimes, yet at a given artifact size the residuals land on
  the **same floor**, so the provisioning cost looks family- and language-independent: it comes from
  the download and the environment start, not from which runtime runs.

Across all the rows, most of what a caller waits through before the handler begins is
fixed environment-provisioning cost, plus a download term that only bites for large packages, and
none of it appears in any `REPORT` line.

An external figure makes that fixed floor plausible. Lambda's execution environments run on
[Firecracker](https://firecracker-microvm.github.io/), the microVM monitor AWS built for exactly
this workload, and Firecracker's public
[specification](https://github.com/firecracker-microvm/firecracker/blob/main/SPECIFICATION.md)
enforces `<= 125 ms` from the `InstanceStart` API call to the start of the guest's `/sbin/init`,
the same magnitude as the ~${Math.round(dlFloor)} ms floor measured here. That is corroboration,
not attribution: the spec's bound holds on specific metal instances with a Firecracker-tuned
minimal kernel and rootfs, `/sbin/init` is not yet a ready Lambda runtime, the residual also
carries the download plus a cold-only service remainder (placement/scheduling and any other work
that runs only on the cold path, such as credential and environment setup; the routine per-invoke
auth/throttle processing cancels in the subtraction), and Lambda's production boot path (which may
not even pay a full microVM boot on every cold start) is not publicly specified. The magnitudes
agree, but nothing here shows what the floor is made of.

<div class="warning" label="Illustrative magnitudes, measured outside the matrix">

These come from an off-matrix probe timing a handful of already-deployed functions from a **single
client, single account**. The `warm_rtt` subtraction removes the client→region path from the residual
_median_, so the residual is vantage-independent (a caller's own network round-trip sits _on top of_
it, not inside it). It does not remove it per-sample, so the range in parentheses widens with client
distance and control-plane jitter. These are **illustrative magnitudes, not part of the benchmark
matrix**, the same way the [cross-language probes](#rust-vs-go-same-work-different-phase) below are framed.

</div>

<!-- Regenerate the table's data with:
     cargo run -p bencher -- probe download-start   (matrix must be deployed; dist/ must be built)
     which writes a run-scoped results/lifecycle-download-start-<run_id>.json (gitignored);
     this page's data loader discovers the newest one at build time. -->

## What a caller waits for end to end

Every number on this site is REPORT-side: a cold start is `init` + the first request's `duration`,
warm latency is the handler's own `duration`. A caller's wall-clock contains more, and the probe
above measured exactly the missing pieces. So the practical question, _what a caller should expect
end to end when network distance doesn't matter_, has a formula that works for **any** cell of the
matrix (every scenario, language, and memory tier on the [main results](./index)), not just the
handful of functions probed here:

- **expected cold wait** ≈ the scenario's **cold start** (init + first duration) **+ the
  residual** (a flat ~${Math.round(dlFloor)} ms for artifacts up to a few MB; size-dependent
  beyond, next sections) **+ the front-end lump**
- **expected warm wait** ≈ the scenario's **warm `duration`** **+ the front-end lump**

The warm numbers on the main results are therefore a floor too, and proportionally a larger one: for
a fast handler the lump is most of the wait. [The production caller](#the-production-caller-no-log-tail-p50-and-p99)
below gives its p50 and p99.

The **front-end lump** is the `warm RTT` of the table above, read as a cost instead of subtracted
out: Lambda's per-invoke service processing plus the network transit, the un-itemized wall-clock
the previous section only needed to cancel.
That is why no metric, from any vantage, itemizes a "latency excluding network" number. The
closest real setup to "network doesn't matter" is a caller in the function's own region: this run's
lump is ~${ms(feLump)}, the median `warm RTT` across the probed cells (the published runs measure
it in-region, on Fargate). That figure includes a few ms that only the probe pays, for asking Lambda
to return the log tail; a production caller's lump is ~${ms(feLumpProd)}
([below](#the-production-caller-no-log-tail-p50-and-p99)). How much of that is still transit cannot be measured here either, only
bounded. AWS documents intra-region networking as single-digit-millisecond round-trips even
_across_ Availability Zones (dedicated metro fiber, AZs up to ~100 km apart; the
[fault-isolation whitepaper](https://docs.aws.amazon.com/whitepapers/latest/aws-fault-isolation-boundaries/availability-zones.html)),
and [independent measurements](https://www.bitsand.cloud/posts/cross-az-latencies) put cross-AZ
round-trips at ~0.5-2 ms in most regions. A same-region HTTPS request that skips Lambda's front end
measured below that range:

> **Dated one-off characterization (taken 2026-10-16, `eu-central-1`, single account; not
> committed data).** From a Lambda function (arm64, 1769 MB, one pooled HTTP/2 connection), 1,000
> sequential GETs to a hello-world server on Fargate (arm64, 1 vCPU) behind an Application Load
> Balancer took **~0.7 ms at p50 and ~1 ms at p99**, network both ways, load balancer and server
> included. The first request on a new TCP + TLS connection took ~10 ms.

The network share of the lump is at most that, so an in-region lump is an **upper bound on the
front-end cost, and nearly all of it is service processing**. From a remote client, the
client→region round-trip goes on top of both expectations, plus a TLS handshake when the connection
is not pooled.

The residual alone would _under_-count the wait: its construction cancels the lump, which even a
zero-distance caller still pays. The two terms
answer different questions: the residual is what being cold costs beyond the REPORT metrics; the
lump is what invoking costs at all.

The probe also timed the full cold wall-clock, `W_cold`, directly (no formula, just the elapsed
time around the SDK's `Invoke` call), so the formula can be checked against reality:

```js
// The front-end lump the formula quotes: the median warm RTT across the probed
// cells, derived from the SAME probe data the decomposition table renders.
// The caller-wait fields (w_warm_p50 + the W_cold/W_warm spreads) are
// guaranteed by the data loader, which refuses to build from an output that
// predates them. Three rows span the formula's range: the canonical
// small-artifact cell at the lowest tier, the slow-init managed-runtime
// bundle, and the fattest zip in the matrix.
const feLump = median(dl.cells.map((c) => c.warm_rtt_p50));
// Warm gaps (W_warm - warm_rtt, i.e. the handler's own Duration) at the two ends
// of the probed set, so the prose's magnitudes track the data: the trivial
// rust/hello cell against the heaviest per-invoke handler among the probed cells.
// The heavy end is SELECTED from the data, name included, so the sentence cannot
// keep naming a scenario that a later probe set no longer tops out on.
const warmGap = (c) => c.w_warm_p50 - c.warm_rtt_p50;
const warmGapCell = dl.cells.reduce((a, b) =>
  warmGap(a) >= warmGap(b) ? a : b,
);
const sanity = [
  ["rust", "hello", 128],
  ["java", "smithyfull", 512],
  ["python", "oneclient", 512],
]
  .map(([lang, scenario, mem]) =>
    dl.cells.find(
      (c) => c.lang === lang && c.scenario === scenario && c.memory_mb === mem,
    ),
  )
  .filter((c) => c !== undefined);
display(
  sanity.length === 0
    ? html`<p class="caption">
        No reference cells for the formula's sanity check in this probe output
        (a scoped <code>--only</code> run).
      </p>`
    : html`<table class="dlstart">
          <thead>
            <tr>
              <th>scenario</th>
              <th class="num">mem</th>
              <th class="num">init + cold dur + residual + front-end</th>
              <th class="num">= formula</th>
              <th class="num"><strong>measured cold wait</strong> (W_cold)</th>
            </tr>
          </thead>
          <tbody>
            ${sanity.map((c) => {
              const sum =
                c.init_p50 +
                c.cold_duration_p50 +
                c.residual_p50 +
                c.warm_rtt_p50;
              return html`<tr>
                <td><code>${c.lang}/${c.scenario}</code></td>
                <td class="num">${c.memory_mb} MB</td>
                <td class="num">
                  ${c.init_p50.toFixed(0)} + ${c.cold_duration_p50.toFixed(0)} +
                  ${c.residual_p50.toFixed(0)} + ${c.warm_rtt_p50.toFixed(0)}
                </td>
                <td class="num">${ms(sum)}</td>
                <td class="num">
                  <strong>${ms(c.w_cold_p50)}</strong>
                  <span class="caption"
                    >(${c.w_cold_min.toFixed(0)}–${c.w_cold_max.toFixed(0)})</span
                  >
                </td>
              </tr>`;
            })}
          </tbody>
        </table>
        <div class="caption">
          Each formula term and the measured wait are independent medians of the
          same ${sanity[0]?.n_samples ?? 0} cold samples (a median is not
          additive), so formula and measurement agree approximately, not
          exactly; the range in parentheses is min–max across those samples.
          Off-matrix probe against deployed functions in ${dl.region}.
        </div>`,
);
```

The warm side checks the same way: on every probed cell the directly measured warm wall-clock
`W_warm` sits above that cell's front-end lump by just the handler's own `Duration`. On a trivial
handler that is ~${ms(warmGap(dlHello), 1)} (for `rust/hello` @ 128 MB: ${ms(dlHello.w_warm_p50, 1)}
<span class="caption">(${dlHello.w_warm_min.toFixed(1)}–${dlHello.w_warm_max.toFixed(1)})</span>
measured against its own ${ms(dlHello.warm_rtt_p50, 1)} `warm RTT`; the range is min–max across the
cold samples' warm batches). The probed rows whose handler does real per-invoke work sit further
above their lump, up to ~${ms(warmGap(warmGapCell))} on <code>${warmGapCell.lang}/${warmGapCell.scenario}</code>,
again matching that cell's own warm `duration`. That is the warm half of the formula for any
handler: its warm `duration` plus the lump.

### The production caller: no log tail, p50 and p99

Every probe invoke so far asks for `LogType=Tail`, because the log tail's `REPORT` line is the only
per-invoke `Duration` a caller can get, and the decomposition needs it. Asking for the tail costs
time: the response waits until the invocation has ended and its `REPORT` exists. The subtraction
above cancels that cost; a caller in production does not ask for the tail. The
${dl.n_warm_per_sample} warm invokes per sample also give a stable median but no usable p99. So after
each cell's cold samples the probe sends **${dl.n_caller_warm} more warm invokes with
`LogType=None`**, timed the same way, as a caller in production would send them:

```js
// Production-caller view: the no-tail warm wait (caller_warm_*), guaranteed
// present by the data loader (stale-schema outputs refuse to build). No REPORT
// comes back for these invokes, so the lump and the tail's price are derived on
// the trivial hello cells only, where the handler's Duration (w_warm - warm_rtt)
// is about a millisecond: there, the tailed and no-tail warm walls differ only by
// the tail, and the no-tail wall minus Duration is the production lump.
const helloCells = dl.cells.filter((c) => c.scenario === "hello");
const tailCost = (c) => c.w_warm_p50 - c.caller_warm_p50;
const lumpProd = (c) => c.caller_warm_p50 - (c.w_warm_p50 - c.warm_rtt_p50);
const feLumpProd = median(helloCells.map(lumpProd));
const tailCostMed = median(helloCells.map(tailCost));
// The front end's own tail on the trivial cell (handler ~nothing): p99 over p50.
const helloP99Gap = dlHello.caller_warm_p99 - dlHello.caller_warm_p50;
// The tail's price across memory tiers on the same trivial function.
const rustHello = helloCells
  .filter((c) => c.lang === "rust")
  .sort((a, b) => a.memory_mb - b.memory_mb);
const tailLow = rustHello[0];
const tailHigh = rustHello[rustHello.length - 1];
display(
  html`<table class="dlstart">
      <thead>
        <tr>
          <th>scenario</th>
          <th class="num">mem</th>
          <th class="num"><strong>caller warm p50</strong></th>
          <th class="num">p90</th>
          <th class="num"><strong>p99</strong></th>
        </tr>
      </thead>
      <tbody>
        ${dl.cells.map(
          (c) =>
            html`<tr>
              <td><code>${c.lang}/${c.scenario}</code></td>
              <td class="num">${c.memory_mb} MB</td>
              <td class="num"><strong>${ms(c.caller_warm_p50, 1)}</strong></td>
              <td class="num">${ms(c.caller_warm_p90, 1)}</td>
              <td class="num">
                <strong>${ms(c.caller_warm_p99, 1)}</strong>
                <span class="caption"
                  >(${c.caller_warm_min.toFixed(1)}–${c.caller_warm_max.toFixed(1)})</span
                >
              </td>
            </tr>`,
        )}
      </tbody>
    </table>
    <div class="caption">
      ${dl.n_caller_warm} back-to-back warm invokes per row with
      <code>LogType=None</code>, nearest-rank percentiles, min–max in
      parentheses. These return no <code>REPORT</code>, so they cannot
      individually be confirmed warm: a sandbox re-placed mid-block lands here
      as a slow outlier, as it would for any caller sending requests one at a
      time. Off-matrix probe against deployed functions in ${dl.region}.
    </div>`,
);
```

The trivial `hello` rows separate the pieces, because their handler's `Duration` is about a
millisecond. On them, asking for the tail costs ~${ms(tailCostMed, 1)} (median), so the production
front-end lump is ~${ms(feLumpProd, 1)} rather than ~${ms(feLump)}. Read the warm half of the
formula with that lump. The tail's price is not constant: on `rust/hello` it is
~${ms(tailCost(tailLow), 1)} at ${tailLow.memory_mb} MB and ~${ms(tailCost(tailHigh), 1)} at
${tailHigh.memory_mb} MB, consistent with the end of the invocation running on the tier's CPU
share.

On `rust/hello` @ 128 MB, where the handler adds almost nothing, the caller's p99 sits ~${ms(helloP99Gap, 1)} above its p50. That spread
comes almost entirely from the front end (the same-region request above that bypasses it has a p99
of ~1 ms), so the main results' warm p99
(handler only) understates a caller's p99 by more than the median lump.

<div class="warning" label="A floor from a same-region caller, not a latency promise">

Both terms and the measured waits are illustrative single-client, single-account magnitudes from
the same off-matrix probe as the decomposition above: same region, pooled HTTPS connection. The
cold ranges are wide because cold placement varies invoke to invoke. Nothing here
subtracts the network; it is merely small from this vantage.

</div>

## Download cost at larger sizes

The decomposition table above tops out at the matrix's fattest real artifact (~${dlMaxZipMB} MB), right where the download
term starts to show. To see how it grows beyond that, a companion probe deploys **synthetic** functions
padded to 1 / 10 / 50 / 100 / 200 MB (a minimal base plus incompressible filler), measures the
same residual, and tears them down. It runs **two runtime families** at each size, Python on the
managed `python3.14` runtime and Rust on the custom `provided.al2023` runtime (reusing the real
`hello` bootstrap), because download and environment start are platform-level work that should not
depend on the runtime: the two series landing on the same curve is the check. Padding with inert
bytes is something the benchmark matrix deliberately never does, because for a _loaded-code_ cold
start (init + first request) filler would measure download rather than code being brought up.
_Here_ that is what makes it the right instrument: the residual subtraction removes init and the
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
// Per-run magnitudes quoted in the prose below are DERIVED from the published
// dscale JSON, never hand-typed, so the "this run measured …" values always match
// the chart the pipeline just refreshed. Shape claims (floor, slope range, ~Nx)
// stay as prose: those hold across runs, where the magnitudes are per-run.
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
floor the decomposition table above shows, measured here by an independent probe, download is lost in it), then,
once the package grows past that, a **near-linear climb** of very roughly **3-8 ms per MB**, reaching
of order **a second and up at 200 MB** (the exact slope and endpoint move run to run; this run measured
Python ~${residSecAt("python", dscaleMax)} s, Rust ~${residSecAt("rust", dscaleMax)} s at ${dscaleMax} MB). Both runtime families show the **same shape**:
managed Python and custom-runtime Rust rise the same way, which points to platform-level byte
transfer rather than a per-runtime code path.
The two lines are not identical run to run, but at every size their min–max bands overlap and the gap
between their medians stays smaller than the spread _within_ either family, so with only
${dscale.samples?.[0]?.n_samples ?? 0} cold samples per point that gap is within noise, and which
family reads slightly higher cannot be attributed to the runtime. So for a large `.zip` package the download term dominates cold start
outright, and **no `REPORT` field or CloudWatch function metric shows that latency**, which is a
concrete reason to keep deployment packages lean. (Keeping packages lean helps a
container image too, but for a different reason: there the cost lands in the _reported_
`Init Duration`, not this invisible residual, as the next section shows.)

On the **solid line in the chart above**, across the whole 1-to-200 MB sweep the
reported **`Init Duration` stays flat** (Rust ~${initMsAt("rust", dscaleMin)} ms, Python ~${initMsAt("python", dscaleMin)} ms, no upward trend, hover any
point to read it) while the dashed residual climbs by roughly an order of magnitude. If code
download were folded into init, that
solid line would climb with the dashed one and a 200 MB package's init would be seconds; it does not,
so the growth lands entirely outside the metric.

Same "illustrative, environment-dependent, single-account" caveat as the table above; the _shape_
(flat floor, then ~linear climb, family-independent) is the reproducible result, the exact ms are
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
figures below are from that paper). The chart below re-runs the same download-scaling probe with a
container-image family (padded images on the managed Python base, deployed two ways from one image:
one that never reads the padding, one that reads it all at init). As before,
**solid line = reported `Init Duration`, dashed line = the unreported pre-init residual.**

```js
const dimg = await FileAttachment(
  "data/lifecycle-download-scaling-image.json",
).json();
```

```js
display(C.zipVsImageDownloadScaling(dimg, invalidation));
```

<div class="caption">Reported <code>Init Duration</code> (solid) and unreported pre-init residual (dashed) vs added artifact size, log-x, at ${dimg.memory_mb} MB / ${dimg.arch}, p50 of ${dimg.samples?.[0]?.n_samples ?? 0} cold samples per point. The zip series here is the managed-Python <code>.zip</code> family measured in the <em>same run</em> as the image series (so all lines share one session, client vantage, and date, and are directly comparable). The publish pipeline writes this chart's data and the chart above in one invocation, so the shared zip family matches. Image series are one container image per size (base ≈${(dimg.base_image_bytes_est / 1e6).toFixed(0)} MB + padding), deployed with the added padding read at init or left untouched. Off-matrix probe in ${dimg.region}.</div>

```js
// Magnitudes quoted in the prose + table below are DERIVED from the published
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
// The crossover bracket: the largest measured size where the zip's summed
// overhead still wins and the smallest where the image does. Read off the same
// p50s the table below prints, so the bracket can never contradict its own rows.
const totals = imgSizes
  .map(totalRow)
  .filter((r) => r.zip != null && r.img != null);
const crossLo = totals.filter((r) => r.zip < r.img).at(-1)?.mb ?? null;
const crossHi =
  totals.find((r) => r.img < r.zip && (crossLo == null || r.mb > crossLo))
    ?.mb ?? null;
// Empty when the flip is not bracketed by two measured sizes (the sentence then
// keeps only its run-to-run caveat rather than printing a null bracket).
const crossoverNote =
  crossLo != null && crossHi != null
    ? `in this run it falls between the ${crossLo} and ${crossHi} MB rows; `
    : "";
```

The four lines show the inversion:

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
- **The invisible part remains, but it stops growing.** Even the image pays a flat
  ~${residFloorLo}-${residFloorHi} ms unreported pre-init residual (microVM start + the minimal boot blocks). The difference
  from `.zip` is that this floor stays _flat_ as size grows, so the hidden cost is bounded.
- **Total cold overhead (init + residual), the two lines summed** (each point's value is available on hover): the
  `.zip` starts lower (it has no base-layer floor) but the image is lower as size grows, with the
  crossover several tens of MB up (${crossoverNote}the exact point moves run to run). The table
  below is built from the same published p50s the chart
  draws (the smallest- and largest-size rows are bold, the two ends of the crossover):

```js
// Total-overhead table, DERIVED from dimg (never hand-typed): summed init+residual
// p50 per size for the zip baseline and the touched image. Bolds the crossover
// extremes (smallest size, where zip wins; largest, where the image wins).
display(
  html`<table class="dlstart">
    <thead>
      <tr>
        <th class="num">Added size</th>
        <th class="num"><code>.zip</code> total</th>
        <th class="num">image total</th>
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
those blocks fault in _while the Init Duration clock is running_ and the image's download of that code
is counted **inside** `Init Duration`. That is why touched-init climbs with size
while the residual does not. (This is the mechanism behind the `.zip`-vs-image note under the table
near the top of the page: for an image, part of the code "download" is reported, in step 3.) A `.zip`, by
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
against the `.zip`'s climb. (The cache tier that served the probe's chunks was not measured, so
this data cannot give the split for these runs.)

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

The chart uses _inert padding_ to push size far past any real artifact; well below the crossover
above there is little to win, and an image carries a fixed base-layer floor
(≈${(dimg.base_image_bytes_est / 1e6).toFixed(0)} MB here) a lean `.zip` does not.

> **Dated one-off characterization (taken 2026-07, arm64/512 MB, single account; not committed
> data).** For a real Rust `smithyfull` binary (~10.6 MB uncompressed, ~4.9 MB zip; not
> padding), zip and image cold-started within noise of each other (~250-270 ms total). The image
> advantage is a _large-artifact_ effect, not a universal win.

### Memory tier moves neither phase's size cost

Raising the memory tier does not rescue the `.zip` either: its size cost is the _unreported_
download residual, which the decomposition table above shows is flat across tiers, consistent with steps 1-2
running before the configured CPU allocation takes effect, so a bigger tier does not shrink it. The image's size cost is the opposite phase, the
_reported_ init growth, but that runs in the Init phase, which (next section) runs on a boosted,
roughly full-vCPU allocation regardless of the configured memory tier, so it too is tier-independent.
The published data above is a single tier (${dimg.memory_mb} MB), so it does not itself establish
that; a separate check did.

> **Dated one-off characterization (taken 2026-07, arm64, single account; not committed data).** A
> memory sweep of the 200 MB touched image across 128 MB → 3 GB found its `Init Duration` nearly
> flat (~400 ms at every tier; a separate run from the published 200 MB chart point above, which
> reflects its own run's magnitude), and a `.zip`'s init flat too once the artifact is not
> pathologically large. Consistent with the Init-phase CPU boost the next section documents: memory
> tier does not move either phase's size cost; only packaging does.

<div class="caption">Same off-matrix caveat as the zip probes above (single client, single account, illustrative magnitudes, not matrix data). Both series are measured in the same in-region run, so the two are directly comparable. The <em>shapes and direction</em> are the reproducible result (zip residual climbs, image init climbs when loaded, image residual flat), not the exact ms.</div>

<!-- Refreshed by the publish pipeline (deploy/run-benchmark.sh's download-scaling step) alongside the zip families:
     cargo run -p bencher -- probe download-scaling --with-image
     which additionally assembles+pushes one padded container image per size with crane (base pull +
     one-layer append + CMD + push to ECR, no container-build daemon), deploys untouched+touched
     functions from each, measures, tears them down, and writes
     results/lifecycle-download-scaling-image-<run_id>.json (gitignored; newest discovered at build). -->

## Init runs on boosted CPU

The remaining sections stay inside steps 3-4, the part the `REPORT` line _does_ report. Here the
decisive effect is _which phase_ work runs in. Two facts from AWS's docs frame it:

- **CPU scales with memory.** Lambda allocates CPU **proportionally to the configured memory**: a
  full vCPU at 1,769 MB, a fraction of a vCPU below that
  ([memory/CPU docs](https://docs.aws.amazon.com/lambda/latest/dg/configuration-memory.html)). At
  128 MB that fraction is roughly 0.07 vCPU, so CPU-bound work runs on the order of 14x slower than
  on a full core.
- **Init and Invoke are distinct phases** (above).

What the docs do **not** state is that the two phases run on _different_ amounts of CPU.
Measurements on this site (next section) and elsewhere are consistent with the Init phase running on a
**boosted CPU, roughly a full vCPU regardless of the configured tier**, dropping to the tier's
fractional allocation only when the handler starts. The clearest statement of it comes from
AWS itself: in [re:Invent 2019, "Best practices for AWS Lambda and Java" (SVS403-R1)](https://www.youtube.com/watch?v=ddg1u5HLwg8&t=718s),
a slide labels the runtime-start and handler-class-init steps **"Boosted host CPU access (up to
10 seconds)"** and the handler-method step **"Throttled CPU access,"** with the presenter
noting they do this "because we want the functions to start faster."

The tier's fractional allocation is the **baseline the function is configured for and billed
at**, not a penalty. The Init phase is the anomaly, running _above_ that baseline; the handler
runs _at_ it.

<div class="warning" label="Observed behavior, not a contract">

The boost is not in the official
[execution-environment docs](https://docs.aws.amazon.com/lambda/latest/dg/runtimes-context.html);
the primary source is a 2019 conference talk, and the "up to 10 seconds" it cites lines up
with the documented 10-second Init limit. A 2026-10 probe measured that limit directly: an init
that ran longer dropped to the configured tier's speed after ~10 s (see
[only the first ~10 seconds of an init are boosted](#only-the-first-10-seconds-of-an-init-are-boosted)). It has held consistently in measurement, but AWS
does not commit to it, so **a system should not depend on it**. What follows is a
way to read the measured numbers, not advice to build around an undocumented mechanism.

</div>

## Setup is cheaper at init than in the handler

Because of the boost, the same one-time setup (resolving credentials and configuration, a first
TLS handshake, CPU-heavy static initialization) costs less in the Init phase than in the handler,
where below ~1.8 GB it gets only the tier's fraction of a vCPU. The effect is largest at the smallest
tiers, where the gap between the boosted and baseline allocations is widest, and shrinks toward
zero as the configured memory approaches a full vCPU (at which point init and invoke run on
roughly the same CPU and placement stops mattering).

This site already shows the effect _within a single runtime_: the [Rust jitter-entropy
panel](./rust#the-aws-lc-rs-jitter-entropy-cold-start-tax) measures the identical one-time
`aws-lc-rs` seeding cost landing in two different phases. When the first TLS handshake happens
at init (`lettercount`), the cost is a **flat bump** in `init_ms` that stays roughly constant
across tiers, paid cheaply on the boosted CPU. When it happens in the handler (`oneclient`), the
same cost becomes a **cliff** in the invoke duration that grows steeply as memory shrinks, consistent with the phase CPU difference.

## Rust vs Go: same work, different phase

The effect also shows up _across_ languages, where it is subtler than
"one runtime is faster." Take **Rust and Go**, two of the runtimes measured here. Their AWS SDKs
can do the **same** expensive one-time setup, building an HTTPS client: load the operating
system's root-certificate store, assemble the TLS stack, open the first connection, and still
post very different cold starts, mostly because they run that setup in **different lifecycle
phases**. The boost covers one and not the other.

Constructing the SDK client objects is not the difference (that costs well under 1 ms in both
Rust and Go). The expensive part is standing up the HTTPS machinery (building the connector and
loading the OS certificate store), and the two SDKs schedule it in opposite phases. Two dated
one-off sets of isolating probes (single account, 128 MB arm64, timed internally, not committed
data) make this concrete: the Rust `load()` breakdown was taken 2026-10 with `aws-config` 1.8.18,
and the Go-vs-Rust first-call comparison further down was taken 2026-07. The ms below are rough
magnitudes, not reproducible from the published dataset.

**Rust resolves it eagerly, at init.** The probes show `aws_config::load().await` building the default HTTPS client
while wiring up the credential providers, so the cost lands in the boosted Init phase:

| Rust `aws_config::load()` at init                            | median  |
| ------------------------------------------------------------ | ------- |
| first call, default (builds the HTTPS client)                | ~55 ms  |
| first call, with a no-op HTTP client supplied                | ~20 ms  |
| first call, with static credentials (no provider chain)      | ~17 ms  |
| second call, with a no-op HTTP client supplied               | ~0.2 ms |
| second call, with static credentials                         | ~0.1 ms |
| default, after one untimed `load()` with static credentials  | ~38 ms  |
| `load_native_certs()` alone, after the same untimed `load()` | ~30 ms  |

The first `load()` in a process costs ~17 ms even when it builds no HTTP client, and a second
call with static credentials takes ~0.1 ms, so that ~17 ms is a one-time cost of running the SDK's
setup code for the first time (the probes do not break it down further). With that paid, building
the default HTTPS client and credential provider chain takes ~38 ms (the chain is ~3 ms of it,
going by the no-op vs static first calls), and ~30 ms is loading and parsing the OS root
certificates; the rest is the TLS/connector assembly. That was `rustls-native-certs` 0.8.4;
0.8.5, in the benchmark's lockfile since 2026-10, reads each certificate file only once and
took ~11 ms off it
([load root certificates from the CA bundle](./tuning#load-root-certificates-from-the-ca-bundle)). Constructing the `aws-lc-rs` crypto provider
is negligible (~0.001 ms in the 2026-07 probes); that is separate from the one-time RNG **seeding**
the [jitter-entropy panel](./rust#the-aws-lc-rs-jitter-entropy-cold-start-tax) measures, which fires
later on the first TLS handshake and is switched off in these probes via
`AWS_LC_SYS_NO_JITTER_ENTROPY=1` (the site default). Run the default `load()` in the handler
instead, on the tier's ordinary CPU rather than the init boost, and it rises to ~900 ms, a ~16x
jump.

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
handshake, which neither SDK performs before the first request. Most of Go's share is the root
certificates: on `provided.al2023` Go also scans both certificate directories, and skipping that
scan took ~775 ms off a 128 MB first request in a later probe
([skip the certificate directory scan](./tuning#skip-the-certificate-directory-scan)).

So the boost does not make Rust cheaper _at the work_; it lets Rust do the same expensive setup in
the one window where 128 MB gets a full vCPU, while Go does near-identical work an order of
magnitude slower at the tier's ordinary allocation. It is a **lifecycle-placement effect**: the
total one-time work is comparable between the two, but Rust front-loads it into the boosted phase and Go defers
it into the metered one. This is also why forcing Rust's setup _into_ the handler erased most of
its cold advantage in a companion test: Rust's cold rose sharply while Go's barely moved.

Two caveats apply. First, even on fully equal footing (all setup forced into the
handler, so both run at the tier's ordinary CPU), Rust still finished ahead: in that companion
test its cold total was ~1150 ms against Go's ~1540 ms, about 1.3x, versus a several-fold
gap in the idiomatic init-placement case. So Rust has a _separate_, smaller per-request efficiency
edge on top of the placement effect, but placement is the larger term. (Those figures are from
the same single-account probes, not the site's benchmark data; treat them as rough magnitudes.)
Second, the eager-init pattern is a real production advantage for Rust, not merely a
benchmarking artifact: an idiomatic Rust
handler does get its setup subsidized on every cold start below ~1.8 GB. The point is only
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

The trigger is narrower than "the invocation failed": what decides it is whether the
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
  (a `StackOverflowError`: the JVM itself can catch one, but Lambda's Java runtime did not survive it
  in testing, the same way it does not survive an `OutOfMemoryError`), Python, Rust, and Go a genuine
  stack overflow aborts the process, so it
  re-inits. Python needs the qualifier "genuine": ordinary deep recursion raises a catchable
  `RecursionError` and stays warm; only an overflow that defeats the interpreter's recursion guard
  (C-level recursion, or a recursion limit raised past the real stack) aborts the process.

<div class="warning" label="A re-init has its own time limit, and a long init can loop">

A first cold start's Init phase gets its own budget, separate from the function timeout (10 seconds
for a standard function; for provisioned-concurrency and SnapStart functions that 10-second limit
does not apply at all, and init may run up to 15 minutes, the limit being 130 seconds or the
configured function timeout, whichever is _higher_). **A suppressed init gets a different limit:**
it runs inside the invoke and was cut off at **max(timeout, min(timeout + 10 s, 20 s))** after the
invoke started. On a standard function that is at least as long as a cold start allows (13 s
against 10 s at a 3-second timeout); with provisioned concurrency or SnapStart it is far shorter
than their first init. Either way an init that does not fit leaves the environment failing every
invoke, a re-init loop that lasted from minutes to over half an hour. That held on-demand, with
provisioned concurrency and with SnapStart, after a timeout, a process exit or an OOM, and for
slow extensions as for a slow runtime. The handler then gets only what is left of the window, at
most the timeout. Make the timeout at least init plus the slowest invoke, keep init short, and do
not count on the provisioned-concurrency or SnapStart init budget: it covers only the first init. The
re-init is not slower than an on-demand cold start's: it runs on boosted CPU too, for its first ~10
seconds ([only the first ~10 seconds of an init are boosted](#only-the-first-10-seconds-of-an-init-are-boosted)).
Measurements: [how much time a re-init gets](#how-much-time-a-re-init-gets).

Catching downstream errors _inside_ the handler only helps for the recoverable-error class
above; it cannot save a failure that ends the process, such as an actual OOM or a hard timeout, both
of which re-init on every runtime regardless.

</div>

A warm sandbox pays init once and serves many invocations, so on the mean the suppressed init is
diluted. But it does not disappear; it moves into the **latency tail (P90/P99)**: the invocation that
follows a crash or timeout pays init **again**, and that init hides inside a `Duration` that looks
like a merely-slow invoke. Unlike a normal cold start, no `REPORT` field marks it as init: it emits no
`Init Duration` (or `Restore Duration`) line, so it is not reported as a cold start at all. (The one signal that does mark it: an extension consuming the
[Telemetry API](https://docs.aws.amazon.com/lambda/latest/dg/telemetry-api.html) receives
`INIT_START`/`INIT_REPORT` events with `phase=invoke` for a suppressed init, so it is observable, but
only with that instrumentation in place.) For SnapStart the same
crash costs more, as covered on the [SnapStart page](./java-snapstart#after-a-crash-or-timeout-snap-start-runs-a-full-init): the recovery is a full JVM init, **not** a snapshot restore, so the crash silently drops the function off the SnapStart fast path.

### How much time a re-init gets

These come from off-matrix probes (2026-10, `eu-central-1`, arm64) whose init slept a set time
that could be longer after a crash than on the first cold start, so the first cold start succeeded
and the re-init could be any length. Unless a row says otherwise, the function was Node (`nodejs24.x`) at 1,024 MB
and the crash was a timeout. The re-init was cut off at a fixed time after the invoke started:

| Timeout | Re-init cutoff | Re-init that succeeded | Re-init that failed, on every invoke after                                |
| ------- | -------------- | ---------------------- | ------------------------------------------------------------------------- |
| 3 s     | 13 s           | 8 s and 12 s           | 14 s: `INIT_REPORT Init Duration: 13001 ms Phase: invoke Status: timeout` |
| 10 s    | 20 s           |                        | 25 s with provisioned concurrency, Node and Rust: `20000 ms`              |
| 15 s    | 20 s           | 12 s                   | 22 s: `20000 ms`                                                          |
| 20 s    | 20 s           |                        | 25 s and 32 s: `20000 ms`                                                 |
| 30 s    | 30 s           | 25 s                   | 32 s: `30001 ms`                                                          |

That is max(timeout, min(timeout + 10 s, 20 s)): up to a 10-second timeout the re-init gets ten
extra seconds, between 10 and 20 seconds it gets whatever is left up to 20 (5 extra at a 15-second
timeout), and from 20 seconds up it gets none. The same cutoffs came out with Rust
(`provided.al2023`) at 3 and 10 s; after a process exit or an OOM instead of a timeout (13 s at a
3-second timeout); for an external extension that was slow to start, with a fast runtime (13 s at
3 s); and for SnapStart (Java 21), whose recovery after a crash is a full init, not a restore (13 s
at a 3-second timeout, 30 s at 30 s). SnapStart's first init, at publish time, may run up to 15
minutes, so a SnapStart function can publish and serve with an init far beyond this window and
only fail at its first crash; with provisioned concurrency the same holds for its 130-second init.

A [2024 post](https://aaronstuyvenberg.com/posts/lambda-timeout-doom-loop) that calls this loop a
"doom loop", on an ordinary on-demand function, had a ~3-second init fail against a 3-second timeout. The probe here
gave the re-init an extra 10 seconds at that timeout, so the cutoff may have changed since then.

### What the handler gets after a re-init

The handler does not get a fresh timeout. In the same probes, with the handler waiting after the
re-init, it got the smaller of the timeout and what was left until the cutoff above, and
`context.getRemainingTimeInMillis()` at the handler's start reported that real time:

| Timeout | Re-init | Remaining at the handler's start | Handler that succeeded | Handler that timed out |
| ------- | ------- | -------------------------------- | ---------------------- | ---------------------- |
| 3 s     | 8 s     | 2,999 ms (a full timeout)        | 2 s                    | 4 s and 6 s            |
| 3 s     | 12 s    | 913 ms (to the 13 s cutoff)      | 0.3 s                  | 1.5 s and 2 s          |
| 15 s    | 12 s    | 7,930 ms (to the 20 s cutoff)    | 7 s                    | 9 s                    |
| 30 s    | 25 s    | 4,886 ms (what was left of 30 s) | 4 s                    | 8 s                    |

`Duration` and `Billed Duration` in the `REPORT` line never exceeded the timeout, even when re-init
and handler together ran longer: a 12-second re-init plus a 7-second handler at a 15-second timeout
reported `Duration: 15000.00 ms`, and an 8-second re-init plus a 2-second handler at 3 seconds
reported `3000.00 ms`. Whether the bill counts the rest was not checked.

### An init over 10 seconds runs twice

On a standard function, a cold start's init that passes 10 seconds is stopped
(`INIT_REPORT … Phase: init Status: timeout`) and run again from the start inside the first
invoke, where only the function timeout bounds it. A 12-second init with a 30-second timeout made
the caller wait ~23 s, the `REPORT` showing only the second run (`Duration: 12094 ms`), and the same
held for a 12-second extension. With a 10-second timeout the same init failed, the retry being
limited to the timeout alone, and the next invoke ran it again within the 20-second window above and
succeeded. The handler gets what the retry leaves of the timeout: after a 39-second retry with a
300-second timeout, `getRemainingTimeInMillis()` reported 260,833 ms.

### Only the first ~10 seconds of an init are boosted

A probe at 128 MB ran a fixed CPU loop of ~12 s at full speed in its init, longer than 10 s, and
recorded its speed every second. Both the retried cold start and a post-crash re-init ran at
~720-740 million iterations per second for ~10 s and then at ~44 million, the same as the loop in
the handler (1/16), until they finished:

```
retried cold start: 716,726,709,741,741,742,742,742,742,694,44,44,47,48,44,44,…  (39.1 s in total)
post-crash re-init: 721,725,706,733,738,737,737,737,738,614,44,44,44,43,46,49,…  (41.7 s in total)
```

That matches the "up to 10 seconds" of the [boost](#init-runs-on-boosted-cpu), and it applies to
each init separately: the retry got its own 10 seconds after the first attempt had used 10. A
shorter re-init stays boosted throughout, even though it runs on the invoke path and bills as
`Duration`: in an earlier probe the same CPU workload in a post-crash init finished within a few
percent of a normal cold init, ~13-16x faster than the throttled handler at 128 MB. At small
memory sizes an init that runs past 10 seconds therefore slows down sharply, here from ~12 s of
work to ~39 s.

A provisioned-concurrency init got no boost at all. The same version at 128 MB ran a ~0.84-second
init in 0.84 s on demand, but in 14.7 s when Lambda provisioned it, at ~44 million iterations per
second from its third second on. With the ~12-second workload, provisioning failed: each init was
stopped at the 130-second limit (`Init Duration: 129999 ms`) and the configuration went to `FAILED`
with `FUNCTION_ERROR_INIT_FAILURE`, for an init that finishes on demand in ~40 s. On-demand
environments of a version with provisioned concurrency (spillover) still ran their init boosted. So
a provisioned-concurrency init takes about as long as the same work in the handler. Whether its
re-init after a crash is boosted was not measured.

### How long a re-init loop lasts

Runs with a 3-second timeout and a 14-second re-init, so the re-init could not fit its 13-second
cutoff, each started by one invoke that timed out. Callers invoked again as soon as their last
invoke returned, except in the single-caller run:

| Traffic                                                           | Result                                                                                                                                                                                                                  |
| ----------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 8 callers, 4 runs (one of them 12 minutes long, three 20 minutes) | One environment looped: ~1% of requests failed, each holding its caller ~13.7 s. In one run it stopped after ~9 minutes; in the other three it was still looping when the run ended, 19 minutes after the first timeout |
| 2 callers, 20 minutes                                             | ~8% failed (83 of 1,092), still looping at the end                                                                                                                                                                      |
| 1 caller, an invoke every ~44 s for 30 minutes, then 12 in a row  | Every invoke failed: 53 over ~35 minutes                                                                                                                                                                                |
| The same function after 15 minutes without traffic                | The next invoke started a new environment and succeeded                                                                                                                                                                 |

The stuck environment's log stream showed its timed-out re-inits throughout, so the loop ended
only when that environment was removed, never by a re-init that started to fit. While traffic kept
it alive that usually did not happen within 20 minutes; the one early end is not explained. The
fewer environments a function runs, the larger the share of requests that hits the stuck one, and
with a single environment it is all of them. After 15 minutes without traffic (one test) the environment
had been reclaimed and the loop was over.

## Reading the cold numbers

- **What the site's "cold start" includes.** It is `init` + first-request `duration` (steps 3-4),
  the part the `REPORT` line reports. It does **not** include the download + environment-start
  (steps 1-2) measured above; a caller waits through those too, but no function metric exposes them,
  so they are documented here separately rather than folded into the headline number.
  [What a caller waits for end to end](#what-a-caller-waits-for-end-to-end)
  turns the headline number into an end-to-end expectation with two measured terms.
- **What a low-tier cold gap reflects.** Every function on this site builds its clients at init in
  each language's idiomatic style, so the published cold starts already embed the placement effect.
  That is representative of well-written handlers, but it means a cold gap at 128 or 256 MB
  partly reflects _where_ each runtime places its setup, not only how fast it runs. The gap is
  expected to narrow at higher tiers.
- **The host CPU varies, and the function does not choose it.** In one-off probes (2026-10,
  `eu-central-1` and `us-east-1`), Lambda placed the same arm64 function on Graviton2, Graviton3
  and Graviton4 hosts, chosen per execution environment. A Rust `smithyfull` cold start at 128 MB
  took ~354 ms on Graviton3 and ~292 ms on Graviton4 in `eu-central-1`, and ~402 ms on Graviton2
  and ~376 ms on Graviton3 in `us-east-1`. The 2026-10-14 benchmark run drew all three in `eu-central-1`, about
  1,000 cold starts each on Graviton2 and Graviton4. Comparing each function with itself, a cold
  start took about 45% longer on Graviton2 than on Graviton4, and about 25% longer on Graviton3. The mix differed by region and between probes on the same day. On x86_64, Firecracker
  (Lambda's VM layer) reports every host as a generic "Intel(R) Xeon(R) Processor" with only the
  host's nominal clock, which hides the generation. Most cold starts showed "@ 2.50GHz". A few
  hosts reported no clock, and those ran a CPU-bound Python loop ~24% faster than the "@ 2.50GHz"
  hosts, which took about twice as long on that loop as Graviton4.
- **On x86_64, Lambda also hides newer instructions.** In a second probe (a Python function at
  1,769 MB, two runs of 30 cold starts per architecture), every x86_64 host showed the same feature
  list: AVX2, but no AVX-512, ADX, SHA extensions or vector AES, and the model number of a Haswell
  Xeon. That matches Firecracker's
  [T2 template](https://github.com/firecracker-microvm/firecracker/blob/main/docs/cpu_templates/cpu-templates.md),
  which copies the features of EC2's old T2 instances (the T2S and T2CL templates hide the same
  ones). The cache sizes show different chips underneath: the "@ 2.50GHz" hosts report a 35.75 MB
  cache, which fits a Cascade Lake Xeon, and the no-clock hosts a 105 MB cache, which fits a
  Sapphire Rapids Xeon. Both chips have ADX, which crypto libraries use for the math in a TLS
  handshake, and AVX-512. Sapphire Rapids also has the SHA extensions and AVX-512 IFMA, which some
  RSA code uses. Because Lambda hides these features, code that picks its fastest path at run time
  (OpenSSL, AWS-LC, ring, Go's crypto, the JVM's JIT) cannot use them. On arm64 each generation
  showed its own extensions (SVE and SHA-512 from Graviton3 on, SVE2 on Graviton4), but pointer
  authentication and SSBS, which EC2 Graviton instances list, were hidden on all of them. Python's
  SHA-256 hashed 40 MB in ~25 ms on every Graviton, which has SHA-256 instructions, and in ~105 ms
  on the common x86_64 hosts. Cascade Lake has no SHA instructions, so that gap comes from the chip
  and would remain without the masking.
- **What the host mix does to the numbers.** AWS does not document the host mix or the masking;
  its [architecture page](https://docs.aws.amazon.com/lambda/latest/dg/foundation-arch.html) still
  describes arm64 as Graviton2. Each cold percentile here mixes the hosts its run drew. The P50
  mostly reflects the most common host, some of the P90 spread comes from slower hosts, and a rerun
  on a different mix can move both. The [appendix](./appendix#host-cpus) lists the mix behind the
  numbers on this site, for runs that recorded it. The `smithyfull` probe is described on
  [cold-start tuning](./tuning), under "How these numbers were measured".
- **For a handler where cold start matters below ~1.8 GB.** Setup work runs faster at init
  than on the first request (for a lazy SDK, that can require a small real call, not just
  constructing a client), and a higher memory tier runs the phases on comparable CPU. Neither
  substitutes for measuring the specific workload, and neither is a guarantee given the
  mechanism is undocumented.
