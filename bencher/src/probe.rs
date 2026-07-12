//! `probe` subcommand: isolate the pre-Init "download + start" cold-start cost.
//!
//! AWS documents a cold start as four sub-phases: (1) download the code, (2)
//! start the execution environment (microVM), (3) run init code outside the
//! handler, (4) run the handler. The Lambda `REPORT` line only ever exposes
//! phases 3 (`Init Duration`) and 4 (the cold invoke's `Duration`); phases 1 and
//! 2 happen BEFORE the runtime signals readiness and appear in no function-side
//! signal or CloudWatch function metric. The only vantage point on them is the
//! caller's wall-clock around the `Invoke` API call.
//!
//! This probe isolates phases 1+2 by subtraction. With the SDK's HTTPS
//! connection to the Lambda data plane already warm, it times a freshly-cold
//! invoke (`W_cold`), then subtracts the in-Lambda cost the REPORT line does
//! report (`init_ms` + the cold `duration_ms`) and the warm network round-trip
//! (`warm_rtt` = a warm invoke's wall-clock minus its own REPORT `Duration`, so
//! only network + invoke-API overhead, not handler processing):
//!
//! ```text
//! residual = W_cold - init_ms - cold_duration_ms - warm_rtt
//! ```
//!
//! The residual is the download+start cost plus a small provisioning/network
//! remainder. A documentation-grade probe, deliberately outside the benchmark
//! matrix: it measures caller-side wall-clock, which the matrix excludes on
//! purpose (the region is pinned to minimize that quantity as noise), so it is not
//! bound by the fairness / measurement-purity invariants in DESIGN.md. Its numbers
//! are illustrative, environment-dependent magnitudes (single client, single
//! account; the absolute values depend on where the caller sits relative to the
//! region and the control-plane's state), and a roughly-constant residual swamped
//! by provisioning noise is a legitimate outcome, not a failure. The probe targets
//! ALREADY-DEPLOYED matrix functions and deploys nothing.
//!
//! Module layout:
//! - [`sample`]: the shared measurement core (one cold sample, its aggregation,
//!   the median) used by every mode.
//! - [`download_start`]: the `download-start` mode, residual vs deployed matrix
//!   functions (deploys nothing).
//! - [`synthetic`]: the `download-scaling` zip mode, ephemeral padded-size
//!   functions across two runtime families.
//! - [`image`]: the `download-scaling --with-image` container-image family (the
//!   one path that shells out to `crane`).

mod download_start;
mod image;
mod sample;
mod synthetic;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

/// Maximum times a probe re-runs a whole unit (a `download-start` cell, or a
/// synthetic/image size) after a transient hard error. Mirrors the matrix's
/// `MAX_CELL_ATTEMPTS` (`run.rs`): the inner cold-force retry (`sample.rs`)
/// absorbs the common "landed warm" case, so this outer retry only covers the
/// rarer one-off infrastructure hiccups that surface as a hard error (a throttle
/// on the retry-disabled timed client, a network blip, a REPORT parse miss).
/// Across a probe's many cold-forces at least one such event is likely; without
/// this a single one would abort the whole probe and block the publish. A
/// persistent failure still exhausts all attempts and fails loud.
pub(super) const MAX_UNIT_ATTEMPTS: u32 = 3;

/// Backoff before re-running a unit after a failed attempt, giving a transient
/// control-plane / platform condition time to clear. Matches the matrix's
/// `CELL_RETRY_BACKOFF`.
pub(super) const UNIT_RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// Runs `unit` up to `MAX_UNIT_ATTEMPTS` times, returning its first success. On a
/// hard error it logs, sleeps `UNIT_RETRY_BACKOFF`, and retries; after the last
/// attempt it returns the final error with context. `label` names the unit in the
/// retry log (e.g. the cell/function name).
///
/// The probe analog of the matrix's `run_cell` outer retry. The unit closure must
/// be self-contained per attempt (buffer-then-commit): a probe unit takes all N
/// cold samples into a fresh local `Vec` and returns them only if every sample
/// succeeded, so a retried unit never keeps a partially-sampled result. Discarding
/// a failed sample for a clean replacement does not violate measurement purity
/// (DESIGN.md forbids the SDK silently retrying WITHIN a single timed invoke, not
/// re-taking a whole failed sample).
pub(super) async fn retry_transient<T, F, Fut>(label: &str, mut unit: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=MAX_UNIT_ATTEMPTS {
        match unit().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                eprintln!("   unit retry {attempt}/{MAX_UNIT_ATTEMPTS} for {label}: {e:#}");
                last_err = Some(e);
                tokio::time::sleep(UNIT_RETRY_BACKOFF).await;
            }
        }
    }
    Err(last_err.expect("loop ran at least once"))
        .with_context(|| format!("{label} failed after {MAX_UNIT_ATTEMPTS} attempts"))
}

/// The `probe` subcommand. Two explicit modes, one per chart the Cold Start
/// Anatomy page draws; there is no default (a bare `probe` is a usage error), so
/// the operation is always spelled out at the call site.
#[derive(Parser)]
pub struct ProbeArgs {
    #[command(subcommand)]
    mode: ProbeMode,
}

