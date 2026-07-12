---
title: "Rust on AWS Lambda: jitter-entropy & opt-level"
toc: false
---

# Rust on AWS Lambda: jitter-entropy & opt-level

Two Rust-specific build settings (opt-level and the aws-lc-rs jitter-entropy flag), measured per memory tier.

```js
const stats = await FileAttachment("data/stats.json").json();
import { filterForm } from "./components/filters.js";
import { colorModel } from "./lib/series.js";
import * as C from "./components/charts.js";
const cm = colorModel(
  stats.dimensions.languages,
  stats.dimensions.architectures,
);
```

```js
// One filter, shared by both sections below. Rust-only page, so the language
// toggle would be meaningless, so the architectures + memories pickers drive both
// the jitter chart and the opt-level dumbbells. The scenarios picker drives
// the opt-level section directly; the jitter chart only ever shows
// `oneclient`/`lettercount` (the two scenarios with the A/B), so unchecking
// either of those in the picker hides that panel and unchecking both hides
// the jitter section entirely.
const sel = view(
  filterForm(stats, {
    colorModel: cm,
    groups: ["architectures", "scenarios", "memories"],
  }),
);
```

```js
const v = C.makeView(stats, sel, invalidation);
```

## The aws-lc-rs jitter-entropy cold-start tax

`aws-lc-rs` (the default crypto backend for `rustls` and the AWS SDK for Rust) collects CPU jitter entropy **once per process**, on the first TLS handshake. That is the same one-time CPU work whichever scenario triggers it, but because the Lambda Init phase appears to run on boosted CPU while the Invoke phase runs on the configured tier's fraction, the wall-clock cost depends on which phase the first TLS call lands in. On the two scenarios below that produces two different shapes:

- **`oneclient` (1 AWS client, DDB), first TLS in the Invoke phase (the cliff).** The SDK client is built in the Init phase, but the first TLS handshake the SDK makes is the per-invoke DDB `GetItem` call, in the **Invoke phase** under the configured tier's fractional vCPU. So the jitter tax lands in the cold invoke's reported `Duration` and the on-vs-off gap grows steeply as memory shrinks.
- **`lettercount` (letter count, CPU), first TLS in the Init phase (the flat bump).** The handler does an `s3.get_object().send()` in the **Init phase**, where the measured init bump is consistent with Init-phase code running on more CPU than the configured tier alone would provide. The same tax lands in `Init Duration` and stays roughly flat across tiers.

The chart shows the _measured consequence_ (cliff vs flat bump). Which phase each `.send()` runs in is fixed by the handler shape (the two bullets above), and the measured phase deltas are consistent with the first TLS work landing in the Invoke phase for `oneclient` and the Init phase for `lettercount`; the Init-phase CPU boost behind the two shapes is inferred, an observed behavior AWS does not formally document. The benchmark records the `REPORT` line, not TLS timing or CPU allocation. [Cold Start Anatomy](./lifecycle) covers the mechanism, the source, the caveats, and why the same effect shapes cold starts _across_ runtimes, not just this Rust A/B.

```js
if (!stats.dimensions.hasJitter) {
  display(
    html`<p class="caption">
      This dataset has no jitter A/B. Re-run the benchmark to populate this
      section.
    </p>`,
  );
}
```

```js
// The jitter section only makes sense if (a) the run produced the A/B and (b)
// at least one of the two A/B scenarios is checked in the filter. Otherwise
// the whole block (heading + caption + chart + explainer) is hidden as a unit
// so the page doesn't show an orphan heading over an empty chart.
const jitterScenarios = ["oneclient", "lettercount"].filter((s) =>
  v.scenarios.includes(s),
);
const showJitter = stats.dimensions.hasJitter && jitterScenarios.length > 0;
```

```js
display(
  showJitter
    ? html`<h3>
          Cold-start latency, jitter on vs off (init + first request, P50)
        </h3>
        <div class="chart-sub">
          Two bars per memory tier: <strong>jitter off</strong> (built with
          <code>AWS_LC_SYS_NO_JITTER_ENTROPY=1</code>) and
          <strong>jitter on</strong> (built without it), each stacked into
          <em>init</em> (dark) and <em>first request</em> (light); the bold
          <code>+ms</code> at each on-bar end is the gap between the two
          stacked-segment totals at that tier.
          <strong>Setting the flag is a latency/security trade-off:</strong> it
          drops one of AWS-LC's defense-in-depth seed sources (see details
          below).
          <em
            >Steady-state warm latency shows no persistent jitter tax; low-tier
            variation, especially on lettercount, appears to be CPU-throttling
            or scheduling noise rather than jitter, and the tax is one-time per
            cold sandbox.</em
          >
          The A/B is built on <code>opt-level=3</code> only, so this panel
          always uses o3.
        </div>`
    : null,
);
display(showJitter ? C.jitterCliff(v) : null);
```

<details class="chart-details">
<summary><strong>Why setting <code>AWS_LC_SYS_NO_JITTER_ENTROPY=1</code> is a trade-off, not a default</strong></summary>

