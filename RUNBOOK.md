# Running a full sweep

How the driver issues ~10^6 invocations against the live Lambda control plane over hours without
throttling or recording a single bad sample. This is operational reference material: read it when tuning
a run or debugging one. For *what* the benchmark measures and how to invoke it, see
[README.md](README.md); for the design rules and invariants, see [DESIGN.md](DESIGN.md).

## Concurrency, quota & the pool

`--pool N` controls how many functions are benchmarked **concurrently** (always serial *within* a
function, so warm invokes keep hitting the same sandbox). The limit on `N` is **not** local CPU,
memory, or sockets: the driver is network-I/O-bound and barely registers on the machine. The
constraint is an **AWS Lambda control-plane quota**, which the driver is engineered around so the pool
can be large.

**Default: `--pool 32`** (`DEFAULT_POOL`), and 32 is safe **structurally**: every call on the scarce
15 req/s control-plane bucket (`UpdateFunctionConfiguration` per cold-force, plus
`PublishVersion`/`DeleteFunction` on the SnapStart path) is issued through the shared
`CONTROL_PLANE_TPS` rate limiter (below), which caps the aggregate issue rate directly; pool size
controls only invoke (data-plane) concurrency, which is exempt. So a larger pool queues more cells on
the limiter but can never push the issue rate over it. Since the rate-capped Update is the cold-force
bottleneck, a larger pool does **not** raise cold-start throughput anyway; past 32 more concurrency
mostly just deepens the queue on the limiter, so 32 is the speed/safety sweet spot. (A multi-hour run
from a low-latency client near the region sees 0 `Rate exceeded` and 0 cell retries; an account already
spending its control-plane quota elsewhere has less headroom, which is what the limiter's 12-of-15
margin is for.)

**The quota, and why the pool is not bound by it.** Each cold cycle does a few control-plane
calls. The scarce one is:

