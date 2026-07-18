//! `probe download-start`: measure the pre-Init download+start residual against
//! ALREADY-DEPLOYED matrix functions (deploys nothing). Resolves the matrix
//! targets across three axes (memory, artifact size, language), samples each with
//! the shared measurement core, and writes the run-scoped download+start table
//! JSON the site's data loader discovers at build time.

use super::ProbeCommon;
use super::sample::{Sample, aggregate, sample_cold_series};
use crate::aws::Aws;
use crate::config::{self, Arch, Cell, Lang, Scenario};
use anyhow::{Context, Result, bail};
use clap::Parser;
use std::path::PathBuf;

/// `probe download-start`: measure the residual against deployed matrix functions.
#[derive(Parser)]
pub(super) struct DownloadStartArgs {
    #[command(flatten)]
    common: ProbeCommon,
    /// Restrict the target cells to a subset, comma-separated `lang:scenario`
    /// (e.g. `rust:hello,java:hello,java:smithyfull`). Omit for the default
    /// selection (the size axis + the memory axis; see `default_targets`).
    #[arg(long)]
    only: Option<String>,
    /// Restrict to specific memory sizes (MB). Repeatable or comma-separated.
    /// Omit to use the per-axis defaults.
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    memory: Vec<i32>,
    /// Output path override. Omit to write a run-scoped, timestamped file under
    /// the repo-root `results/` dir (`results/lifecycle-download-start-<run_id>.json`),
    /// gitignored like the matrix runs and discovered by the site at build time.
    #[arg(long)]
    out: Option<PathBuf>,
}

/// Per-cell aggregated probe result, serialized into the run-scoped `results/` JSON.
///
/// `lang`/`scenario`/`arch` are the canonical `as_str` ids (e.g. `smithyfull`, not
/// the enum's snake_case serde form `smithy_full`), matching how the main matrix
/// records them (`run.rs`) and the ids the site's label maps key on (`format.js`).
#[derive(serde::Serialize)]
struct CellResult {
    function_name: String,
    lang: String,
    scenario: String,
    arch: String,
    memory_mb: i32,
    zip_bytes: u64,
    unzipped_bytes: u64,
    n_samples: u32,
    w_cold_p50: f64,
    w_cold_min: f64,
    w_cold_max: f64,
    init_p50: f64,
    cold_duration_p50: f64,
    warm_rtt_p50: f64,
    w_warm_p50: f64,
    w_warm_min: f64,
    w_warm_max: f64,
    residual_p50: f64,
    residual_min: f64,
    residual_max: f64,
}

/// Top-level probe output written to the run-scoped `results/` JSON.
#[derive(serde::Serialize)]
struct ProbeOutput {
    generated_at_unix_ms: u128,
    region: String,
    note: String,
    n_warm_per_sample: u32,
    cells: Vec<CellResult>,
}

