# LambdaBench

> Published at [lambdabench.dev](https://lambdabench.dev). An independent personal project, not
> affiliated with or endorsed by AWS or any vendor; the numbers are best-effort measurements,
> provided as-is with no guarantee of accuracy
> ([full disclaimer](https://lambdabench.dev/appendix#disclaimer)).

Benchmarks **whole Lambda scenarios** (not just empty cold starts) across language, architecture,
and memory, for **Rust**, **Node 24**, **Java 25**, **Python 3.14**, and **Go**, measuring both
**cold** and **warm** invocation latency. Java additionally carries a **SnapStart** dimension, so the
steep plain-JVM cold start and its SnapStart-restored counterpart are both measured. Python and Go
host every scenario *except* the two Smithy ones (`smithy`, `smithyfull`): neither has a fair way to
run a Smithy server (see [Matrix](#matrix) for why).

Every matrix function is deployed as a **.zip file archive**; container-image packaging is not a
matrix dimension, but its cold-start behavior is measured separately by the download-scaling probe
(see the [zip-vs-image finding](#finding-zip-hides-its-size-cost-a-container-image-reports-it)). The
headline cold metric everywhere is **init (or restore) + the first request's duration**, and that is
a *floor*, not the caller's full wait: code download and environment setup finish before any `REPORT`
clock starts and appear in no function metric, so they are measured separately by the
[`probe` subcommand](#probe-the-pre-init-download--start-cost-documentation-only) rather than folded
into the headline number.

## Quick start

LambdaBench is the benchmark behind **[lambdabench.dev](https://lambdabench.dev)**, where the results are
published. This repo holds both halves that produce them: the **`bencher` CLI** that runs the benchmark and
the **interactive site** that renders a run. Clone it to reproduce the numbers, explore a run locally, or
contribute a scenario.

**Run the benchmark.** Drive the pipeline with the `bencher` CLI, the same phases the publish run executes:

```sh
cargo run -p bencher -- doctor          # verify toolchain (cargo-lambda, node/npm, JDK 25, python3+pip, go, crane) + AWS identity
cargo run -p bencher -- build           # build unique artifacts -> dist/ (no AWS needed; compile check)
cargo run -p bencher -- run             # build + deploy + benchmark -> results/run-<id>.{jsonl.gz,meta.json}
cargo run -p bencher -- teardown        # delete functions + role + table (asks first)

cd site && npm install && npm run dev   # explore the results in the browser
```

> [!WARNING]
> **Use an AWS account dedicated to this project** (a personal sandbox account, not one shared with
> other work). Every resource name `run`/`teardown` touch is a fixed literal, not namespaced per
> clone or per run (e.g. the DynamoDB table is always `lambdabench-table`, the KMS key always sits
> behind `alias/lambdabench-key`). `teardown` deletes by these exact names, plus one broader sweep: it
> also scans every KMS key in the account/region and schedules deletion (7-day recoverable window) for
> any carrying lambdabench's tag. In an account shared with another lambdabench checkout, or that
> happens to have an unrelated resource sharing a name/tag, `run`/`teardown` cannot tell it apart from
> its own. See [Region / account](#region--account).

`run` owns the full pipeline (build → deploy → benchmark), so there is no separate deploy step. To
iterate without repeating work, scope `run` and skip the phases that have not changed:
`run --skip-build` reuses the artifacts already in `dist/`, and `run --skip-deploy` invokes the
functions already deployed (both fail loud if what they expect is missing).

[Usage](#usage) below has the flags and scoped-run recipes; the rest of this file explains what the
scenarios measure and how cold starts are forced. Two companion docs go deeper: [DESIGN.md](DESIGN.md)
has the design rules and invariants (read before adding or editing a scenario), and
[RUNBOOK.md](RUNBOOK.md) covers running a full sweep at scale (pool sizing, the control-plane quota, and
the reliability safeguards).

## Usage

`doctor`/`build`/`run`/`teardown` are the pipeline (see [Quick start](#quick-start)); `run`'s deploy phase
provisions the IAM role + DDB table + KMS key + S3 bucket alongside the function matrix (see
[Matrix](#matrix) for the exact count), and `doctor` prints the region the driver
will use (pinned in `config.rs::REGION`). The flags below tune and scope a `run`: `--only`/`--memory`/`--lang`/`--arch`
restrict the whole pipeline (build + deploy + run) to the cells they select:

```sh
# Recommended full run. Iteration counts come from the `full` profile (the default; see
# config.rs::Profile): the five light/I/O scenarios run 50 cold cycles x 50 warm per cycle
# (FULL_LIGHT_COUNTS), while the four CPU probes (`lettercount`, `authz`, `batch`, `cache`) run their
# own long-warm counts and dominate run time. --pool defaults to 32 and is rate-safe by design, so no
# flags need be passed (see RUNBOOK.md). The publish pipeline (deploy/run-benchmark.sh) runs
# this same `full` profile; the exact per-cell counts that ran are recorded in the meta's
# iteration_buckets.
cargo run -p bencher -- run

# Run just the CPU probes, scoped here to Rust and Node (extend the list with more
# lang:scenario pairs to cover other languages, or invert the selection for a quick
# pass over everything else):
cargo run -p bencher -- run --only rust:lettercount,node:lettercount,rust:authz,node:authz,rust:batch,node:batch,rust:cache,node:cache

# Quick smoke run: the `smoke` profile runs a tiny flat count over whatever cells you select (here,
# a few light scenarios) just to exercise the build+deploy+invoke+parse+record pipeline end to end:
cargo run -p bencher -- run --profile smoke --only rust:hello,node:hello,java:hello

# Scope the whole pipeline to one language with --lang, e.g. just Java (plain + SnapStart variants;
# the SnapStart cold-cycle count is capped automatically). --profile smoke keeps it fast:
cargo run -p bencher -- run --profile smoke --lang java --memory 512,1024 --arch arm64

# Visualize: interactive site (decoupled; auto-picks the newest run file):
cd site && npm install && npm run dev      # live dev server
cd site && npm run build                   # static site -> site/out-site/ (host anywhere)
```

### `probe`: the pre-Init download + start cost (documentation only)

`probe` is a separate, documentation-grade measurement, **not** part of the benchmark matrix. A
cold start's first two sub-phases, downloading the code and starting the execution environment, run *before*
`Init Duration`'s clock and so appear in no `REPORT` line or CloudWatch function metric; the only
vantage point on them is the caller's wall-clock, isolated by subtraction
(`residual = W_cold − init − cold_duration − warm_rtt`). Alongside that decomposition it records
the full caller wall-clocks themselves (`w_cold`/`w_warm`, p50 + min–max), which the site publishes
as the end-to-end wait a same-region caller sees, cold and warm (network vs Lambda's per-invoke
front-end is one un-itemized lump, so a "minus network" number is deliberately not claimed). It
has **two explicit subcommands**, one per [Cold Start Anatomy](site/src/lifecycle.md) chart; there
is no default mode:

```sh
# Deploy the matrix first (bencher run, or bencher run --skip-build), then:
cargo run -p bencher -- probe download-start     # residual vs already-deployed matrix functions (deploys nothing)
cargo run -p bencher -- probe download-scaling   # deploys ephemeral padded functions (1..200 MB) to push past the
                                                 # matrix's ~17 MB real-artifact ceiling; --with-image adds a
                                                 # zip-vs-container-image sweep (via crane)
```

Each writes a run-scoped `results/lifecycle-*-<run_id>.json` (gitignored, like the matrix run) that
the site's data loader discovers at build time. The numbers are illustrative, single-client/account
magnitudes, so they are framed as such and kept off the matrix. Nothing is committed: a fresh clone
has no probe data until a run produces it, and the site build fails loud rather than publish stale
numbers. The publish pipeline (`deploy/run-benchmark.sh`) runs both subcommands in-region after the
matrix run, so a manual run is only needed for local iteration. Flags: `--cold-samples` /
`--warm-per-sample` (sample counts), `--only` / `--memory` / `--arch` (target selection), `--out`
(output path). `download-scaling` is the **one place padding is used on purpose** — the [Matrix](#matrix)
section carries the rule for why that is legitimate here and nowhere else — and the full probe design
(the residual method, the two runtime families, the teardown backstop) is in
[DESIGN.md](DESIGN.md#the-probe-subcommand-is-outside-these-invariants).

## Scenarios

Each scenario does the *same task* in each language, so the comparison is fair.

> See **[DESIGN.md](DESIGN.md)** for the design rules and invariants behind
> the scenarios: fairness, measurement purity (e.g. **disable SDK retries on
> every client**), and a checklist for adding one. Read it before adding or
> editing a scenario; most invariants are not enforced by the compiler.

The nine scenarios fall into two groups, read on two different axes.

**Group 1, handler shapes, read on cold start.** `hello` is a bare baseline; the other four are
realistic handler shapes built on it (a server framework, AWS clients, or both). All are light or
I/O-bound, so **cold start** is their interesting axis. They are *not* a linear ladder: `smithy`
(framework, no client) and `oneclient` (client, no framework) are siblings off `hello`, and `smithyfull`
combines both branches. So compare each to its **related** scenario (`smithy` vs `hello` for the
framework's cost; `threeclient` vs `oneclient` for each added client), never by subtracting across the
row: each initializes its own way and the costs do not add up.

| # | Name          | What it does                                                                                                                                                                                                        |
|---|---------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 1 | `hello`       | Returns a constant. Pure runtime + handler-dispatch baseline.                                                                                                                                                       |
| 2 | `smithy`      | Hosts a handler behind the **Smithy server SDK**, routing one request, no AWS call. Adds the server framework to `hello`.                                                                                           |
| 3 | `oneclient`   | Constructs + calls **one** AWS client (DynamoDB `GetItem`).                                                                                                                                                         |
| 4 | `threeclient` | Constructs + calls **three** AWS clients (DynamoDB + KMS Encrypt + S3 `GetObject`).                                                                                                                                 |
| 5 | `smithyfull`  | The realistic shape: Smithy server SDK hosting a `CreateOrder` write flow (KMS-encrypt a signature, DDB `PutItem`, S3 `PutObject` a receipt), with full request/response (de)serialization + constraint validation. |

**Group 2, CPU probes, read on warm latency.** These four ask one question (*how much does language
choice matter for CPU-bound work?*), and the answer depends entirely on **where the CPU time goes**: your
own code, a shared native library, a standard-library parser, or the garbage collector. Each isolates
one of those. The deep mechanics (the JSON-parser spread, the retained-heap GC tail, and the "read the
tail in absolute ms, not the P99/P50 ratio" caveat) live in the [Findings](#findings) below.

| # | Name          | What it isolates                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
|---|---------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 6 | `lettercount` | **CPU in your own code.** Fetches a ~1 MB JSON array of ASCII strings from S3 once at init, then each warm invoke parses it and counts `a`..`z` across all entries: a tight byte/code-unit loop run in each runtime's *own* execution (machine code vs JIT vs interpreter), not a shared native lib. Widest warm spread; steep low-memory cliff for the slow runtimes.                                                                                                                      |
| 7 | `authz`       | **CPU split between a native lib and your glue.** A real JWT authorizer: verify an RS256 signature (native crypto, configured symmetrically across languages) then extract + type-map claims (in-language). RS256 verify is cheap, so the in-language glue still dominates. The warm gap narrows and flattens vs `lettercount` but does not vanish. The token arrives in the invoke payload.                                                                                                |
| 8 | `batch`       | **JSON-parser speed (median), with a transient-GC tail.** Parses a ~16 MB JSON record batch (loaded once at init) and groups-by key each invoke. The median is each language's standard parser; the parsed graph is freed each invoke, so the GC tail is secondary. Floored at 256 MB (512 on Java, 1024 on Java SnapStart). See the `encoding/json` finding.                                                                                                                               |
| 9 | `cache`       | **Retained-heap GC, the dedicated GC probe.** Holds a ~100 MB live set across warm invokes and churns a slice each invoke, so a tracing GC must re-trace a large *retained* heap every cycle (the path `batch`'s transient graph never reaches). The warm tail separates from the median on GC'd runtimes at low memory; a secondary median axis reflects heap representation. Indexed ring of buffers, not a hashmap, to isolate the GC. Floored at 512 MB. See the retained-heap finding. |

Read every scenario by **direct comparison across languages**, never by subtracting one scenario from
another. The design rules that keep the probes fair (the long-warm run shape, init-vs-payload input,
ASCII-only counting, symmetric native crypto, and the SHA-trap that motivated them) live in
[DESIGN.md](DESIGN.md).

## Matrix

- **Languages:** Rust (`provided.al2023`), Node 24 (`nodejs24.x`), Java 25 (`java25`), Python 3.14 (`python3.14`), Go (`provided.al2023`, native binary)
- **Memory (MB):** 128, 256, 512, 1024, 2048, 3008
- **Architectures:** arm64, x86_64
- **Optimization (Rust only):** `opt-level=3` (speed) vs `opt-level=z` (size). Which cold-starts faster is scenario-dependent, so both are measured. One caveat: the smaller artifact's faster-download advantage lands in the download-and-unpack phase, which no REPORT metric isolates (see the timings note under [How it works](#how-it-works)). So this A/B captures the *loaded-code* trade-off (init link/load + warm execution speed), not total cold start including download. Node, Java, Python, and Go have no equivalent knob.

  This A/B, plus the handler-shape scenarios (which grow the artifact with *real* linked/loaded code: framework, SDK clients), is how the matrix studies artifact size: as actually-loaded code, never as inert bytes. **This is why the matrix never pads an artifact with dead filler to "isolate size", and it is the canonical statement of that rule** (referenced from the probe section above and DESIGN.md). A *loaded-code* cold start (init + first request) is about code being brought up, linked, loaded, and initialized. Filler is downloaded and unpacked but never brought up, so padding a matrix artifact would measure the wrong phase. Inert filler is right in exactly one place, the [Cold Start Anatomy download-scaling probe](https://lambdabench.dev/lifecycle): that probe subtracts init and the first request away, leaving only the download + environment-start term, which is exactly what filler grows. Same technique, opposite validity, decided by which phase you are measuring.
- **`aws-lc-rs` jitter-entropy A/B (Rust only, scoped):** every Rust binary in the matrix is built with `AWS_LC_SYS_NO_JITTER_ENTROPY=1`, which drops the AWS-LC CPU-jitter entropy source to recover its cold-start latency cost (a latency/security trade-off, not a universal win). A second variant with the seeding *enabled* is built only for `oneclient` and `lettercount` × o3 × both arches × all memory tiers, because those two scenarios place the same one-time cost in *opposing* Lambda lifecycle phases (the cliff in the Invoke phase vs the flat bump in the Init phase); other scenarios would just repeat one of them. The full mechanism, the trade-off, and the SnapStart parallel are in the [jitter-entropy Finding](#finding-the-aws-lc-jitter-entropy-cold-start-tax-same-cost-two-outcomes) below.
- **SnapStart (Java):** plain JVM vs SnapStart, treated as two **separate runtimes**. The dashboard shows `Java` and `Java SnapStart` as distinct series alongside the other runtimes, not as a Java sub-variant, because SnapStart is a fundamentally different execution model, not a tuning knob: it restores a pre-initialized snapshot instead of running the JVM init on every cold start, replacing `Init Duration` with a `Restore Duration` (usually much smaller on a heavy handler, but not always on a light one). The SnapStart variant is **primed**: a CRaC `beforeCheckpoint` hook runs one representative invocation during init, so the AWS SDK's lazy class loading / marshaller construction / JIT is baked into the snapshot. That is the realistic config an operator who enables SnapStart would ship (see the priming finding under Findings).

  SnapStart sweeps the **same** memory range as every other series (a full peer, not a reduced sub-sweep), except where a per-variant floor drops a tier the runtime cannot survive. SnapStart raises the `batch` and `smithyfull` floors above plain Java's; `lettercount`'s ≥512 MB floor applies to both plain and SnapStart Java, so it narrows the range identically for each (see the function-count bullet below). Only its per-cell cold-cycle count is capped at 10 (`config.rs::SNAPSTART_COLD_CYCLES`), because each cold sample publishes a fresh function version and the snapshot is created at publish time (~10-30 s). The cap only ever *reduces* a count: the light scenarios drop from 50 to 10 and `batch` from 15 to 10, but the CPU probes (`lettercount`, `authz`, `cache`) whose base cold count is already 5 stay at 5.
- **Python / Go scenarios:** Python and Go each host every scenario **except** the two Smithy ones (`smithy`, `smithyfull`). [smithy-python](https://github.com/smithy-lang/smithy-python) is client/types codegen only, with no **server** SDK. [smithy-go](https://github.com/aws/smithy-go) *does* ship a `go-server-codegen` plugin, but it is explicitly a work-in-progress, is not published to Maven Central (you must build it to mavenLocal yourself), generates **only** the `awsJson1_0` protocol (the shared `CoffeeShop` model is `restJson1`, so a Go server would do different (de)serialization work than the Rust/Node/Java servers and break the same-task fairness rule), and provides no Lambda adapter. So neither has a fair way to host the Smithy server; those cells are never generated. Like Node, Python and Go have no opt-level or SnapStart dimension (one plain runtime per cell); Go, like Rust, compiles to a per-arch native binary.
- **= 674 Lambda functions**: Rust 228 (9 scenarios × both opt-levels × tiers × 2 arch, **plus 24 jitter=On diagnostic cells** on `oneclient`/`lettercount` × o3 × tiers × 2 arch), Node 102, Java two runtimes (plain 96 and SnapStart 92), Python 78, and Go 78. The counts are lower than a flat scenario×tier×arch product because the memory floors are language- and variant-aware (`config.rs::Scenario::min_memory_mb`), and unrunnable cells below a floor are dropped, not recorded as failures:
  - `batch` needs ≥512 MB on plain Java and ≥1024 MB on SnapStart (the JVM, and the restored snapshot on top of it, OOM the smaller tiers), and ≥256 MB on Rust/Node/Python/Go.
  - `cache` needs ≥512 MB on **every** language (its ~100 MB retained live set OOMs/CPU-starves the lower tiers), so it contributes only its 4 upper tiers.
  - `lettercount` on Java needs ≥512 MB on **both** plain and SnapStart (its GC-pressure working set OOMs at 256 MB and the CPU-starved probe times out at 128 MB).
  - `smithyfull` on **SnapStart** needs ≥256 MB (at 128 MB the snapshot restore competes with the first invoke's CPU and times out).

Per function: a number of **cold cycles**, each followed by **N warm** invokes on the same sandbox.
The counts come from the run's **profile** (`--profile full|smoke`, default `full`): under `full` each
scenario has its own count (light scenarios at 50×50, the CPU probes at long warm sequences to build GC
pressure, thinned on the slowest runtime's starved low-memory tiers), while `smoke` runs a tiny flat
count over the whole matrix for a quick pipeline check. `config.rs::Cell::iterations` is the single
source of truth, and the exact per-cell counts that ran are recorded in the meta's `iteration_buckets`.

All Rust scenarios are compiled with identical optimizations (fat LTO, `codegen-units=1`, stripped,
`panic=abort`, and the jitter-entropy flag described above), varying only the `opt-level` and
jitter-entropy dimensions.

## Findings

The benchmark exists to answer questions, not just produce charts. Each finding below is a place where
the intuitive reading of the data is wrong, or where *how* a runtime does something matters more than
*what* it does. The **full write-up, mechanism, and charts for each live on
[lambdabench.dev](https://lambdabench.dev)** (linked per finding); this section keeps only the headline
shape and the data that exists nowhere else.

**These are durable *shapes* (orderings, ratios, mechanisms), not absolute latencies.** No run ships in
this repo: the result files are git-ignored and the dashboard is generated at build time from whichever
local run you point it at (newest in `results/`, or `LAMBDABENCH_RESULTS`). So the P50/P99/P99.9-in-ms
numbers live only in that run output and the site built from it, never in this prose. Two
characterizations below are **dated one-offs** (a repo-only microbenchmark and an unprimed-SnapStart
measurement) that the live matrix does not produce and cannot track; they are kept here because they
appear nowhere else.

### Finding: SnapStart priming is decisive for SDK-heavy Java handlers

SnapStart snapshots the JVM *after* init but cannot capture work that only happens on the first
*invocation* (the AWS SDK v2's lazy class loading, JIT, TLS, credential resolution) unless you force it
to run *before* the checkpoint. The benchmark's SnapStart variant is therefore **primed** wherever an
operator realistically can, the config a competent operator would ship. The durable shape on SDK-heavy
`threeclient`: `primed < plain < unprimed`, so **unprimed SnapStart is slower than no SnapStart at all**.
But priming is not what decides whether SnapStart *wins*: **the size of the handler is.** Restore is
itself work, so SnapStart only wins where the init + first-call cost it skips exceeds that restore cost:
it wins on the SDK-heavy and heavy-init handlers (`oneclient`, `threeclient`, `batch`, and even *unprimed*
`lettercount`, the clean proof it is init size, not priming) and loses at every tier on the thin ones
(`hello`, `authz`, `cache`), where fully-primed `authz` still trails plain Java.

Full mechanism, the primed/not-primed split, and live magnitudes on
**[lambdabench.dev/java-snapstart](https://lambdabench.dev/java-snapstart)**. The primed/not-primed split
is specified by DESIGN.md Measurement-purity invariant #10 (keep the two in sync). One repo-only measurement not on the site:

> **Dated one-off characterization (taken 2026-06, arm64/512 MB `threeclient`).** The unprimed variant is
> **not** in the live matrix (it would double the slowest cells to measure a config no one should ship),
> so these numbers do not appear on the dashboard and do not track later runs:
>
> | Config                   | Total cold start                    |
> |--------------------------|-------------------------------------|
> | Plain JVM (no SnapStart) | ~5.0 s                              |
> | SnapStart, **un**primed  | ~8.4 s (first request alone ~7.3 s) |
> | SnapStart, **primed**    | ~2.3 s (first request ~0.9 s)       |

### Finding: SnapStart can't be fully primed behind the smithy-java server framework

The Smithy scenarios are fronted by smithy-java's `LambdaEndpoint`, whose first-request cost (protocol
resolution, (de)serialization, constraint validation, and the JIT of all of it) is reachable *only*
through `LambdaEndpoint::handleRequest`. smithy-java exposes no supported hook to drive that path before a
checkpoint (the `SmithyServiceProvider` SPI is just `Service get()`, and the proxy-event type is
package-private), so an operator can prime the operation and (for `smithyfull`) the SDK clients but
**not the framework marshalling**. That unprimable residue is a *second* penalty on top of restore
overhead, so both Smithy scenarios trail plain Java net, unlike the SDK-only `threeclient` where priming
reaches everything and SnapStart wins. This is a property of the smithy-java
API surface used in this run (1.4.0), not of SnapStart; a future warmup hook would close it.
Faking it by reaching into package-private internals is rejected as a methodology bug (DESIGN.md
Measurement-purity invariant #10). Full write-up and magnitudes on
**[lambdabench.dev/java-snapstart](https://lambdabench.dev/java-snapstart)**.

### Finding: the AWS-LC jitter-entropy cold-start tax: same cost, two outcomes

`aws-lc-rs` (the default crypto backend for `rustls` and the AWS SDK for Rust) collects CPU jitter
entropy **once per process**, on the first TLS handshake in the sandbox's lifetime. That one-time tax
lands on whichever invoke does the first TLS call (usually the cold one); warm steady-state is always
identical between jitter=On and jitter=Off. Building with `AWS_LC_SYS_NO_JITTER_ENTROPY=1` drops it, a
**latency/security trade-off, not a free win** (it removes one of AWS-LC's defense-in-depth entropy
sources; the second source becomes the OS plus the CPU hardware RNG (`RDRAND`/`RNDR`) where present, but
on a CPU without one, such as arm64 Graviton2, it falls back to the OS for both slots, so the two are no
longer independent, see the [Rust page](https://lambdabench.dev/rust) for the full detail). AWS's own Rust SDK team documents this as
the Lambda cold-start mitigation, framed as a per-workload trade-off (see the
[smithy-rs announcement](https://github.com/smithy-lang/smithy-rs/discussions/4541)).

The interesting shape: the *same* cost lands in radically different places depending on which Lambda
lifecycle phase does the first handshake, because Init runs on boosted (near-full-vCPU) CPU while Invoke
runs on the tier's fractional allocation. `oneclient` does it in the Invoke phase (a **cliff** in
`duration_ms` that steepens as memory shrinks); `lettercount` does it in the Init phase (a **flat bump**
in `init_ms`, paid cheaply today). AWS-LC also auto-opts-out inside a snapshot restore
(`is_vm_ube_environment()`), so this never fires for SnapStart. The benchmark sets the flag matrix-wide
and keeps a scoped jitter=On A/B on `oneclient`/`lettercount` × o3 to quantify it. Full mechanism, the
Init-phase-boost caveat, and both chart panels on **[lambdabench.dev/rust](https://lambdabench.dev/rust)**;
the lifecycle-phase story is on **[lambdabench.dev/lifecycle](https://lambdabench.dev/lifecycle)**.

The build-side mechanics (where the flag is set, and why the two variants must not share a
`CARGO_TARGET_DIR`) are in DESIGN.md Fairness invariant #4.

### Finding: a crash or timeout re-runs Init on the next invocation, and on SnapStart that means a full init, not a restore

A cold start is not the only way to pay init. An invocation that **ends the runtime process** forces
the next invocation to re-run the whole Init phase, a *suppressed init* whose duration AWS folds into
that next invocation's reported `Duration`, so it reads as one slow invoke with no separate init line.
The trigger is not "the invocation failed", it is whether the process survives: tested across all five
runtimes, an **OOM**, a **timeout**, and a **process exit** re-init on every one, while an **ordinary
handler exception / returned error** stays warm on every one (the mapping diverges at the edges, e.g. a
Go `panic` re-inits while a Rust `panic` stays warm). The sharper edge is on **SnapStart**: the
recovery after such a failure runs a **full from-scratch JVM init, not a snapshot restore**, so it does
not just re-pay the fast path, it abandons it, on the runtime where init is most expensive. Full
write-up, the per-runtime mapping, and the verification on
**[lambdabench.dev/lifecycle](https://lambdabench.dev/lifecycle)** (general) and
**[lambdabench.dev/java-snapstart](https://lambdabench.dev/java-snapstart)** (the SnapStart recovery
path). This is off-matrix behavioral characterization (the live matrix forces clean cold starts and
does not crash functions), so it is stated as mechanism, not committed magnitudes.

### Finding: cold start = init + first request, because runtimes split one-time work differently

`Init Duration` measures only the Init phase. Any one-time setup a runtime *defers past* the readiness
boundary (lazy SDK clients, credential/endpoint resolution, first connection, TLS, JIT) is paid on the
first invocation instead, in that request's `Duration`. So **`Init Duration` alone is not a fair
cold-start metric**: it rewards laziness. The benchmark records both the cold marker
(`init_ms`/`restore_ms`) and the cold invocation's `duration_ms`, and the **headline cold metric
everywhere is their sum**. The clearest case is Go vs Rust on `oneclient`: Go's init is *lower*, but the
AWS SDK for Go v2 builds its client lazily while `aws-config` does it eagerly, so read by total cold start
the ordering flips and Rust comes out ahead. This is the mirror image of SnapStart priming (which
*hoists* first-request cost into snapshot time); in both cases the total is the only fair metric. Full
breakdown on **[lambdabench.dev/lifecycle](https://lambdabench.dev/lifecycle)** and the "Cold start
breakdown" chart on **[lambdabench.dev/comparison](https://lambdabench.dev/comparison)**.

### Finding: zip hides its size cost, a container image reports it

The matrix is zip-only, but the download-scaling probe (`probe download-scaling --with-image`) runs
the same residual instrument over padded container images, and the two packagings are a clean
inversion. A `.zip` is downloaded and unpacked in full before the microVM does anything, so its size
cost lands in the **unreported pre-init residual**: a flat provisioning floor up to a few MB, then a
near-linear climb reaching of order a second near Lambda's size limit, latency no `Init Duration`,
`REPORT` field, or CloudWatch function metric ever shows. A container image is chunked, cached, and
**demand-loaded lazily at block level** (the mechanism AWS documents in Brooker et al.,
[*On-demand Container Loading in AWS Lambda*](https://arxiv.org/abs/2305.13162)), so its pre-init
residual stays flat at every size and the size cost surfaces in the **reported** `Init Duration`
instead, to the extent the code is actually loaded at startup. Summed (init + residual), the zip
starts lower but the image pulls ahead as size grows, with the crossover in the low tens of MB; at
this matrix's realistic artifact sizes the two are within noise, so the image advantage is a
large-artifact effect, not a universal win. Memory tier moves neither phase's size cost. The probe's
padding is a deliberate worst case for the image (unique, all-touched bytes that defeat
deduplication and lazy loading), so a real image's transfer term should be smaller; like all probe
output these are illustrative off-matrix magnitudes, not matrix data. Full charts, mechanism, and
the paper's cache numbers on **[lambdabench.dev/lifecycle](https://lambdabench.dev/lifecycle)**.

### Finding: Go's slower `batch` warm time is `encoding/json`, not GC

Because `batch` exercises allocation + GC, it is tempting to read Go's slower warm median as GC cost. It
is not: it is the **JSON parse**, specifically Go's reflection-based `encoding/json` decoder. An isolated
microbenchmark on the same ~16 MB / 573k-record batch pins it down:

> **Dated one-off characterization (taken 2026-06, single machine).** A standalone microbenchmark, not
> part of the Lambda matrix, so it is not on the dashboard and does not track later runs. It exists to
> attribute the live `batch` median spread to its cause.
>
> |                               | parse   | group-by |
> |-------------------------------|---------|----------|
> | Rust (`serde` derive)         | ~30 ms  | ~16 ms   |
> | Go (`encoding/json` → struct) | ~207 ms | ~7 ms    |

The parse is the entire gap; Go's group-by is actually *faster* than Rust's (which clones the key into
the map), so the allocator/GC is not the culprit. So `batch`'s median is a serialization-library story
(`serde`'s monomorphization vs `encoding/json`'s runtime reflection), not a GC story. We keep
`encoding/json` because it is Go's idiomatic parser; swapping in `goccy/go-json` or `sonic` would compare
libraries, not languages (the SHA trap; see the [Fairness note](#fairness-note)). `batch` *does* surface
a modest GC tail (read it in absolute P99.9 − median, not the ratio); the full tail discussion and charts
are on **[lambdabench.dev/comparison](https://lambdabench.dev/comparison)**.

### Finding: the GC P99 tax is real, but it needs a *retained* heap, which is why `cache` exists

The popular "we rewrote our Go hot path in Rust for 5× better P99" stories pin the win on GC stalling the
tail. That effect is real, but it needs two things `batch` lacks: a **large, retained** live set (tracing
cost scales with the live set, not the garbage) and **scarce** CPU (so the concurrent collector can't
hide on a spare core). `batch`'s graph is transient and freed each invoke, so its GC tail is mild. That
is why **`cache`** exists: a ~100 MB live set held across warm invokes and churned each invoke, forcing a
tracing GC to re-trace the whole set every cycle. On the GC'd runtimes the warm tail then separates from
the P50 at the fractional-vCPU tiers and eases as vCPU grows; there is also a median axis (boxed
representations sit higher). Note this is a *single-request* GC tax: Lambda runs one request per sandbox,
so the concurrency-amplified stop-the-world of the high-RPS articles is structurally out of reach here.
The tail-vs-median charts and the read-in-absolute-ms rule are on
**[lambdabench.dev/comparison](https://lambdabench.dev/comparison)**.

## How it works

1. **Build once, deploy many.** Code is identical across the 6 memory configs (and Node JS / Java
   bytecode is identical across both arches), so each unique artifact is built once and fanned out to
   every function that shares it. Rust uses `cargo-lambda` (release, fat LTO, stripped) cross-compiling
   arm64 + x86_64; Node uses `esbuild` (minified, `target=node24`, ESM); Java uses Gradle (one
   deployment zip per scenario, classpath under `lib/`, generated Smithy server SDK for the smithy
   scenarios). SnapStart is a function-config flag on the same Java zip, so it adds no artifact.
   Python lays out `handler.py` plus its `pip install --target` dependencies (resolved for the
   `python3.14` Lambda runtime and the cell's arch, wheel-only) flat in the zip; the pure-Python
   bundles (boto3-using scenarios) are arch-independent, while `authz` ships a per-arch artifact
   because it bundles `cryptography`'s native wheel. Go cross-compiles a static `bootstrap` executable
   with `go build` (`GOOS=linux`, the cell's `GOARCH`, `CGO_ENABLED=0`, `-tags lambda.norpc`,
   `-trimpath`, and `-ldflags=-s -w` to strip symbols and DWARF) for the OS-only `provided.al2023`
   runtime, one per-arch binary, like Rust, with the AWS SDK for Go v2 linked in. The strip matches
   Rust's `strip` and Node's minification, so every artifact is measured as loadable code rather than
   carrying debug bytes only some languages would ship.
2. **Cold starts are forced**, not left to chance. For plain functions (Rust, Node, plain Java, Python, Go), each cycle
   bumps a `COLD_NONCE` environment variable via `UpdateFunctionConfiguration`, which creates a fresh
   execution environment; the driver waits for *that specific update* to land before invoking: it
   polls until the function's `RevisionId` has changed from its pre-update value **and**
   `LastUpdateStatus` is `Successful` (waiting on `Successful` alone can return on a *stale* status
   and hit the still-warm prior sandbox; [RUNBOOK.md](RUNBOOK.md)'s failure-mode table has the full
   story). **SnapStart** works differently: it applies only to
   published versions, so each cycle bumps `COLD_NONCE`, publishes a fresh function version (whose
   snapshot has never been restored), waits for that version to reach `Active`, then invokes that
   version qualifier: a guaranteed cold restore with no warm-retry loop. The version is deleted after
   the cycle to bound code storage.
3. **Timings come from the real platform.** Every invoke uses `LogType=Tail`; the `REPORT` line is
   parsed for `Init Duration` (plain cold only) or `Restore Duration` (SnapStart cold only),
   `Duration`, `Billed Duration`, `Memory Size`, and `Max Memory Used`. Either an `Init Duration` or a
   `Restore Duration` marks a cold start. Note `Init Duration` is measured *after* the package is
   downloaded and unpacked, so it captures runtime + handler init (module load, client construction), not
   the code-download time; no metric here isolates the download-and-unpack phase.
4. **Fail loud, never fall back.** The run errors out if a cold invoke has neither `Init Duration` nor
   `Restore Duration`, a warm invoke unexpectedly *has* one, `FunctionError` is set, the status code
   isn't 200, or the `REPORT` line can't be parsed.
5. **Serial within a function** (so warm invokes always hit the same warm sandbox), **parallel across
   functions** (independent functions never affect each other's sandboxes).

## Running a full sweep

Issuing ~10^6 invocations against the live control plane over hours (without throttling or recording a
single bad sample) has its own operational concerns; those live in **[RUNBOOK.md](RUNBOOK.md)**, so read
it when tuning or debugging a run. The default `--pool 32` is rate-safe by design, so a plain `run`
needs no tuning.

## Fairness note

The comparison is only fair if every language runs on an equivalent footing, so two rules matter to
anyone reading the numbers (the full rationale, and the traps they avoid, are DESIGN.md Fairness
invariant #3 for the SDK rule and Fairness invariant #2 for the native-crypto rule):

- **Every language bundles its own pinned AWS SDK.** Rust statically links its SDK, so the others match
  rather than using the runtime-provided one: Node bundles `@aws-sdk/*`, Java `software.amazon.awssdk:*`,
  Python `boto3`/`botocore`, and Go links the AWS SDK for Go v2 statically (like Rust).
- **Native crypto (`authz`) is symmetric, not avoided.** Each language verifies RS256 through the
  production crypto path its ecosystem ships: assembly-optimized big-integer RSA plus a
  hardware-accelerated SHA-256 (the per-language library pairings are enumerated in DESIGN.md
  Fairness invariant #2). This is the *opposite* choice from `lettercount` (which avoids native libs to measure
  the language itself), and it is what keeps `authz` from the SHA trap of comparing library configs
  instead of runtimes.

## Output

All data lands in `results/run-<id>.jsonl.gz`: gzipped JSONL, **one row per invocation**, with every
parsed timing the analysis needs. (The raw Lambda log tail is used live during the run: its REPORT
line is parsed, and it is dumped into error messages on a failure, but is not persisted: it is ~40% of
each row and nothing read it back.) Run metadata (matrix, counts, account, region, tool versions,
artifact sizes, timestamps) goes in `results/run-<id>.meta.json`. This means any chart can be
re-rendered later **without re-running the benchmark**. Inspect a run with `zcat results/run-<id>.jsonl.gz | head`.

The parsed `Billed Duration` and `Memory Size` are also what the site's **cost view** is priced from
(mean $ per million warm invokes: GB-seconds plus the per-request fee, at a fixed eu-central-1
on-demand reference rate, per architecture), on
**[lambdabench.dev/comparison](https://lambdabench.dev/comparison)**.

## Region / account

Pinned to **eu-central-1** (Frankfurt), in `config.rs::REGION`. The in-Lambda latencies the REPORT line
reports are region-independent, so any region gives valid data; the region is chosen to be close to the
driver, because warm invokes are issued serially per cell and the client↔region round-trip dominates
wall-clock, so running near the driver shortens a full run substantially. To move regions, change that one
constant: the S3 bucket name includes the region (so names never collide across regions), and bucket
creation sets the required `LocationConstraint` for any non-us-east-1 region automatically. The driver
prints the resolved AWS identity in `doctor` and refuses to run if credentials are missing or expired.

**Deploy this to an account of its own.** Every resource `run` creates and `teardown` deletes, the
Lambda functions, log groups, IAM role, DynamoDB table, S3 bucket, ECR repo, KMS key/alias, has a
name computed from fixed constants in `config.rs`, the same on every clone; nothing is namespaced per
user or per run. `teardown` deletes each by that exact, predetermined name (see
`bencher/src/teardown.rs`'s module doc: never a prefix sweep of a live account listing), and none of
these deletes check ownership first: `DeleteFunction`, `DeleteTable`, `DeleteBucket`, and the rest all
act on whatever resource holds that name, lambdabench's or not. In practice that's a low-probability
collision, since a matrix function name is a full descriptor (e.g.
`lambdabench-rust-hello-arm64-o3-1024`: language + scenario + arch + opt/snap/jitter + memory) unlikely
to already exist by chance. The KMS orphan sweep is the one exception worth calling out separately: it
doesn't get handed a name at all, but lists every KMS key in the account/region and schedules deletion
for any carrying the tag `lambdabench-managed-kms-key=true` (needed because that key can lose its
alias, its only other handle, to a prior failed run), so it can in principle reach a key that merely
happens to carry the same tag rather than one sharing a specific name.

None of this is namespaced per run, so one collision *is* guaranteed rather than just low-probability:
two lambdabench checkouts run by different people in the same account produce identical names (and the
same KMS tag) for everything, so either one's `teardown` deletes the other's resources in full. A
dedicated account removes both the coincidence risk and the guaranteed one.

## Layout

```
bencher/      Rust CLI driver (doctor / build / run / probe / teardown)
smithy/       Shared Smithy model (CoffeeShop service): single source of truth
scenarios/    The deployed code: rust/ node/ java/ python/ go/ × {hello,smithy,oneclient,threeclient,smithyfull,lettercount,authz,batch,cache}
deploy/       CDK app + Fargate runner that host and publish lambdabench.dev (see deploy/README.md)
dist/         Built artifacts + build-manifest.json (generated)
results/      run-<id>.jsonl.gz + meta (raw run output) and lifecycle-*-<id>.json (probe output); all git-ignored
site/         Observable Framework interactive site (filterable; hostable static build)
```