> **Lambda control-plane API quota ("remainder" bucket): 15 requests/second, account-wide, across all
> APIs combined (not per-API), EXCLUDING `Invoke`, `GetFunction`, and `GetPolicy`. Cannot be increased**
> ([documented](https://docs.aws.amazon.com/lambda/latest/dg/gettingstarted-limits.html)).

Two design choices keep a large pool safely under it:

1. **Readiness polling uses `GetFunction`, not `GetFunctionConfiguration`.** The poll loop in
   `wait_ready` is the bulk of control-plane traffic (several polls per cold-force). `GetFunction` has
   its **own 100 req/s quota** (also documented, and excluded from the remainder bucket) and returns the
   same `RevisionId`/`State`/`LastUpdateStatus`, so the polls stay off the scarce 15/s bucket entirely.
   (`current_revision`, the pre-update read, uses `GetFunction` for the same reason.)
2. **A shared rate limiter gates every remaining 15/s call.** After (1), the calls left on the 15/s
   bucket are `UpdateFunctionConfiguration` (per cold-force), `PublishVersion`/`DeleteFunction` (the
   SnapStart path), and teardown's `DeleteFunction` sweep. Each acquires a global token-bucket rate
   limiter (`aws::CONTROL_PLANE_TPS`, currently 12) before it is issued, so the aggregate issue rate
   is capped **directly**, regardless of how many cells are in flight or how fast individual calls
   return. 12 sits below the 15/s quota with headroom for jitter, SDK retries, and anything else in
   the account touching the same bucket.

Warm invokes use the **`Invoke`** API, which is exempt from the 15/s quota anyway (its limit is
10 × concurrency, far out of reach), and they dominate wall-clock, so the pool speeds up the part that
matters while the cold-force machinery stays rate-safe. Net: **pool 32 is the rate-safe default; the
binding limit is the 15/s Update bucket, not the pool, so if you ever see control-plane throttling lower
`CONTROL_PLANE_TPS` rather than chasing it with a bigger pool.**

**Adaptive retry (backstop).** The AWS clients use `RetryConfig::adaptive()` (generous attempts and
backoff): on a `TooManyRequestsException` the SDK backs off *and* runs a client-side rate limiter that
proactively slows the request rate. With the issue rate already capped by `CONTROL_PLANE_TPS` this
should rarely engage, but it absorbs any
residual burst transparently rather than aborting a multi-hour run. The long max-backoff only applies to
throttled control-plane calls inside `force_cold` (where a cold start is wanted anyway); it cannot push
a warm invoke cold, and fail-loud would catch it if it somehow did.

**Why so long.** Wall-clock is dominated by warm invokes (one sequential network round-trip each,
~150 ms), not the cold-forces (each cold-force costs ~3.5 s of `UpdateFunctionConfiguration` settle
time, but there are far fewer of them). Run time therefore scales with total warm invokes. The five
light/I/O scenarios are quick; the four CPU probes (`lettercount`, `authz`, `batch`, `cache`) are the
expensive ones: `lettercount`, `authz`, and `cache` because their per-scenario count runs long warm
sequences (`5 × 1500`), and `batch` because each warm invoke does heavy per-invoke work (parsing a
~16 MB batch) over its `15 × 200` count. Budget for the four dominating a full run. Use `--profile smoke`
for a quick low-count pass (or edit the counts in `config.rs::Scenario::full_base_counts`) to trade
runtime against sample count, and consider running the CPU probes separately via a scoped `--only`
(the README [Usage](README.md#usage) recipes include one) if you want a quick pass over the rest.

## Reliability: failure modes & safeguards

A full run issues ~10^6 invocations and tens of thousands of forced cold starts (one per cold
cycle per function: `cold_cycles × functions`) against the live Lambda control plane over hours. At
that scale, rare-but-real AWS behaviours show up. The tenet is
**fail loud, never silently fall back**: a row is never recorded unless it is a genuine,
invariant-holding sample. On
top of that, *mechanisms* (forcing a cold start, keeping a warm sandbox) are retried when they hit a
transient, because a transient is not bad data, it is the same measurement that just needs another
attempt. Each safeguard below targets a specific failure mode that surfaces at this scale.

| Failure mode                                                                                                                | Root cause                                                                                                                                                                                                                                        | Safeguard                                                                                                                                                                                                                                                                          | Where                                                     |
|-----------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-----------------------------------------------------------|
| Expected-cold invoke comes back **warm**, deterministically (every cycle)                                                   | `wait_ready` returning on a **stale `Successful`**: `GetFunctionConfiguration` is read-after-write inconsistent and can report the *previous* update's `Successful` before the new one registers, so the invoke hits the still-warm prior sandbox | Wait until the `RevisionId` has **changed from the pre-update value** *and* `LastUpdateStatus == Successful`                                                                                                                                                                       | `aws/lambda.rs` `wait_ready` / `force_cold`               |
| Waiting on the Update-returned revision id **never settles**                                                                | A config update mints **one revision while `InProgress`** (the one the Update call returns) and a **different** revision once `Successful`, so "Update-returned id" and "Successful" are never observable together                                | Compare against the **pre-update** revision (captured before the call), not the id the Update returns                                                                                                                                                                              | `aws/lambda.rs` `force_cold` comment                      |
| Expected-cold invoke comes back warm **transiently** (1–6 retries then fine)                                                | **Data-plane propagation lag**: after the control plane marks the update `Successful`, the invoke router can still route to the old (warm) sandbox for up to ~tens of seconds, especially for heavy Node functions just used                      | Re-force the cold start, up to `MAX_COLD_FORCE_ATTEMPTS` (6), discarding the warm result                                                                                                                                                                                           | `run.rs` `force_cold_invoke`                              |
| Expected-**warm** invoke comes back **cold** mid-cycle                                                                      | AWS **spontaneously recycles** the warm execution environment between two back-to-back invokes (host maintenance / capacity rebalancing)                                                                                                          | Discard the whole cycle's buffered rows and re-run the cycle, up to `MAX_CYCLE_ATTEMPTS` (4)                                                                                                                                                                                       | `run.rs` `run_one_cycle`                                  |
| A single cell fails on a **one-off** infrastructure hiccup (e.g. a config update that never settles within the wait budget) | Rare tail events are *likely* across tens of thousands of cold-forces; with strict fail-fast, one would abort the entire multi-cell run                                                                                                           | Buffer **all** of a cell's rows and flush only on full success; re-run the whole cell up to `MAX_CELL_ATTEMPTS` (3). A *persistent* failure still exhausts retries and aborts (fail-loud preserved)                                                                                | `run.rs` `run_cell`                                       |
| `wait_ready` **times out** on a normal-but-slow update                                                                      | A poll budget tighter than AWS's own `FunctionUpdated` waiter (300 s) gives up early; a congested control-plane update can legitimately exceed a minute                                                                                           | Poll budget is **300 s** (600 × 500 ms), matching boto3                                                                                                                                                                                                                            | `aws/lambda.rs` `wait_ready`                              |
| `TooManyRequestsException: Rate exceeded` under a large pool                                                                | The cold-force `Update` and readiness-poll calls both press on the **account-wide 15 req/s control-plane quota** (cannot be increased); left on one bucket, a large pool bursts over it                                                           | Readiness polling uses `GetFunction` (own 100 req/s quota) so only `Update` stays on the 15/s bucket, and a shared rate limiter (`CONTROL_PLANE_TPS`) caps the aggregate issue rate directly, independent of pool size; `RetryConfig::adaptive()` is the backstop | `aws/lambda.rs` `wait_ready` / `force_cold`, `aws.rs` |
| S3 `503 SlowDown` inside the `smithyfull`/`threeclient` handler                                                             | A **single shared S3 key** across functions concentrates all PUTs on one object (S3 partitions per key)                                                                                                                                           | Each function writes its **own** fixed receipt key / order PK (`<fn>/lambdabench-receipt`, `lambdabench-order-<fn>`), the function name **leads** the S3 key so writes spread across partition prefixes, still idempotent and bounded                                              | `aws/lambda.rs` `environment`                             |
| `ExpiredTokenException` mid-run (only if creds are short-lived)                                                             | Credentials expired before the run finished                                                                                                                                                                                                       | Validate identity up front so a bad/expired credential fails immediately, not hours in. Long-lived STS session credentials (e.g. a 15 h session) comfortably cover a full run, so this is not a practical concern with them; it only bites if you run under a short-TTL credential | `aws.rs` `Aws::load`                                  |

The retries **nest**: `force_cold_invoke` (cold-force) inside `run_one_cycle` (cycle) inside `run_cell`
(whole cell). Each absorbs a different transient; a failure that is *persistent* (a real handler bug,
OOM, a non-200 status, an unparseable `REPORT`) exhausts every layer and aborts the run with the full
decoded log tail. No safeguard ever records a warm sample as cold or vice versa: buffering is
per-cycle and per-cell, flushed only once invariants hold.