AWS-LC seeds its RNG by hedging across two entropy sources as a defense-in-depth measure, with CPU jitter as the default seed root (see `entropy_sources.c` in [aws-lc](https://github.com/aws/aws-lc/blob/44766fa7daa88e5afc7fc6de3311c48eeeb02f39/crypto/fipsmodule/rand/entropy/entropy_sources.c), the C library that the `aws-lc-sys` crate vendors). With `AWS_LC_SYS_NO_JITTER_ENTROPY=1` set, AWS-LC seeds from the OS instead and uses the CPU's hardware RNG (`RDRAND` / `RNDR`) as the second source; on a CPU without a hardware RNG it falls back to the OS source for the second slot too, so the two sources are no longer independent. AWS-LC's own build option warns that with jitter disabled "randomness generation might not use two independent entropy sources," so it should be evaluated per workload. AWS's own Rust SDK team documents this flag as the cold-start mitigation for Lambda and frames it as a trade-off, not a blanket recommendation to disable ([smithy-rs announcement](https://github.com/smithy-lang/smithy-rs/discussions/4541)). Every other Rust chart on this site is built with the flag set, so its latency numbers reflect the optimized configuration; this page isolates what setting the flag actually changes on Lambda.

**AWS-LC auto-opts-out inside a snapshot-restore environment.** A runtime check detects a VM uniqueness-breaking event (UBE), the resume of a snapshotted/cloned VM, and switches to the same OS + `RDRAND` configuration on its own ([entropy_sources.c](https://github.com/aws/aws-lc/blob/44766fa7daa88e5afc7fc6de3311c48eeeb02f39/crypto/fipsmodule/rand/entropy/entropy_sources.c)). Plain Lambda does not signal a VM UBE, so this does not fire for a plain Rust function; setting the build flag is the manual equivalent.

**Two ways to avoid the Invoke-phase cliff** for a handler shaped like `oneclient`:

- **Force a TLS handshake at init.** Constructing the SDK client is not enough, since `aws-lc-rs` is lazy; a small real call on the client the handler uses (for the DynamoDB-backed `oneclient`, a cheap call such as `list_tables`, though this variant is illustrative and not one of the measured builds) is required. This is cheap under the Init-phase CPU behavior noted above; if that behavior changes, the cost moves to the first request.
- **Build with `AWS_LC_SYS_NO_JITTER_ENTROPY=1`.** Removes the cost outright (in either phase) but drops one of AWS-LC's defense-in-depth entropy sources, so it is a workload-specific decision.

</details>

## Opt-level: speed vs size

`opt-level=3` optimizes for runtime speed, usually at the cost of a bigger binary; `opt-level=z` optimizes for size, which can load faster. Neither outcome is guaranteed: rustc's optimizer is not fully predictable, so `z` is not always the smaller binary and `3` is not always the faster code ([cargo profiles reference](https://doc.rust-lang.org/cargo/reference/profiles.html#opt-level)). Which wins for cold start is scenario-dependent, so both are measured.

One scope caveat: a smaller artifact also downloads and unpacks faster, but that phase runs _before_ the Init phase and no `REPORT` metric isolates it (it is only visible from the caller's wall-clock, which [Cold Start Anatomy](./lifecycle#the-hidden-steps-download-environment-start) measures separately, and where the download term is small below a few MB). So this A/B captures the **loaded-code** trade-off (init link/load + warm execution speed), not total cold start including download. The chart below plots cold **init** P50 for exactly that reason.

```js
if (!stats.dimensions.hasOpt) {
  display(
    html`<p class="caption">
      This dataset has no opt-level dimension (it was run with a single Rust
      opt-level).
    </p>`,
  );
}
```

```js
display(
  stats.dimensions.hasOpt
    ? html`<h3>Cold init P50</h3>
        <div class="chart-sub">
          Each row is scenario × arch × memory, with the two binary sizes (o3/oz
          MB) inline; the dots are the cold-init P50. For cold start, oz's
          usually-smaller binary tends to give a lower cold init, and the gap is
          wider on most of the larger-binary scenarios (a few invert);
          occasionally o3 wins instead. The gap is shown per row. Left dot =
          faster. Both sides use the jitter-off build, so the o3 and oz dots are
          a clean opt-level comparison.
        </div>`
    : null,
);
display(stats.dimensions.hasOpt ? C.optDumbbell(v, { metric: "cold" }) : null);
```

```js
display(
  stats.dimensions.hasOpt
    ? html`<h3>Warm P50</h3>
        <div class="chart-sub">
          Same rows, warm latency. Here oz's slower machine code costs: small
          for most I/O-bound rows (with a few low-tier and framework exceptions)
          but large for the CPU-bound scenarios (<code>lettercount</code>, and
          especially <code>batch</code>), where o3 can be markedly faster. Left
          dot = faster.
        </div>`
    : null,
);
display(stats.dimensions.hasOpt ? C.optDumbbell(v, { metric: "warm" }) : null);
```
