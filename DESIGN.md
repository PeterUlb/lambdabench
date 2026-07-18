# Design guide: invariants, rationale, and how to add a scenario

This is the **contributor-facing design guide**: the non-obvious decisions behind
the benchmark, the rules that keep the comparison fair and the data trustworthy,
*why* each one matters, and a checklist for
adding a new scenario without silently breaking them. For *what each scenario is*,
see the scenario tables in [README.md](README.md); this file is the *why* behind
them and the rules an edit must not break.

If you are adding/editing a scenario: **read the Invariants
first.** Most of them are not enforced by the compiler, it is entirely possible
to write code that builds, deploys, runs, and produces *subtly wrong numbers*.

---

## The two groups the scenario set splits into

The scenarios split into two groups read on two different axes: **handler shapes**
(`hello`, `smithy`, `oneclient`, `threeclient`, `smithyfull`), read on cold start,
and **CPU probes** (`lettercount`, `authz`, `batch`, `cache`), read on warm
latency. The **[README Scenarios section](README.md#scenarios)** describes what each
one is and how to read it (handler shapes are not a linear ladder, so compare each
to its *related* shape rather than subtracting across the set; CPU probes isolate
*where the CPU time goes*). That reader-facing description is not repeated here.
This section records only the *design intent* an edit must preserve; measured
results live in the run data and the README "Finding" sections, never here.

**The `batch`/`cache` pair is deliberate, keep them separate.** Both stress
allocation, but `batch` isolates the *parser* (transient garbage, freed each invoke,
median-bound) and `cache` isolates the *collector* (a large RETAINED live set
re-traced every cycle, tail-bound). A single combined "GC scenario" cannot tell you
whether a slow tail comes from parser allocation or from collector pressure on a
retained heap, and the right fix differs in each case. `authz` is likewise the
deliberate middle ground between `lettercount` (pure in-language CPU, no native lib)
and a pure-native tie: its native RS256 slice is small enough that the in-language
claim-mapping glue still spreads the runtimes.

**Three cross-cutting reading rules the probes share** (applied throughout the
README Findings):
- **Read warm tails in absolute ms, not the P99/P50 ratio.** A runtime with a
  tiny median posts a large *ratio* from ordinary allocator/OS jitter without a
  GC problem; the ratio is meaningful only *within* a runtime.
- **A GC tail needs a large LIVE heap and scarce CPU.** Tracing cost scales with
  the live set, not the garbage, and a concurrent collector only steals from the
  handler when it has no spare core, so the tail is worst at the low
  (fractional-vCPU) tiers and eases as memory/vCPU rises.
- **Effective n for the tail is the cycle count, not the warm-sample count.**
  Warm invokes within one cycle share a sandbox by design (the GC needs that
  to reach steady state), so they are autocorrelated. The independent-replicate
  count for tail statistics is the cycle count: `5` for the long-warm CPU
  probes, `15` for `batch`. The site returns `null` for `p99` below 200 raw
  samples and for `p999` below 1000 (`site/src/lib/stats.js`), so cells
  without the resolution to support a tail percentile do not chart noise.

---

## Invariants (do not break these)

### Fairness

1. **Same task in every language.** A scenario must do byte-for-byte equivalent
   work in each runtime and return equivalent output. If the languages can't be
   made to do the *same* thing, it is not a fair scenario. (One structural
   exception: **Python and Go skip the two Smithy scenarios**, because neither
   ecosystem has a server SDK that could host the shared `restJson1` service
   doing the *same* (de)serialization work — the README
   [Matrix](README.md#matrix) section has the full per-ecosystem rationale.
   `Lang::supports` encodes this, and those Python/Go cells are never
   generated.)
   - **Each language uses its CONVENTIONAL handler, not its most-minimal one.**
     Every `hello` uses the standard, idiomatic entrypoint for its runtime: Java
     implements `RequestHandler` (pulling `aws-lambda-java-core`), Rust uses
     `lambda_runtime`, Go uses `aws-lambda-go`'s `lambda.Start`, Node/Python
     export the ordinary handler function. This is
     deliberate: `hello` is the *baseline a real handler starts from*, not a
     micro-optimized floor. So `hello` is **not** the absolute lowest cold start
     each language can achieve, e.g. Java could shave more with a bare POJO
     handler (basic/generic input + output types skip the `RequestHandler`
     interface and its dependency). We measure the conventional shape because
     that is what people actually deploy, and because the handler-shape
     comparison (`smithy` vs `hello`, `threeclient` vs `oneclient`) is only
     meaningful if every scenario uses the same handler convention. The site
     `hello` blurb is phrased accordingly ("the
     startup floor for a conventional handler doing no real work"), NOT as an
     absolute minimum.

2. **Native libraries must be symmetric and equally well-configured.** Using a
   native lib is fine (real code does), but both sides must use a production-
   grade, hardware-accelerated path, not one optimized and one fallback.
   - **The SHA trap:** a SHA-256 hashing scenario is unfair by construction. Node's
     OpenSSL uses the CPU's SHA hardware while Rust's `sha2` defaults to a
     *software* path on arm64, so the benchmark measures the library config, not
     the language. This is why there is no plain-hashing scenario.
   - **The fix, applied in `authz`:** Rust verifies RS256 via `jsonwebtoken` →
     **AWS-LC** (`aws-lc-rs`, assembly-optimized); Node via `jose` →
     WebCrypto/OpenSSL; Java via `nimbus-jose-jwt` → the JVM's built-in JCA
     (`SunRsaSign`, the production RSA path the JDK ships); Python via `PyJWT` →
     `cryptography` → OpenSSL (the production RSA path that ecosystem ships); Go
     via `golang-jwt` → the standard library's `crypto/rsa` + `crypto/sha256`
     (the runtime's assembly-optimized routines, the production RSA path Go
     ships). All are the production path each ecosystem ships. The Rust
     `jsonwebtoken` backend feature MUST be `aws_lc_rs`, **never** `rust_crypto`
     (a pure-software path that reintroduces the SHA trap). On the Python side
     the equivalent trap would be a pure-Python JWT/RSA implementation; `PyJWT`
     with `cryptography` installed delegates to OpenSSL, so that is the required
     pairing. Go's standard-library crypto is already the native, hardware-backed
     path, so there is no software-fallback trap to avoid there.

3. **Every language bundles its own AWS SDK.** Rust statically links its SDK, so
   the Node bundles include `@aws-sdk/*` (via esbuild), the Java zips bundle
   `software.amazon.awssdk:*` (AWS SDK for Java v2, with the `UrlConnectionHttpClient`,
   no Netty/Apache, for a leaner cold start), the Python zips bundle
   `boto3`/`botocore` (`pip install --target`, pinned), and the Go binary links
   the AWS SDK for Go v2 statically (like Rust) rather than using a
   runtime-provided SDK. All ship their own SDK, pinned per language (Go via its
   `go.mod`). (Python's runtime *does* ship a boto3, but using it would be
   unpinned and an unfair runtime-optimized path, so it is bundled like the rest.)

4. **All Rust scenarios compile with identical optimizations** (fat LTO,
   codegen-units=1, stripped, panic=abort, and
   `AWS_LC_SYS_NO_JITTER_ENTROPY=1`: the flag-set baseline). Most of these are cargo-lambda's own
   [release optimizations](https://www.cargo-lambda.info/guide/release-optimizations.html)
   (symbols stripped, `panic=abort`, `codegen-units=1`, plus the Neoverse-N1 /
   Haswell `target-cpu` tuning it applies via rustflags; see that page for
   what each does and why). We leave those on and deviate in one place: LTO is
   upgraded from cargo-lambda's default `lto="thin"` to **`fat`**, trading
   longer compile time for a more optimized binary, applied identically to
   every Rust cell so the o3-vs-oz and cross-language comparisons stay fair.
   The standalone crates (smithy, smithyfull, excluded from the workspace
   because they depend on gradle-generated code) must declare the SAME
   `[profile.release]` locally, or they silently fall back to that `thin`
   default and ship a less-optimized binary than the rest.
   - **`AWS_LC_SYS_NO_JITTER_ENTROPY` does not key the cargo artifact path.**
     `aws-lc-sys`'s `build.rs` does emit `cargo:rerun-if-env-changed=...` so
     cargo reruns it on a value change, but the env value is not encoded into
     the output path. So the jitter=On A/B variant (`Jitter::On`) MUST live in
     a distinct `CARGO_TARGET_DIR` from the jitter=Off build. Sharing one
     would let a rebuild for one variant clobber the other's `aws-lc-sys`
     object files and ship the wrong build. `bencher/src/build.rs::build_rust`
     keys the target dir on `(opt, jitter)` for exactly this reason; preserve
     that split if you touch the build path.

### Measurement purity

1. **Disable SDK retries on EVERY client.** This is the easiest invariant to break
   and the hardest to notice. The AWS SDK default retries throttles/5xx
   transparently (Rust `RetryConfig::standard()`, Node `maxAttempts:3`). A retried
   throttle does **not** surface as an error: it succeeds with an **inflated
   Duration** that gets recorded as if it were real latency. For the benchmark we
   want a throttle to FAIL LOUD, not corrupt the tail.
   - **Rust:** build config with
     `aws_config::defaults(...).retry_config(aws_config::retry::RetryConfig::disabled()).load().await`,
     NOT `aws_config::load_defaults(...)` (which keeps default retries).
   - **Node:** `new XxxClient({ maxAttempts: 1 })` on every client (1 = initial
     attempt, no retries).
   - **Java:** `ClientOverrideConfiguration.builder().retryStrategy(AwsRetryStrategy.doNotRetry())`
     on every AWS SDK v2 client (the canonical no-retry strategy in the current
     SDK; the older `.retryPolicy(RetryPolicy.none())` is equivalent).
   - **Python:** `Config(retries={"total_max_attempts": 1})` on every boto3
     client (`total_max_attempts` = initial attempt + retries, so 1 = no
     retries; verified against botocore's `_compute_retry_max_attempts`).
   - **Go:** `config.WithRetryer(func() aws.Retryer { return aws.NopRetryer{} })`
     on `LoadDefaultConfig` (the shared config every client is built from).
     `aws.NopRetryer` reports `MaxAttempts() == 1` and is never retryable, so a
     throttle/transient surfaces as a hard error instead of an inflated Duration.
   - This applies to init-time clients too (e.g. `lettercount`'s S3 fetch, a
     retry there inflates `init_ms`).
   - **Exception:** the *driver's* own client (`bencher/src/aws.rs`) keeps
     `RetryConfig::adaptive()`. That retries the cold-force *mechanism* (control-
     plane Updates/polls), which does not corrupt data: it just lands the cold
     start. Different layer, different rule.

2. **Big payloads load at init; small inputs come in the invoke payload.**
   - `lettercount` fetches its ~1 MB blob from S3 **once at init**: embedding it
     would let the compiler const-fold the work and add an artifact-size confound;
     sending 1 MB per invoke would put network/deserialize time in the warm
     measurement.
   - `authz` receives its ~885 B JWT **in the invoke payload**, because that is
     how a real authorizer works (token arrives with the request) and the
     platform's payload deserialization is a tiny, fair, shared cost at that size.
     Pre-loading a token pool would be unrealistically cache-friendly.
   - Rule of thumb: if the input is large or its transfer would dominate, load it
     at init; if it is small and arrives-per-request in the real world, pass it in
     the payload.

3. **Cold start is `init + first request`, never init alone.** `Init Duration`
   covers only the Init phase: everything a runtime does *before* it signals
   readiness to the Lambda Runtime API (for the custom-runtime languages, before
   `lambda_runtime::run(...)` in Rust / `lambda.Start(...)` in Go, plus package
   init; for managed runtimes, the module-load + handler-construction code). Any
   one-time setup a runtime *defers past* that boundary (lazy SDK client
   construction, endpoint/credential resolution, first connection + TLS, JIT) is
   paid on the **first invocation** and shows up in *that request's* `Duration`,
   not in `Init Duration`. So comparing init in isolation **rewards laziness**: a
   runtime that sets up eagerly looks slow on init but is done; one that defers
   looks fast on init but pays on the first request. The metric must be the
   **sum** of the cold marker and the cold invocation's own duration. The driver
   records both (`init_ms`/`restore_ms` and the cold row's `duration_ms`), and
   every cold aggregate/chart uses the sum (the site additionally *splits* it so
   the eager-vs-lazy difference is visible). Concrete shape: on `oneclient` the
   AWS SDK for Go v2 builds its client lazily, so Go's init is *lower* than Rust's
   while its first request is *far higher*. By total cold-start latency Rust comes out
   ahead, the opposite of the init-only reading. The current-run absolute numbers
   live in the README "Finding" sections and the run data, not here. This is the
   mirror image of SnapStart priming (#6), which deliberately hoists first-request
   cost *into* init/snapshot time.

4. **Fail loud, never fall back.** A row is recorded only if it is a genuine,
   invariant-holding sample (cold has `init_ms`, warm does not, status 200, no
   `FunctionError`, parseable REPORT). On any violation the cell buffers are
   discarded and the cell re-runs (up to `MAX_CELL_ATTEMPTS`); a *persistent*
   violation aborts the whole run with the decoded log tail. No safeguard ever
   records a warm sample as cold or a retry-inflated number as latency.

5. **Per-function write isolation for shared AWS resources.** Functions that
   write (smithyfull's CreateOrder) use a per-function S3 key / DDB partition key
   (`<fn>/lambdabench-receipt`, `lambdabench-order-<fn>`), not one shared key: a shared key
   concentrates all writes on one S3 object/DDB partition and triggers throttling.
   The function name LEADS the S3 key so writes spread across partition prefixes
   (S3 partitions by key prefix); the DDB PK is hashed, so its order does not matter.

6. **SnapStart is measured PRIMED, the config you would actually ship.** SnapStart
   snapshots the JVM after init, but the AWS SDK v2's first-*invoke* costs (lazy
   class loading of the marshaller/endpoint/protocol graph, JIT, TLS, credential
   resolution) are not in the snapshot unless forced to run before the checkpoint.
   Each Java handler with reachable first-invoke work registers a CRaC
   `beforeCheckpoint` hook (`org.crac`) that runs one representative invocation
   during init, so that cost lands in the snapshot. The handlers with nothing for
   priming to reach (`hello`, the CPU probes `lettercount`/`cache`, and the
   framework-only `smithy`) are left unprimed.
7. **SnapStart is its own runtime, swept like the rest.** The site treats
   `Java SnapStart` as a distinct series alongside the other runtimes, not a
   Java sub-variant, because it is a different execution model, not a tuning
   knob; it sweeps the full memory range like every other series (NOT a reduced
   sub-sweep), except where a per-variant floor drops a tier
   (`config.rs::Scenario::min_memory_mb`). The README
   [Matrix](README.md#matrix) SnapStart bullet is the reader-facing statement
   of both rules, and its function-count bullets enumerate every floor with the
   OOM/CPU-starvation reason behind each. The cross-language summary/ranking
   intersect over shared cells, so those holes simply don't contribute there.
   Only its per-cell cold-cycle count is reduced (`SNAPSTART_COLD_CYCLES`),
   since each cold sample publishes a fresh snapshot.
8. **Why primed, not unprimed:** the benchmark measures the end-user experience,
   and a competent operator who enables SnapStart on an SDK-heavy handler primes
   it (AWS's own guidance). Unprimed SnapStart backfires: on an SDK-heavy
   handler the restored JVM pays the whole SDK first-call cost at once on the
   measured invoke, landing it *slower than plain Java with no SnapStart at
   all*: the ordering is `primed < plain < unprimed`. The unprimed config is a
   documented one-off (the dated table in README "Finding: SnapStart priming"
   has the numbers), not part of the live matrix: that would double the slowest
   cells to measure a config no one should ship.
9. **No cross-contamination:** the hook fires only when a checkpoint is taken.
   On a plain (non-SnapStart) function `org.crac` uses a no-op context, so the
   *same jar* deployed to a plain function never primes. Plain Java stays a
   genuine cold start.
10. **Prime only what an operator realistically can, through the same
    entrypoint they would.** A prime is honest only if it reproduces a config
    a real operator could ship, so each primed handler calls its OWN public
    handler entrypoint once in `beforeCheckpoint`, never a framework-internal
    path the operator has no supported access to. A prime that reaches past the
    public API to flatter the numbers is as much a methodology bug as no prime
    at all.
    - PRIMED (an SDK first-call cost the operator can hoist via the public
      entrypoint): `oneclient`, `threeclient`, `smithyfull` (each warms its AWS
      SDK client graph by calling its operation/handler once), `authz` (calls its
      `RequestHandler::handleRequest` to warm the RS256-verify + claim-mapping
      path), `batch` (S3 SDK at init plus the first parse so the first invoke
      isn't the only cold-class-loading invoke).
    - **The Smithy framework path is NOT fully primable, and we do not fake it.**
      The measured `smithy`/`smithyfull` invoke enters through
      `LambdaEndpoint::handleRequest` (proxy event → protocol resolution → request
      (de)serialization → constraint validation → operation → response
      serialization). smithy-java exposes no supported way to drive that path
      before a checkpoint: the `SmithyServiceProvider` SPI an operator implements
      is just `Service get()`, and `LambdaEndpoint`'s `ProxyRequest`/`ProxyResponse`
      are package-private. So `smithyfull` primes only its SDK clients (a direct
      `createOrder` call, the dominant cost an operator CAN hoist), and the
      framework marshalling cost stays on the first restored invoke. `smithy` has
      no SDK or AWS call at all, so there is nothing an operator can hoist; it
      registers NO `beforeCheckpoint` hook and is left genuinely unprimed. Do NOT
      "fix" this by injecting a class into the
      `software.amazon.smithy.java.aws.integrations.lambda` package to construct
      the package-private event and call `handleRequest`: that drives the real
      path but measures a config no operator can ship with the public API, which
      over-credits SnapStart: the opposite bias from an absent prime, but still a
      bias. The residual framework cost is a documented finding (README "Finding:
      SnapStart can't be fully primed behind the smithy-java server framework"),
      not a gap to close. Revisit only if smithy-java ships a public warmup hook.
    - NOT PRIMED (no SDK path to hoist): `hello`, `lettercount`, `cache` (and
      `smithy`, per above). Priming pure-CPU work would warm only the user's hot loop's JIT,
      which no other runtime gets an analog of (Node/Python/Go have no SnapStart;
      a plain Java cold start is also unprimed). Keeping these unprimed makes
      SnapStart's cold sample on a CPU probe measure restore + first-touch JIT
      cost, the same shape a non-SnapStart cold start pays, fair vs the other
      runtimes.
11. **It moves cost, does not hide it dishonestly:** priming shifts SDK warm-up
    into snapshot-creation (publish) time, which is real and grows, but that is
    exactly what a production SnapStart deployment pays, and it is paid at publish,
    not per invoke. The benchmark measures invoke-time cold start, which is what
    the end-user feels.

### The `probe` subcommand is outside these invariants

`bencher probe` (`bencher/src/probe/`) is a **documentation-grade probe, not a
matrix scenario**, and it is deliberately NOT bound by the fairness and
measurement-purity rules above. It measures **caller-side wall-clock** around the
`Invoke` call to isolate the pre-Init download+start cost that no `REPORT`-line
signal can see, whereas the matrix records only in-Lambda REPORT timings and pins
the region precisely to *minimize* caller wall-clock as noise (`config.rs::REGION`).
Alongside that decomposition it also records the full cold/warm caller waits
(`w_cold`/`w_warm`, p50 + min–max), which the site publishes as the end-to-end
wait from the probe's vantage.
So the "same task per language", bundled-SDK, and reproducibility rules do not apply
to it: its output is single-account, region-specific magnitudes, written run-scoped
into `results/` (gitignored) and discovered by the site's data loader at build time,
framed as such, never into the matrix run data. Like the matrix run it is not
committed: a fresh clone has no probe data until a run produces it, and the build
fails loud rather than fall back to stale numbers. The one
purity rule it *does* honor is retry honesty (Measurement purity #1): its timed
invokes go through a retry-DISABLED client (`Aws::retryless_lambda_client`) so a
throttle fails loud instead of inflating the measured wall-clock, while the
control-plane cold-force mechanism reuses the driver's adaptive-retry client (the
same layer carve-out as #1's driver exception). On a transient hard error it
re-takes the whole failed unit (all N samples of a cell/size, buffer-then-commit
via `probe::retry_transient`), mirroring the matrix's `run_cell` retry; that
discards and re-measures a failed sample, it never retries *within* a timed
`Invoke`, so retry honesty holds. It has two explicit subcommands,
one per Cold Start Anatomy chart. Its **`download-start`** subcommand deploys
nothing (targets already-deployed matrix functions) and forces cold via the same
env-bump path as `run`.

Its **`download-scaling`** subcommand is the one part of
the codebase that *deploys* (ephemeral `lambdabench-synthdl-*` functions padded to
a range of sizes) and that **pads artifacts with inert filler**, which the matrix
deliberately avoids; the README [Matrix](README.md#matrix) section carries the
canonical rule for why padding fits here and not the matrix.
It runs two runtime families at each size (Python on `python3.14`, Rust on
`provided.al2023`, reusing the real compiled `hello` bootstrap so the function
still answers invokes) to confirm the download slope is family-independent, as a
platform-level cost should be. The synthetic functions are created, measured, and
torn down within the run (both on success and on error); their names are part of
the exact set `bencher teardown` reconstructs from config, so teardown is the hard
backstop if a run is interrupted before its own cleanup. The size set is a fixed
code const, not a CLI knob (like the matrix's iteration `Profile`), so teardown
enumerates every name the probe can create, no size can slip past it. They are
never part of the matrix, never recorded in the run data, and use a `synthdl` name
segment that cannot collide with a matrix `function_name`.

Its **container-image mode** (`download-scaling --with-image`) is the complement to
the zip synthetic sweep: per size it
assembles ONE padded container image on the managed Python base and deploys two
functions from it, `image-untouched` (padding baked in but never read, so Lambda's
lazy block-level loading may skip it) and `image-touched` (the handler reads the
padding at init, forcing the blocks in), measured with the same residual
subtraction. It exists to document that packaging *type* moves where artifact size
is paid: a `.zip` pays it in the unreported pre-Init residual, a container image
pays it in the reported `Init Duration` (see lifecycle.md "Zip vs container
image"). It falls under the same off-matrix framing as the zip mode, single
account, illustrative, never matrix data, and writes a separate run-scoped JSON
(`results/lifecycle-download-scaling-image-<run_id>.json`, gitignored and discovered
by the site loader at build time, like the zip mode's output). Two things make it distinct: it is the
ONLY path in the codebase that shells out to a container tool (`crane`) and pushes
to ECR (`ensure_ecr_repo` + `crane_ecr_login`); and it creates ECR resources
(`lambdabench-synthdl` repo + per-run image tags) that the per-run teardown deletes,
with `bencher teardown`'s exact-name delete of the `lambdabench-synthdl` repo (which
force-deletes its images) as the hard backstop.
crane is daemonless (base pull + one-layer append + CMD + push, no container-build
daemon or VM), so unlike a `docker`/`finch` build it runs unattended in the publish
pipeline: `probe download-scaling --with-image` is its own `deploy/run-benchmark.sh` step, and
the runner image carries the `crane` binary. The image JSON is therefore refreshed
every publish, exactly like the two zip probe JSONs.

### Keeping prose in sync with the data

Numbers in prose go stale when the platform changes; the project handles this in
three ways, one per surface:

1. **Rendered pages** (`index`, `comparison`, `rust`, `java-snapstart`,
   `appendix`) hardcode almost nothing, they render every figure from
   `stats.json`, so a re-run updates them automatically. This is the default and
   the reason measured results live in the run data, not in prose (see the top of
   this file).
2. **README "Finding" sections** state a few concrete magnitudes, but the
   volatile ones are labelled **dated one-off characterizations** ("taken 2026-06,
   …") and the surrounding prose is written as *shape* ("a cliff that steepens as
   memory shrinks"), not as a current magnitude, so it does not silently rot.
   The shape claims themselves (which series wins where: the SnapStart win/lose
   split, the Go-vs-Rust `batch` and `oneclient` orderings, the opt-level and
   jitter directions, `cache`'s non-GC tail floor, and the smithy-java version
   quoted in prose) are guarded by `scripts/check-findings-prose.py`, which the
   publish pipeline runs against the freshly built `stats.json` (non-fatal, like
   the lifecycle check below). If you state a new ordering in a finding, add a
   matching assertion there.
3. **`site/src/lifecycle.md`** is the one exception: it states *current*
   magnitudes from the off-matrix probes (the download+start table and the
   download-scaling chart) in prose, because those cannot come from `stats.json`.
   That is the primary drift surface, and it is guarded by
   `scripts/check-lifecycle-prose.py`, which asserts the key claims (the
   provisioning floor, the 200 MB residual, the ~4-8 ms/MB slope, the two runtime
   families tracking) still hold against the freshly-produced probe JSONs (it
   discovers the newest `results/lifecycle-*` the same way the site loaders do),
   with generous tolerances that catch a real platform shift rather than run noise.
   The publish pipeline runs it as a non-fatal step after the probes; on drift it warns and
   names the exact prose lines to revisit. If you add a new claimed number to
   that page, add a matching assertion there. One rounded echo lives outside
   that page: `site/src/index.md`'s cold-start caveat box states the same two
   probe magnitudes in order-of-magnitude words ("a couple hundred ms", "of
   order a second"), so if the guard flags drift, revisit that box too.

### Run shape

1. **CPU probes use a higher warm count, language- and memory-aware.** Via
   `Scenario::full_base_counts(lang, memory_mb)`: `lettercount`, `authz`, and
   `cache` run `5 cold × 1500 warm`: lettercount to build GC pressure, authz to
   let the warm hot path settle so the cross-runtime gap is measured cleanly, and
   cache because its signal IS the warm tail (P99/P99.9), which needs a long warm
   sequence to populate densely and to let the GC reach its steady-state cadence.
   `batch` runs `15 cold × 200 warm`: its GC pressure is per-invoke (every warm
   invoke parses the whole ~16 MB batch), so it does not need a high warm count.
   (Memory floors — which tiers a scenario/runtime pair skips, and why — are
   enumerated once, in the README [Matrix](README.md#matrix) section.) The light
   scenarios run `50 cold × 50 warm` (`FULL_LIGHT_COUNTS`). All of the above is the
   `full` profile; `--profile smoke` instead runs a tiny flat count over every cell
   (for a quick end-to-end pipeline sanity pass). The per-cell count is a pure
   function of the profile and the cell (`Cell::iterations`), and the resolved
   counts that actually ran are recorded in the meta's `iteration_buckets`.
   - **Starved-tier reduction.** The full counts assume a compiled runtime
     finishing each invoke in single-digit ms. On the *slowest* runtime (Python)
     at the *smallest* CPU tiers (128/256 MB ≈ 0.07-0.15 vCPU) the same counts run
     for tens of minutes per cell and drag the whole run (measured: Python
     `lettercount`@128 ≈ 50 min, `batch`@256 ≈ 35 min). There the counts are
     thinned (`lettercount` 1500→300 warm, `batch` 15→5 cold) while **all
     memory tiers are kept**. We thin samples, not tiers, deliberately: the
     low-memory cliff is the most interesting CPU-probe data, and the
     cross-runtime gap there is wide and stable enough to show with fewer samples;
     dropping Python's low tiers would instead make the cross-language chart
     asymmetric exactly where the contrast is sharpest. The planned-invocation
     estimate and the run loop share this one function, so they cannot disagree.

---

## How to add a scenario

A scenario touches these places. Miss one and you get a build error, a missing
function, or (worse) a silently-wrong chart.

**Rust driver (`bencher/src/`):**
- `config.rs`: add the `Scenario` enum variant; add it to `Scenario::ALL`
  (update the array length); add arms to `as_str()`, `needs_ddb()`,
  `needs_kms_s3()`, `needs_s3()`, and `full_base_counts()` as appropriate.
- `build.rs`: add the scenario → cargo package (or gradle project) mapping. The
  Python and Go paths are generic (Python needs `scenarios/python/<scenario>/
  handler.py` + optional `requirements.txt`; Go needs
  `scenarios/go/<scenario>/main.go` in the shared `scenarios/go` module), so no
  per-scenario edit is needed there unless the scenario embeds an extra file (cf.
  `authz`'s JWK copy in both `build_python` and `build_go`).
- `config.rs`: if the new scenario is one Python or Go cannot host fairly,
  exclude it in `Lang::supports`. Otherwise add an `scenarios/python/<scenario>/`
  and `scenarios/go/<scenario>/` handler (below). Go binaries are always
  arch-specific (handled by `arch_significant`); decide whether the Python bundle
  is arch-specific (native wheel) by updating `arch_significant` too.
- `aws/lambda.rs`: add any env wiring in `environment()`, and an `invoke_payload`
  arm if the scenario needs a non-`{}` payload.
- `main.rs`: add the `--only` parser arm for the scenario name.

**Artifacts:**
- `scenarios/rust/<scenario>/`: new bin crate (Cargo.toml + src/main.rs). Add it
  to the root `Cargo.toml` workspace `members` (unless it depends on gradle codegen,
  in which case `exclude` it and declare `[profile.release]` locally).
- `scenarios/node/<scenario>/index.mjs`: the Node handler.
- `scenarios/node/build.mjs`: add the scenario to the `scenarios` array.
- `scenarios/java/<scenario>/`: new Gradle subproject (`build.gradle.kts` + a
  `lambdabench.<Scenario>Handler` implementing `RequestHandler`, or a
  `SmithyServiceProvider` + `smithy-build.json` for a smithy scenario). Add it to
  `scenarios/java/settings.gradle.kts` `include(...)`. The handler class name must
  match `Cell::handler()` in `config.rs`. The root `build.gradle.kts` supplies the
  Java 25 toolchain and the `buildZip` task.
- `scenarios/python/<scenario>/handler.py`: exposes a `handler(event, context)`
  function (matching `Cell::handler()`'s `handler.handler`). Add a
  `requirements.txt` if it needs third-party deps (bundled via `pip install
  --target` for the `python3.14` runtime); skip it for a stdlib-only handler like
  `hello`. Skip the whole directory if Python does not host the scenario.
- `scenarios/go/<scenario>/main.go`: a `package main` with `lambda.Start(...)`
  in the shared `scenarios/go` Go module (so it deploys a `bootstrap` executable,
  matching `Cell::handler()`). Add any new third-party dep with `go get` and run
  `go mod tidy` in `scenarios/go`. Skip the whole directory if Go does not host
  the scenario.

**Visualization (`site/src/lib/format.js`):**
- `SCENARIO_LABELS`: full label.
- `KNOWN_SCENARIO_ORDER`: display order.
- `SHORT_SCENARIO`: compact label for dense dumbbell/distribution axes.
- `SCENARIO_BLURBS`: the one-line `does` / `why` shown in the Scenarios
  reference block (keep `does` to mechanics and `why` to the question tested, not
  a winner; see the comment there).

**Docs:**
- `README.md`: scenario table, matrix function count (the `config.rs` test
  `all_cells_count_matches_readme` fails until it is updated), any run-time notes.
- This file: if the scenario establishes or relies on a new invariant.

**Before you commit, re-check the Invariants above**, especially Measurement
purity #1 (disable SDK retries on every client you construct) and Fairness #2
(symmetric native libs).