/// The default target selection when `--only` / `--memory` are not given. Three
/// axes with heavy reuse between them (see the inline groups). Returned as
/// `(lang, scenario, memory)` triples resolved against `all_cells()` so a matrix
/// change can never leave a stale hand-written list.
fn default_targets() -> Vec<(Lang, Scenario, i32)> {
    // `dedup_targets` below collapses the overlap so shared cells are probed once.
    // The residual measures download + environment start, which depends on artifact
    // size, memory tier, and runtime family, NOT scenario logic (that lands in
    // init/duration, which the per-sample subtraction cancels), so the set spans
    // those three axes rather than the whole matrix.
    // --- Axis 1: vCPU / memory (same function, vary the tier). Provisioning is
    // expected to be flat here (steps 1-2 run before the configured CPU applies).
    // rust/hello is floor-free at every tier.
    let mut t: Vec<(Lang, Scenario, i32)> = vec![
        (Lang::Rust, Scenario::Hello, 128),
        (Lang::Rust, Scenario::Hello, 512),
        (Lang::Rust, Scenario::Hello, 3008),
    ];

    // --- Axis 2: download SIZE (@512 MB, arm64), ordered small -> large, crossing
    // both runtime families so a flat small-artifact floor says provisioning is
    // family-independent and any rise is the download term:
    //   node/hello       ~1 KB    (managed nodejs, esbuild tree-shakes the SDK)
    //   python/hello     ~1 KB    (managed python, stdlib only)
    //   java/hello       ~0.02 MB (managed java, bare handler jar)
    //   rust/hello       ~0.5 MB  (custom provided.al2023 bootstrap; from axis 1)
    //   python/authz     ~5.5 MB  (managed python + native crypto wheel)
    //   rust/smithyfull  ~mid     (custom runtime, larger binary: the only big-ish
    //                             artifact on provided.al2023)
    //   java/smithyfull  ~14.5 MB (managed java, Smithy SDK jar)
    //   python/oneclient ~17 MB   (managed python + bundled boto3; large + a fast
    //                             init/duration, so the least clock-skew-sensitive
    //                             large point)
    t.push((Lang::Node, Scenario::Hello, 512));
    t.push((Lang::Python, Scenario::Hello, 512));
    t.push((Lang::Java, Scenario::Hello, 512));
    t.push((Lang::Python, Scenario::Authz, 512));
    t.push((Lang::Rust, Scenario::SmithyFull, 512));
    t.push((Lang::Java, Scenario::SmithyFull, 512));
    t.push((Lang::Python, Scenario::OneClient, 512));

    // --- Axis 3: LANGUAGES on a realistic SDK-heavy handler, at a low and a high
    // tier, so the per-language floor and any vCPU effect are both visible. Uses
    // each language's most real-world scenario it can host: smithyfull for the
    // runtimes that host it (rust/java/node), oneclient for those that cannot
    // (python/go skip the Smithy scenarios; see Lang::supports). These are also the
    // fattest artifact per language, so this doubles as the "big in every language"
    // set. rust/java smithyfull@512 and python oneclient@512 overlap axis 2.
    for &mem in &[512, 3008] {
        t.push((Lang::Rust, Scenario::SmithyFull, mem));
        t.push((Lang::Java, Scenario::SmithyFull, mem));
        t.push((Lang::Node, Scenario::SmithyFull, mem));
        t.push((Lang::Python, Scenario::OneClient, mem));
        t.push((Lang::Go, Scenario::OneClient, mem));
    }

    dedup_targets(t)
}

/// Removes duplicate `(lang, scenario, memory)` triples, preserving first-seen
/// order, so a cell shared across the axes above is probed exactly once.
fn dedup_targets(targets: Vec<(Lang, Scenario, i32)>) -> Vec<(Lang, Scenario, i32)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (l, s, m) in targets {
        if seen.insert((l, s as usize, m)) {
            out.push((l, s, m));
        }
    }
    out
}

