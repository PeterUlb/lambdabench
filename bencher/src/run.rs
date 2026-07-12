//! The benchmark run loop.
//!
//! For each function (cell): repeat `cold_cycles` times { bump COLD_NONCE to
//! force a fresh sandbox; invoke once (cold); invoke `warm_per_cycle` times
//! (warm) on that same sandbox }. Strictly serial within a function so warm
//! invokes always hit the warm sandbox; functions run concurrently up to the
//! pool size, since independent functions never share sandboxes.
//!
//! Fail-loud invariants (no fallback): a cold invoke MUST report a cold marker
//! (Init Duration for a plain cold start; Restore Duration for a SnapStart
//! restore), a warm invoke MUST NOT. The SnapStart path is stricter still: it
//! requires a Restore Duration specifically and rejects an Init-Duration cold,
//! so a version where SnapStart silently failed to apply cannot be mislabeled as
//! a SnapStart sample (see the restore-invoke check below). Status must be 200
//! and there must be no FunctionError. Any violation aborts the whole run.

use crate::aws::Aws;
use crate::config::Cell;
use crate::parse::parse_report;
use crate::record::{InvocationRow, ResultsWriter};
use anyhow::{Context, Result, bail};
use futures::stream::{self, TryStreamExt};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Current UTC wall-clock time as `HH:MM:SS`, for stamping progress lines so a
/// stall is visible at a glance. Derived from the shared epoch-ms clock (no date
/// dependency).
fn utc_hms() -> String {
    let secs = (crate::config::now_unix_ms() / 1000) as u64;
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

/// Tunable parameters for a run.
#[derive(Clone, Copy)]
pub struct RunParams {
    /// The iteration-count profile (`full` for the published methodology, `smoke`
    /// for a quick pipeline sanity pass). The per-cell cold/warm counts are
    /// derived from this via `Cell::iterations`, the single source of truth, so a
    /// run is fully described by the profile plus the matrix.
    pub profile: crate::config::Profile,
    pub pool: usize,
}

/// Runs the benchmark over the given cells, appending rows to the writer.
pub async fn run(
    aws: &Aws,
    cells: &[Cell],
    params: RunParams,
    run_id: &str,
    sizes: &BTreeMap<String, crate::deploy::ArtifactSizes>,
    writer: Arc<ResultsWriter>,
) -> Result<()> {
    let total = cells.len();
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Fail-fast: `try_for_each_concurrent` runs up to `pool` cells at once and, on
    // the first `Err`, stops pulling new cells and drops the in-flight futures
    // (which cancels them at their next `.await`). Cells are independent and rows
    // are written synchronously between awaits, so cancellation loses no committed
    // data and cannot leave a half-written line.
    let outcome = stream::iter(cells.iter().copied().map(Ok))
        .try_for_each_concurrent(params.pool, |cell| {
            let aws = aws.clone();
            let writer = writer.clone();
            let run_id = run_id.to_string();
            // The size map is built from the same cell set this run iterates, so
            // every cell must be present. A miss means an upstream inconsistency,
            // not a zero-size artifact: fail loud rather than record placeholder
            // sizes.
            let size = sizes.get(&cell.function_name()).copied();
            let done = done.clone();
            async move {
                let size = match size {
                    Some(s) => s,
                    None => {
                        let e = anyhow::anyhow!(
                            "no artifact sizes for cell {} (size map and run cells disagree)",
                            cell.function_name()
                        );
                        let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        eprintln!(
                            "[{}] [{n}/{total}] FAILED {}: {e:#}",
                            utc_hms(),
                            cell.function_name()
                        );
                        return Err(e);
                    }
                };
                let r = run_cell(&aws, &cell, params, &run_id, size, &writer).await;
                let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                let ts = utc_hms();
                match &r {
                    Ok(()) => println!("[{ts}] [{n}/{total}] done {}", cell.function_name()),
                    Err(e) => {
                        eprintln!(
                            "[{ts}] [{n}/{total}] FAILED {}: {e:#}",
                            cell.function_name()
                        )
                    }
                }
                r
            }
        })
        .await;

    // Drain any SnapStart versions whose cycle was cancelled by fail-fast before
    // its inline `delete_version` could run (see `VersionGuard`). Done here so
    // cleanup stays on the runtime and ahead of main returning; a `tokio::spawn`
    // from Drop would race runtime shutdown. Best-effort: a failure is logged and
    // does not mask the run's outcome.
    let leaked: Vec<(String, String)> = std::mem::take(
        &mut *aws
            .leaked_versions
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    if !leaked.is_empty() {
        eprintln!(
            "[{}] reclaiming {} leaked SnapStart version(s) from cancelled cells",
            utc_hms(),
            leaked.len()
        );
        for (name, version) in leaked {
            if let Err(e) = aws.delete_version(&name, &version).await {
                eprintln!(
                    "[{}]   warning: failed to delete leaked version {name}:{version}: {e:#}",
                    utc_hms()
                );
            }
        }
    }

    outcome
}

/// Maximum re-forces of a cold start when the first invoke after a `COLD_NONCE`
/// bump still lands warm. The control plane confirming `Successful` is eventually
/// consistent with the data plane retiring the old sandbox, so an invoke can
/// briefly still hit it. Not a data fallback: a warm result is never recorded as
/// cold; we retry the mechanism until it delivers a cold sandbox, else abort.
const MAX_COLD_FORCE_ATTEMPTS: u32 = 6;

/// Escalating backoff between cold-force retries (`BASE * attempt`, capped): early
/// retries stay fast for the common case (old sandbox retired within a second or
/// two), later ones spread out to ride out the rare deep propagation lag (tens of
/// seconds) rather than exhausting all attempts inside a too-narrow window, which
/// would force a costly whole-cell re-run instead of letting the cell self-heal in
/// place.
const COLD_FORCE_RETRY_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_millis(1500);
/// Upper bound on a single cold-force backoff, so the escalation stays bounded.
const COLD_FORCE_RETRY_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(12);

/// Maximum re-runs of a cold+warm cycle when a *warm* invoke unexpectedly lands on
/// a freshly cold sandbox: AWS can spontaneously retire a warm environment between
/// back-to-back invokes (host maintenance, capacity rebalancing), which taints the
/// cycle's warm series. Not a data fallback: an invariant-violating cycle is never
/// recorded; we re-run until a clean one is obtained, else abort.
const MAX_CYCLE_ATTEMPTS: u32 = 4;

/// Backoff before re-running a cycle after a spontaneous warm-sandbox recycle.
const CYCLE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1000);

/// The outcome of one attempt at a cold+warm cycle.
enum CycleOutcome {
    /// All invariants held; carries the buffered rows ready to flush.
    Complete(Vec<InvocationRow>),
    /// A warm invoke at this index came back cold (spontaneous AWS recycle);
    /// the cycle must be discarded and re-run.
    WarmRecycled { idx: u32 },
}

/// Maximum re-runs of a cell's cycle loop when an attempt fails. The per-cycle
/// and cold-force retries absorb common transients; this outer retry covers rarer
/// one-off hiccups that surface as a cell-level error (a config update that never
/// settles, a transient platform error), which are likely to fire at least once
/// across a full run's thousands of cold-forces. A persistent failure (a real bug,
/// OOM) still exhausts all attempts and aborts, so fail-loud holds. Rows are
/// buffered per cell and flushed only on full success, so a re-run never leaves
/// partial/duplicate rows; a retry resumes at the cycle that failed, keeping the
/// completed cycles' rows (see `run_cell`).
const MAX_CELL_ATTEMPTS: u32 = 3;

/// Backoff before re-running a whole cell after a failed attempt, giving a
/// transient control-plane / platform condition time to clear.
const CELL_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

/// The resolved cold/warm iteration counts for one cell under the run's profile.
/// Resolved once per cell (via `Cell::iterations`) and threaded through the cell's
/// cycle loop, so the count the loop runs is exactly the count `main` planned for.
#[derive(Clone, Copy)]
struct Counts {
    cold: u32,
    warm: u32,
}

/// Runs all cold/warm cycles for a single function, retrying the cycle loop on a
/// one-off failure. Buffers every row across all cycles and flushes them only
/// once the entire cell completes cleanly, so a re-run never duplicates or
/// half-writes a cell.
///
/// A retry resumes at the cycle that failed, keeping the completed cycles' rows:
/// cycles are independent (each forces its own fresh cold sandbox and validates
/// coldness/warmness per invoke), so a later failure does not invalidate them.
async fn run_cell(
    aws: &Aws,
    cell: &Cell,
    params: RunParams,
    run_id: &str,
    sizes: crate::deploy::ArtifactSizes,
    writer: &ResultsWriter,
) -> Result<()> {
    // Resolve the cold/warm counts that apply to THIS cell under the run's
    // profile. The pool size is a run-wide concurrency knob, never per-cell. Only
    // the iteration counts vary per cell; the retry/buffering structure is shared.
    let (cold, warm) = cell.iterations(params.profile);
    let counts = Counts { cold, warm };

    // Buffered rows and the next cycle to run, both surviving across attempts so
    // a retry picks up exactly where the failed attempt stopped.
    let mut rows: Vec<InvocationRow> =
        Vec::with_capacity(counts.cold as usize * (1 + counts.warm as usize));
    let mut cycle: u32 = 0;

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=MAX_CELL_ATTEMPTS {
        match run_cycles(aws, cell, counts, run_id, sizes, &mut cycle, &mut rows).await {
            Ok(()) => {
                // Hand the whole completed cell to the writer at once: it appends
                // the rows and sync-flushes them as a unit, matching the cell-level
                // atomicity of the buffering above (a crash keeps every finished
                // cell). Per-row flushing would cost ~70% in size for no extra
                // durability, since rows are never written individually.
                writer.write_cell(&rows)?;
                return Ok(());
            }
            Err(e) => {
                // Log and back off only when a retry will actually follow; the
                // final attempt's error propagates below and is logged as FAILED
                // by the caller.
                if attempt < MAX_CELL_ATTEMPTS {
                    eprintln!(
                        "[{}]   cell retry {attempt}/{MAX_CELL_ATTEMPTS} for {} (resuming at cycle {cycle}/{}): {e:#}",
                        utc_hms(),
                        cell.function_name(),
                        counts.cold
                    );
                    tokio::time::sleep(CELL_RETRY_BACKOFF).await;
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.expect("loop ran at least once")).with_context(|| {
        format!(
            "cell {} failed after {MAX_CELL_ATTEMPTS} attempts",
            cell.function_name()
        )
    })
}

/// Runs the cell's remaining cycles, starting at `*cycle`, appending each
/// completed cycle's rows to `rows` and advancing `*cycle` past it. On an error
/// the two out-params reflect exactly the completed prefix: a failed cycle
/// contributes no rows (`run_one_cycle` returns them only once the whole cycle's
/// invariants hold) and does not advance the counter, so the caller's retry
/// resumes at the failed cycle with every clean cycle's rows intact.
async fn run_cycles(
    aws: &Aws,
    cell: &Cell,
    counts: Counts,
    run_id: &str,
    sizes: crate::deploy::ArtifactSizes,
    cycle: &mut u32,
    rows: &mut Vec<InvocationRow>,
) -> Result<()> {
    while *cycle < counts.cold {
        let cycle_rows = run_one_cycle(aws, cell, counts, run_id, sizes, *cycle)
            .await
            .with_context(|| format!("cycle {} of {}", *cycle, cell.function_name()))?;
        rows.extend(cycle_rows);
        *cycle += 1;
    }
    Ok(())
}

/// Runs one cold+warm cycle, buffering all rows and returning them only once the
/// entire cycle holds its invariants. If a warm invoke lands on a freshly cold
/// sandbox (AWS spontaneously recycled the warm environment mid-cycle), the
/// buffered rows are dropped and the whole cycle is re-run. A partial or
/// invariant-violating cycle is never returned.
async fn run_one_cycle(
    aws: &Aws,
    cell: &Cell,
    counts: Counts,
    run_id: &str,
    sizes: crate::deploy::ArtifactSizes,
    cycle: u32,
) -> Result<Vec<InvocationRow>> {
    for attempt in 1..=MAX_CYCLE_ATTEMPTS {
        match try_one_cycle(aws, cell, counts, run_id, sizes, cycle).await? {
            CycleOutcome::Complete(rows) => return Ok(rows),
            CycleOutcome::WarmRecycled { idx } => {
                eprintln!(
                    "[{}]   warm-recycle retry {attempt}/{MAX_CYCLE_ATTEMPTS} for {} (cycle {cycle}): warm invoke {idx} landed cold (AWS recycled the sandbox), re-running cycle",
                    utc_hms(),
                    cell.function_name()
                );
                tokio::time::sleep(CYCLE_RETRY_BACKOFF).await;
            }
        }
    }
    bail!(
        "{} (cycle {cycle}): failed to complete a clean cycle in {MAX_CYCLE_ATTEMPTS} attempts (warm sandbox kept getting recycled mid-cycle)",
        cell.function_name()
    )
}

/// Attempts one cold+warm cycle, returning the buffered rows if every invariant
/// held, or `WarmRecycled` if a warm invoke came back cold. The cold start is
/// obtained one of two ways depending on the cell:
///   - non-SnapStart: bump `COLD_NONCE` on `$LATEST` to retire the warm sandbox,
///     then invoke `$LATEST` (retried until it lands cold, see
///     `force_cold_invoke`); warm invokes hit the same `$LATEST` sandbox.
///   - SnapStart: publish a fresh version (whose snapshot has never been
///     restored), then invoke that version: its first invoke is deterministically
///     a cold restore, so no retry loop is needed; warm invokes hit the same
///     version. The version is deleted at the end of the cycle.
///
/// Platform errors and a cold invoke missing its cold marker still fail loud.
async fn try_one_cycle(
    aws: &Aws,
    cell: &Cell,
    counts: Counts,
    run_id: &str,
    sizes: crate::deploy::ArtifactSizes,
    cycle: u32,
) -> Result<CycleOutcome> {
    if cell.snapstart {
        try_one_snapstart_cycle(aws, cell, counts, run_id, sizes, cycle).await
    } else {
        try_one_latest_cycle(aws, cell, counts, run_id, sizes, cycle).await
    }
}

/// The non-SnapStart cold+warm cycle: cold-force on `$LATEST` (qualifier `None`)
/// with the retry-on-warm loop, then warm invokes on the same sandbox.
async fn try_one_latest_cycle(
    aws: &Aws,
    cell: &Cell,
    counts: Counts,
    run_id: &str,
    sizes: crate::deploy::ArtifactSizes,
    cycle: u32,
) -> Result<CycleOutcome> {
    let mut rows: Vec<InvocationRow> = Vec::with_capacity(1 + counts.warm as usize);

    // Force a cold sandbox and take the single cold invoke (idx 0). Retries the
    // force if the invoke lands warm, so the cold row is always a genuine cold
    // start.
    let (nonce, cold_row) = force_cold_invoke(aws, cell, run_id, sizes, cycle)
        .await
        .with_context(|| format!("cold invoke cycle {cycle} of {}", cell.function_name()))?;
    rows.push(cold_row);

    match collect_warm_invokes(
        aws, cell, counts, run_id, sizes, cycle, &nonce, None, &mut rows,
    )
    .await?
    {
        Some(idx) => Ok(CycleOutcome::WarmRecycled { idx }),
        None => Ok(CycleOutcome::Complete(rows)),
    }
}

/// RAII guard that deletes a published SnapStart version even if its cycle future
/// is cancelled mid-await (fail-fast drops in-flight cells). The inline cleanup
/// path calls `disarm()` once `delete_version` runs; a still-armed drop pushes
/// `(name, version)` onto `Aws::leaked_versions`, which `run::run` drains before
/// main returns. (A `tokio::spawn` from Drop would race the runtime shutdown that
/// follows fail-fast.)
struct VersionGuard {
    aws: Aws,
    name: String,
    version: Option<String>,
}

impl VersionGuard {
    fn new(aws: Aws, name: String, version: String) -> Self {
        Self {
            aws,
            name,
            version: Some(version),
        }
    }

    fn disarm(&mut self) {
        self.version = None;
    }
}

impl Drop for VersionGuard {
    fn drop(&mut self) {
        if let Some(version) = self.version.take() {
            // Cannot await in Drop, so hand the pair to the run-loop drain. Recover
            // from a poisoned lock rather than swallow the leak record.
            let mut q = self
                .aws
                .leaked_versions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            q.push((self.name.clone(), version));
        }
    }
}

/// The SnapStart cold+warm cycle: publish a fresh version (guaranteed-cold
/// restore on first invoke), invoke it once cold then `warm_per_cycle` times
/// warm, all on that version qualifier, then delete the version. No cold-force
/// retry loop: a freshly published version has no warm environment, so its first
/// invoke is always a restore; if it somehow is not, fail loud rather than
/// recording a non-cold sample as cold.
async fn try_one_snapstart_cycle(
    aws: &Aws,
    cell: &Cell,
    counts: Counts,
    run_id: &str,
    sizes: crate::deploy::ArtifactSizes,
    cycle: u32,
) -> Result<CycleOutcome> {
    let name = cell.function_name();
    let nonce = uuid::Uuid::new_v4().to_string();
    let version = aws
        .publish_cold_version(cell, &nonce)
        .await
        .with_context(|| format!("publishing cold version for {name} (cycle {cycle})"))?;

    // Arm a drop guard so the version is deleted even if this future is cancelled
    // mid-await by a sibling cell's failure (see `VersionGuard`). The inline path
    // below disarms it once the delete is attempted.
    let mut guard = VersionGuard::new(aws.clone(), name.clone(), version.clone());

    let outcome =
        run_snapstart_invokes(aws, cell, counts, run_id, sizes, cycle, &nonce, &version).await;
    if let Err(e) = aws.delete_version(&name, &version).await {
        eprintln!(
            "[{}]   warning: failed to delete version {name}:{version}: {e:#}",
            utc_hms()
        );
    }
    // Inline delete attempted (success or logged failure): disarm so the Drop
    // path does not re-issue it. The Drop path exists solely for cancellation.
    guard.disarm();
    outcome
}

/// The invoke sequence of a SnapStart cycle (cold restore + warm tail) on a
/// pre-published version qualifier. Separated from `try_one_snapstart_cycle` so
/// the version deletion can wrap it unconditionally.
#[allow(clippy::too_many_arguments)]
async fn run_snapstart_invokes(
    aws: &Aws,
    cell: &Cell,
    counts: Counts,
    run_id: &str,
    sizes: crate::deploy::ArtifactSizes,
    cycle: u32,
    nonce: &str,
    version: &str,
) -> Result<CycleOutcome> {
    let mut rows: Vec<InvocationRow> = Vec::with_capacity(1 + counts.warm as usize);

    // Cold restore invoke (idx 0) on the fresh version.
    let res = aws
        .invoke_tail(cell, Some(version))
        .await
        .with_context(|| {
            format!(
                "cold restore invoke cycle {cycle} of {}",
                cell.function_name()
            )
        })?;
    check_platform_ok(cell, &res)?;
    let report = parse_report(&res.log_tail)
        .with_context(|| format!("parsing REPORT for {}", cell.function_name()))?;
    if report.restore_ms.is_none() {
        // A SnapStart cold start MUST report a `Restore Duration`. Require it
        // specifically, not the looser `is_cold()`: if SnapStart silently failed to
        // apply, the version cold-starts with `Init Duration`, which `is_cold()`
        // would accept and mislabel as a SnapStart sample. Fail loud instead.
        bail!(
            "{} (cycle {cycle}): first invoke of fresh version {version} did not report a Restore Duration (SnapStart did not restore, init_ms={:?}); log:\n{}",
            cell.function_name(),
            report.init_ms,
            res.log_tail
        );
    }
    rows.push(build_row(
        cell, run_id, sizes, cycle, 0, true, nonce, &res, &report,
    ));

    match collect_warm_invokes(
        aws,
        cell,
        counts,
        run_id,
        sizes,
        cycle,
        nonce,
        Some(version),
        &mut rows,
    )
    .await?
    {
        Some(idx) => Ok(CycleOutcome::WarmRecycled { idx }),
        None => Ok(CycleOutcome::Complete(rows)),
    }
}

/// Issues the warm tail of a cycle (idx `1..=warm_per_cycle`) against the given
/// qualifier, appending a row per invoke to `rows`. Returns `Some(idx)` if a
/// warm invoke unexpectedly came back cold (the caller treats this as a
/// spontaneous warm-sandbox recycle and re-runs the cycle), or `None` if every
/// warm invoke held the invariant. Shared by the `$LATEST` and SnapStart paths.
#[allow(clippy::too_many_arguments)]
async fn collect_warm_invokes(
    aws: &Aws,
    cell: &Cell,
    counts: Counts,
    run_id: &str,
    sizes: crate::deploy::ArtifactSizes,
    cycle: u32,
    nonce: &str,
    qualifier: Option<&str>,
    rows: &mut Vec<InvocationRow>,
) -> Result<Option<u32>> {
    for idx in 1..=counts.warm {
        let res = aws.invoke_tail(cell, qualifier).await.with_context(|| {
            format!(
                "warm invoke {idx} cycle {cycle} of {}",
                cell.function_name()
            )
        })?;
        check_platform_ok(cell, &res)?;
        let report = parse_report(&res.log_tail)
            .with_context(|| format!("parsing REPORT for {}", cell.function_name()))?;

        if report.is_cold() {
            // The warm sandbox was retired between invokes and this one cold
            // started. Abandon the cycle; its warm samples are no longer a clean
            // series on one sandbox.
            return Ok(Some(idx));
        }

        rows.push(build_row(
            cell, run_id, sizes, cycle, idx, false, nonce, &res, &report,
        ));
    }
    Ok(None)
}

/// Forces a cold sandbox and takes the single cold invoke (idx 0) for a cycle,
/// retrying the force when the invoke lands on a warm sandbox. Returns the nonce
/// of the successful cold start (so the warm invokes can carry the same value)
/// and the buffered cold row. Aborts if no cold start is obtained within
/// `MAX_COLD_FORCE_ATTEMPTS`.
async fn force_cold_invoke(
    aws: &Aws,
    cell: &Cell,
    run_id: &str,
    sizes: crate::deploy::ArtifactSizes,
    cycle: u32,
) -> Result<(String, InvocationRow)> {
    for attempt in 1..=MAX_COLD_FORCE_ATTEMPTS {
        let nonce = uuid::Uuid::new_v4().to_string();
        aws.force_cold(cell, &nonce)
            .await
            .with_context(|| format!("forcing cold for {}", cell.function_name()))?;

        let res = aws.invoke_tail(cell, None).await?;
        check_platform_ok(cell, &res)?;
        let report = parse_report(&res.log_tail)
            .with_context(|| format!("parsing REPORT for {}", cell.function_name()))?;

        if report.is_cold() {
            let row = build_row(cell, run_id, sizes, cycle, 0, true, &nonce, &res, &report);
            return Ok((nonce, row));
        }

        // Warm despite the bump: the control plane confirmed the new revision, but
        // the invoke router still hit a pre-existing warm sandbox (data-plane lag).
        // Re-force rather than record warm as cold; log the RequestId so the
        // serving sandbox can be cross-referenced in CloudWatch.
        eprintln!(
            "[{}]   cold-force retry {attempt}/{MAX_COLD_FORCE_ATTEMPTS} for {} (cycle {cycle}): invoke landed warm (req {}), re-forcing",
            utc_hms(),
            cell.function_name(),
            report.request_id,
        );
        // Escalating backoff: BASE * attempt, capped. Spreads the later retries
        // across the data-plane propagation window instead of bunching them.
        let backoff = (COLD_FORCE_RETRY_BACKOFF_BASE * attempt).min(COLD_FORCE_RETRY_BACKOFF_CAP);
        tokio::time::sleep(backoff).await;
    }

    bail!(
        "{} (cycle {cycle}): failed to force a cold start in {MAX_COLD_FORCE_ATTEMPTS} attempts (invoke kept landing warm)",
        cell.function_name()
    )
}

/// Fails loud on platform-level invoke problems (non-200 status, FunctionError),
/// and for the HTTP-fronted Smithy scenarios also on the response envelope's
/// `statusCode` (see `check_http_envelope_ok`): their framework can serialize an
/// error as a 500 inside the body while the Lambda invoke returns 200, which would
/// otherwise be recorded as a clean invoke. The Rust handler panics instead, so it
/// already surfaces as a `FunctionError`; this brings the Node/Java handlers to the
/// same fail-loud bar.
fn check_platform_ok(cell: &Cell, res: &crate::aws::lambda::InvokeResult) -> Result<()> {
    if res.status_code != 200 {
        bail!(
            "{} returned status {} (expected 200); log:\n{}",
            cell.function_name(),
            res.status_code,
            res.log_tail
        );
    }
    if let Some(err) = &res.function_error {
        bail!(
            "{} reported FunctionError={err}; log:\n{}",
            cell.function_name(),
            res.log_tail
        );
    }
    if cell.scenario.is_http_fronted() {
        check_http_envelope_ok(&cell.function_name(), res)?;
    }
    Ok(())
}

/// Validates the HTTP-envelope `statusCode` of a Smithy-fronted invoke's response
/// (`{ "statusCode": N, ... }`, the API Gateway proxy shape), failing loud on a
/// non-2xx or a missing/unparseable payload. A 5xx there is a server-side failure
/// the outer invoke result does not reflect. Takes a bare `name` so the probe's
/// name-based path can share it.
pub(crate) fn check_http_envelope_ok(
    name: &str,
    res: &crate::aws::lambda::InvokeResult,
) -> Result<()> {
    let payload = res.payload.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "{name} (HTTP-fronted) returned no response payload to validate; log:\n{}",
            res.log_tail
        )
    })?;
    let envelope: serde_json::Value = serde_json::from_str(payload)
        .with_context(|| format!("{name} (HTTP-fronted) response is not JSON: {payload}"))?;
    let status = envelope
        .get("statusCode")
        .and_then(|s| s.as_i64())
        .ok_or_else(|| {
            anyhow::anyhow!("{name} (HTTP-fronted) response has no integer statusCode: {payload}")
        })?;
    if !(200..300).contains(&status) {
        bail!(
            "{name} (HTTP-fronted) handler returned HTTP {status} (expected 2xx): the server \
             framework serialized an error into the response body, which the outer invoke \
             does not reflect; body:\n{payload}\nlog:\n{}",
            res.log_tail
        );
    }
    Ok(())
}

/// Builds one results row from a validated invocation. Pure: it does not write,
/// so callers can buffer rows and flush a whole cycle only once its invariants
/// hold.
#[allow(clippy::too_many_arguments)]
fn build_row(
    cell: &Cell,
    run_id: &str,
    sizes: crate::deploy::ArtifactSizes,
    cycle: u32,
    idx_in_cycle: u32,
    is_cold: bool,
    nonce: &str,
    res: &crate::aws::lambda::InvokeResult,
    report: &crate::parse::Report,
) -> InvocationRow {
    let ts_unix_ms = crate::config::now_unix_ms();

    InvocationRow {
        run_id: run_id.to_string(),
        ts_unix_ms,
        lang: cell.lang.as_str().to_string(),
        scenario: cell.scenario.as_str().to_string(),
        memory_mb: cell.memory_mb,
        arch: cell.arch.as_str().to_string(),
        opt: cell.opt.map(|o| o.as_str().to_string()),
        snapstart: cell.snapstart,
        jitter: cell.jitter.map(|j| j.as_str().to_string()),
        runtime_id: cell.lang.runtime().to_string(),
        function_name: cell.function_name(),
        artifact_zip_bytes: sizes.zip,
        artifact_unzipped_bytes: sizes.unzipped,
        cycle,
        idx_in_cycle,
        is_cold,
        cold_nonce: nonce.to_string(),
        request_id: report.request_id.clone(),
        invoke_status: res.status_code,
        function_error: res.function_error.clone(),
        init_ms: report.init_ms,
        restore_ms: report.restore_ms,
        duration_ms: report.duration_ms,
        billed_ms: report.billed_ms,
        memory_size_mb: report.memory_size_mb,
        max_memory_used_mb: report.max_memory_used_mb,
    }
}