#[derive(Subcommand)]
enum ProbeMode {
    /// Pre-Init download+start residual against ALREADY-DEPLOYED matrix functions
    /// (deploys nothing). Writes the download+start table JSON.
    DownloadStart(download_start::DownloadStartArgs),
    /// Synthetic padded-size sweep isolating the download term past the matrix's
    /// real-artifact ceiling: deploys ephemeral padded functions, measures the
    /// residual at each size, tears them down. `--with-image` adds the
    /// container-image family. Writes the download-scaling JSON(s).
    DownloadScaling(synthetic::DownloadScalingArgs),
}

/// Sampling knobs shared by both probe modes, flattened into each so every mode's
/// `--help` lists them.
#[derive(Parser)]
pub(super) struct ProbeCommon {
    /// Cold samples per cell. Each costs one control-plane cold-force (seconds),
    /// so this is kept small; the residual is read as the median across samples.
    /// Must be >= 1 (the median is taken over these samples).
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..))]
    pub(super) cold_samples: u32,
    /// Warm invokes taken per cold sample to establish the warm round-trip
    /// baseline (`warm_rtt` = their median) that is subtracted from `W_cold`.
    /// Must be >= 1 (the median is taken over these invokes).
    #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u32).range(1..))]
    pub(super) warm_per_sample: u32,
    /// Restrict to a single architecture (arm64 or x86_64). Defaults to arm64.
    #[arg(long, default_value = "arm64")]
    pub(super) arch: String,
}

/// Warms the shared data-plane HTTPS/TLS connection by issuing one throwaway
/// `Invoke` against a NON-EXISTENT function and swallowing the expected
/// `ResourceNotFoundException`. The transport connection is established before the
/// request is routed to any function, so a not-found response comes back over the
/// very connection a real invoke would reuse. Avoids needing a dedicated primer
/// function deployed (and a real primer doubling as a probe target). Any error
/// other than not-found (auth, throttle, network) is surfaced, since it would mean
/// the connection is not actually healthy.
pub(super) async fn prewarm_connection(client: &aws_sdk_lambda::Client) -> Result<()> {
    match client
        .invoke()
        .function_name(crate::config::PREWARM_NONEXISTENT_FN)
        .payload(aws_sdk_lambda::primitives::Blob::new(b"{}".to_vec()))
        .send()
        .await
    {
        Ok(_) => Ok(()), // Not expected (the function does not exist), but harmless.
        Err(err) => {
            let svc = err.into_service_error();
            if svc.is_resource_not_found_exception() {
                Ok(())
            } else {
                Err(anyhow::Error::new(svc).context("prewarming data-plane connection"))
            }
        }
    }
}

/// Resolves a probe's output path. An explicit `--out` (`Some`) is used verbatim
/// (`write_json` resolves a relative path against the repo root). Otherwise the
/// default is a run-scoped, timestamped file under the repo-root `results/` dir:
/// `results/lifecycle-<kind>-<run_id>.json`.
///
/// Mirrors the matrix's `results/run-<id>.jsonl.gz` convention on purpose: the
/// probe output is NOT committed (gitignored like the raw runs), the site
/// discovers the newest at build time (parsing the `<unix_ms>` in the run id, as
/// for a matrix run), and the same `results/` archive sweep preserves it. The
/// committed-static-file model it replaces let a stale (e.g. laptop-vantage) probe
/// run reach the published site as a fallback.
pub(super) fn probe_out_path(explicit: &Option<PathBuf>, kind: &str, run_id: &str) -> PathBuf {
    match explicit {
        Some(p) => p.clone(),
        None => PathBuf::from(format!("results/lifecycle-{kind}-{run_id}.json")),
    }
}

/// Serializes `value` to pretty JSON and writes it to `out`, resolving a relative
/// path against the repo root (not the process cwd) so the default lands in
/// `results/` regardless of where the driver was invoked from. An absolute
/// path is used as-is. Shared by the matrix and synthetic probe outputs.
pub(super) fn write_json<T: serde::Serialize>(
    repo_root: &std::path::Path,
    out: &PathBuf,
    value: &T,
) -> Result<()> {
    let out_path = if out.is_absolute() {
        out.clone()
    } else {
        repo_root.join(out)
    };
    let json = serde_json::to_string_pretty(value).context("serializing probe output")?;
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&out_path, json)
        .with_context(|| format!("writing probe output to {}", out_path.display()))?;
    println!("\nWrote {}", out_path.display());
    Ok(())
}

/// Dispatches the `probe` subcommand to its explicit mode (`download-start` /
/// `download-scaling`); a bare `probe` is a clap usage error.
pub async fn run(repo_root: &std::path::Path, args: ProbeArgs) -> Result<()> {
    match args.mode {
        ProbeMode::DownloadStart(a) => download_start::run_download_start(repo_root, a).await,
        ProbeMode::DownloadScaling(a) => synthetic::run_synthetic(repo_root, a).await,
    }
}