/// `probe download-start`: resolves the matrix targets, verifies they are deployed,
/// samples each, and writes the run-scoped download+start table JSON. Deploys
/// nothing (targets must already exist).
pub(super) async fn run_download_start(
    repo_root: &std::path::Path,
    args: DownloadStartArgs,
) -> Result<()> {
    let arch = Arch::parse(&args.common.arch).map_err(anyhow::Error::msg)?;

    // Resolve targets from the matrix (never a hand-built list), so a matrix
    // change can't produce a stale target; floor-invalid cells are dropped with a
    // note.
    let targets = resolve_targets(&args, arch)?;
    if targets.is_empty() {
        bail!("no target cells selected (check --only/--memory/--arch and scenario memory floors)");
    }

    // Artifact sizes come from dist/ without any AWS call, exactly as
    // `run --skip-deploy` does. Fail loud if dist/ is incomplete. The connection
    // prewarm invokes a non-existent function, so no primer cell needs sizing.
    let keys = config::unique_artifacts(&targets);
    let (artifacts, _manifest) = crate::build::load_artifacts_from_dist(repo_root, &keys)
        .context("reading artifact sizes from dist/ (run `bencher build` first)")?;
    let sizes = crate::deploy::sizes_from_artifacts(&artifacts, &targets)?;

    let aws = Aws::load().await?;
    println!(
        "== lambdabench probe download-start == account {} region {}",
        aws.account_id,
        config::REGION
    );
    println!(
        "targets: {} cell(s), {} cold sample(s) x (1 prewarm + 1 cold + {} warm) each",
        targets.len(),
        args.common.cold_samples,
        args.common.warm_per_sample
    );

    // Preflight: every target must already be deployed. Checks each by exact name
    // (GetFunction), same as `run --skip-deploy`. The prewarm target is
    // intentionally non-existent, so it is not in this check.
    let mut missing = crate::missing_functions(&aws, &targets).await?;
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        bail!(
            "{} probe function(s) are not deployed: {}. Deploy the matrix first \
             (`bencher run` or `bencher run --skip-build`).",
            missing.len(),
            missing.join(", ")
        );
    }

    // Retry-DISABLED client for the timed invokes: a throttle must fail loud, not
    // inflate the wall-clock (DESIGN.md Measurement purity #1). The bundle's
    // adaptive-retry client is used for the control-plane cold-force mechanism.
    let timed = aws.retryless_lambda_client();

    let mut results = Vec::new();
    for cell in &targets {
        let name = cell.function_name();
        println!("\n-- {name}");
        // Buffer-then-commit per cell: take all N samples into a fresh Vec, return
        // them only if every sample succeeds, so a transient failure re-runs the
        // whole cell (via retry_transient) rather than leave a partially-sampled
        // cell in the aggregate. Mirrors run_cell's discipline.
        let samples = sample_cold_series(
            &aws,
            &timed,
            &name,
            &crate::aws::lambda::invoke_payload(cell),
            cell.scenario.is_http_fronted(),
            args.common.cold_samples,
            args.common.warm_per_sample,
        )
        .await?;

        let sz = sizes
            .get(&name)
            .expect("size resolved for every probe cell above");
        results.push(aggregate_cell(cell, sz.zip, sz.unzipped, &samples));
    }

    let out = ProbeOutput {
        generated_at_unix_ms: config::now_unix_ms(),
        region: config::REGION.to_string(),
        note: "illustrative, environment-dependent magnitudes (single client, single account; \
               not matrix data). residual = W_cold - init - cold_duration - warm_rtt \
               (mostly download + environment start, plus a small scheduling/placement remainder \
               the warm-RTT subtraction does not cancel; all of it invisible to the REPORT line). \
               w_cold / w_warm are the FULL caller wall-clocks of a cold / warm invoke from this \
               client (pooled HTTPS connection), so they include the client->region network path."
            .to_string(),
        n_warm_per_sample: args.common.warm_per_sample,
        cells: results,
    };

    let out_path = super::probe_out_path(&args.out, "download-start", &config::run_id());
    super::write_json(repo_root, &out_path, &out)?;
    Ok(())
}

/// Resolves the target cells from the matrix, applying `--only` / `--memory` (or
/// the per-axis defaults) and dropping any cell below its scenario memory floor
/// with a printed note. Only non-SnapStart cells are eligible (the probe uses the
/// env-bump cold-force, not the SnapStart publish path).
fn resolve_targets(args: &DownloadStartArgs, arch: Arch) -> Result<Vec<Cell>> {
    // Validate --memory values against the swept tiers up front (like select_cells).
    config::validate_memory_tiers(&args.memory).map_err(anyhow::Error::msg)?;

    // Build the desired (lang, scenario, memory) set.
    let wanted: Vec<(Lang, Scenario, i32)> = match &args.only {
        None => {
            if args.memory.is_empty() {
                default_targets()
            } else {
                // With --memory but no --only, apply the memories to the default
                // (lang, scenario) pairs (deduped, preserving first-seen order).
                let mut pairs: Vec<(Lang, Scenario)> = Vec::new();
                for (l, s, _) in default_targets() {
                    if !pairs.contains(&(l, s)) {
                        pairs.push((l, s));
                    }
                }
                pairs
                    .into_iter()
                    .flat_map(|(l, s)| args.memory.iter().map(move |m| (l, s, *m)))
                    .collect()
            }
        }
        Some(only) => {
            let mems: Vec<i32> = if args.memory.is_empty() {
                vec![512]
            } else {
                args.memory.clone()
            };
            let mut v = Vec::new();
            for part in only.split(',') {
                let (l, sc) = part
                    .split_once(':')
                    .with_context(|| format!("--only entry '{part}' must be lang:scenario"))?;
                let lang = Lang::parse(l).map_err(|e| anyhow::anyhow!("{e} in '{part}'"))?;
                let scenario =
                    Scenario::parse(sc).map_err(|e| anyhow::anyhow!("{e} in '{part}'"))?;
                if !lang.supports(scenario) {
                    bail!(
                        "--only entry '{part}' selects {}:{}, not part of the matrix",
                        lang.as_str(),
                        scenario.as_str()
                    );
                }
                for m in &mems {
                    v.push((lang, scenario, *m));
                }
            }
            v
        }
    };

    // Resolve each triple to the real matrix Cell (plain, non-SnapStart), honoring
    // scenario memory floors.
    let all = config::all_cells();
    let mut out = Vec::new();
    for (lang, scenario, memory_mb) in wanted {
        let floor = scenario.min_memory_mb(lang, false);
        if memory_mb < floor {
            println!(
                "  skip {}:{} @ {memory_mb} MB (below floor {floor} MB)",
                lang.as_str(),
                scenario.as_str()
            );
            continue;
        }
        // Match the plain cell: for Rust the o3+jitter-off headline build, for
        // other langs the single plain cell.
        let found = all.iter().find(|c| {
            c.lang == lang
                && c.scenario == scenario
                && c.arch == arch
                && c.memory_mb == memory_mb
                && !c.snapstart
                && c.jitter != Some(config::Jitter::On)
                && (lang != Lang::Rust || c.opt == Some(config::Opt::O3))
        });
        match found {
            Some(c) => {
                if !out
                    .iter()
                    .any(|e: &Cell| e.function_name() == c.function_name())
                {
                    out.push(*c);
                }
            }
            None => bail!(
                "no matrix cell for {}:{} @ {memory_mb} MB {} (check --arch and the matrix)",
                lang.as_str(),
                scenario.as_str(),
                arch.as_str()
            ),
        }
    }
    Ok(out)
}

/// Aggregates a cell's samples into p50 of each term plus the residual spread via
/// the shared `aggregate` (same reduction as the synthetic and image families),
/// attaching the cell's identity + artifact sizes.
fn aggregate_cell(cell: &Cell, zip: u64, unzipped: u64, samples: &[Sample]) -> CellResult {
    let a = aggregate(samples);
    CellResult {
        function_name: cell.function_name(),
        lang: cell.lang.as_str().to_string(),
        scenario: cell.scenario.as_str().to_string(),
        arch: cell.arch.as_str().to_string(),
        memory_mb: cell.memory_mb,
        zip_bytes: zip,
        unzipped_bytes: unzipped,
        n_samples: a.n_samples,
        w_cold_p50: a.w_cold_p50,
        w_cold_min: a.w_cold_min,
        w_cold_max: a.w_cold_max,
        init_p50: a.init_p50,
        cold_duration_p50: a.cold_dur_p50,
        warm_rtt_p50: a.warm_rtt_p50,
        w_warm_p50: a.w_warm_p50,
        w_warm_min: a.w_warm_min,
        w_warm_max: a.w_warm_max,
        residual_p50: a.residual_p50,
        residual_min: a.residual_min,
        residual_max: a.residual_max,
    }
}
