//! lambdabench driver CLI.
//!
//! Subcommands:
//!   doctor   - read-only environment + AWS identity check
//!   build    - build the unique deployable artifacts into dist/
//!   run      - build + deploy + execute the cold/warm benchmark, writing
//!              results/run-<id>.* (use --skip-build / --skip-deploy to iterate)
//!   probe    - documentation probes for the Cold Start Anatomy page, NOT part of
//!              the matrix. Two explicit modes:
//!                probe download-start    - pre-Init download+start residual vs
//!                                          already-deployed matrix functions
//!                probe download-scaling  - synthetic padded-size sweep (add
//!                                          --with-image for the container family)
//!   teardown - delete all benchmark resources

mod aws;
mod build;
mod config;
mod deploy;
mod parse;
mod probe;
mod record;
mod run;
mod teardown;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::{Arch, Cell, Lang, Scenario, all_cells};
use record::{IterationBucket, ResultsWriter, RunMeta, RunStatus};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "bencher", about = "Lambda scenario cold/warm benchmark")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Read-only environment + AWS identity check.
    Doctor,
    /// Build the unique deployable artifacts into dist/.
    Build {
        /// Restrict to whole languages, comma-separated (e.g. `--lang python`).
        /// Only those languages are built. Omit to build every language.
        #[arg(long, value_delimiter = ',', num_args = 1..)]
        lang: Vec<String>,
    },
    /// Run the cold/warm benchmark.
    Run(RunArgs),
    /// Documentation probes for the Cold Start Anatomy page (not part of the
    /// matrix). Has two modes: `download-start` and `download-scaling`.
    Probe(probe::ProbeArgs),
    /// Delete all benchmark resources.
    Teardown {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Parser)]
struct RunArgs {
    /// Iteration-count profile. `full` (default) is the published methodology:
    /// per-scenario cold/warm counts (light scenarios at 50x50, CPU probes at
    /// their long-warm counts), with the jitter-A/B bypass and SnapStart
    /// cold-cycle clamp applied. `smoke` runs a tiny flat count over the whole
    /// matrix for a quick pipeline check (not statistically meaningful). Per-cell
    /// counts derive from the profile alone (see `Cell::iterations`), so there are
    /// no separate count flags to keep in sync.
    #[arg(long, value_enum, default_value_t = config::Profile::default())]
    profile: config::Profile,
    /// Functions benchmarked concurrently (serial within each function).
    #[arg(long, default_value_t = config::DEFAULT_POOL)]
    pool: usize,
    /// Restrict to a subset, comma-separated `lang:scenario` (e.g.
    /// `rust:hello,node:hello`). Omit to run the full matrix.
    #[arg(long)]
    only: Option<String>,
    /// Restrict to whole languages, comma-separated (e.g. `--lang rust,node`).
    /// Unlike `--only`, this scopes build AND deploy AND run, so other languages
    /// are never built or deployed. Omit to include every language.
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    lang: Vec<String>,
    /// Restrict to specific memory sizes (MB). Repeatable or comma-separated
    /// (e.g. `--memory 128 --memory 2048` or `--memory 128,2048`). Omit to sweep all.
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    memory: Vec<i32>,
    /// Restrict to a single architecture (arm64 or x86_64).
    #[arg(long)]
    arch: Option<String>,
    /// Skip the build step and reuse the artifacts already in `dist/`. Fails
    /// loud if a required artifact is missing. Use when nothing has changed
    /// since the last build (avoids re-running cargo / esbuild).
    #[arg(long)]
    skip_build: bool,
    /// Skip the deploy step and invoke the functions as already deployed. Fails
    /// loud if a selected cell's function does not exist. Use when the functions
    /// are already deployed and unchanged (avoids the full re-deploy pass).
    #[arg(long)]
    skip_deploy: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let repo_root = repo_root()?;

    match cli.command {
        Cmd::Doctor => doctor(&repo_root).await,
        Cmd::Build { lang } => {
            let langs = parse_langs(&lang)?;
            build_artifacts(&repo_root, &langs)?;
            Ok(())
        }
        Cmd::Run(args) => cmd_run(&repo_root, args).await,
        Cmd::Probe(args) => probe::run(&repo_root, args).await,
        Cmd::Teardown { yes } => cmd_teardown(yes).await,
    }
}

/// Resolves the repository root (the directory containing this workspace).
///
/// The path is baked in at COMPILE time (`CARGO_MANIFEST_DIR`), so the binary is
/// tied to the checkout it was built from: correct for the intended
/// `cargo run -p bencher` usage, but a relocated or copied binary would resolve
/// scenario sources, dist/ and results/ into the original build checkout.
fn repo_root() -> Result<PathBuf> {
    // CARGO_MANIFEST_DIR is bencher/; parent is the repo root.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest
        .parent()
        .context("resolving repo root")?
        .to_path_buf())
}

/// Read-only checks: tools on PATH, AWS identity, region.
async fn doctor(repo_root: &Path) -> Result<()> {
    use std::process::Command;
    println!("== lambdabench doctor ==");
    println!("repo root: {}", repo_root.display());
    println!("region:    {}", config::REGION);

    let tools = [
        ("cargo", "cargo", vec!["--version"]),
        ("cargo-lambda", "cargo", vec!["lambda", "--version"]),
        ("node", "node", vec!["--version"]),
        ("npm", "npm", vec!["--version"]),
        // Java builds via the project's gradle wrapper (./gradlew), so only the
        // JDK is a host prerequisite, not a system gradle. Smithy codegen also
        // runs through gradle's smithy-jar plugin, so no standalone Smithy CLI.
        ("java", "java", vec!["--version"]),
        // Python artifacts are assembled with `pip install --target` (host
        // python3/pip drives the cross-platform wheel download); the deployed
        // runtime is Lambda's managed python3.14, not the host interpreter.
        ("python", "python3", vec!["--version"]),
        ("pip", "python3", vec!["-m", "pip", "--version"]),
        // Go cross-compiles to a native `bootstrap` binary for provided.al2023
        // (GOOS=linux, the cell's GOARCH); only the Go toolchain is needed.
        ("go", "go", vec!["version"]),
        // crane assembles + pushes the zip-vs-image probe family's padded images
        // (`bencher probe download-scaling --with-image`). Daemonless, so it is the
        // only container tool the runner needs; the matrix build/run never uses it.
        ("crane", "crane", vec!["version"]),
    ];
    let mut missing = Vec::new();
    for (label, tool, args) in tools {
        match Command::new(tool).args(&args).output() {
            Ok(out) if out.status.success() => {
                let v = String::from_utf8_lossy(if out.stdout.is_empty() {
                    &out.stderr
                } else {
                    &out.stdout
                });
                println!("  [ok] {label}: {}", v.lines().next().unwrap_or("").trim());
            }
            _ => {
                println!("  [MISSING] {label}");
                missing.push(label);
            }
        }
    }

    match aws::Aws::load().await {
        Ok(aws) => println!("  [ok] AWS identity: account {}", aws.account_id),
        Err(e) => {
            println!("  [MISSING] AWS identity: {e:#}");
            missing.push("aws-credentials");
        }
    }

    let cells = all_cells();
    println!(
        "matrix: {} functions, {} unique artifacts",
        cells.len(),
        config::unique_artifacts(&cells).len()
    );

    if missing.is_empty() {
        println!("doctor: all good.");
        Ok(())
    } else {
        bail!("doctor: missing prerequisites: {missing:?}");
    }
}

/// Builds the unique artifacts for the full matrix.
/// Parses the repeated/comma-separated `--lang` values into `Lang` variants. An
/// empty input yields an empty vec, meaning "all languages" downstream.
fn parse_langs(values: &[String]) -> Result<Vec<Lang>> {
    values
        .iter()
        .map(|s| Lang::parse(s).map_err(anyhow::Error::msg))
        .collect()
}

/// Builds the unique artifacts for whole languages (an empty slice = all). Used
/// by the `build` subcommand, which operates on a language's full matrix.
fn build_artifacts(
    repo_root: &Path,
    langs: &[Lang],
) -> Result<(
    std::collections::BTreeMap<String, build::Artifact>,
    serde_json::Value,
)> {
    let cells = config::cells_for_langs(langs);
    let keys = config::unique_artifacts(&cells);
    build_artifacts_for_keys(repo_root, &keys)
}

/// Builds exactly the given unique artifact keys. Used by `run`, which scopes
/// the build to only the artifacts its selected cells need (so a targeted run
/// does not build a whole language's matrix).
fn build_artifacts_for_keys(
    repo_root: &Path,
    keys: &[config::ArtifactKey],
) -> Result<(
    std::collections::BTreeMap<String, build::Artifact>,
    serde_json::Value,
)> {
    println!("Building {} unique artifacts...", keys.len());
    let (artifacts, manifest) = build::build_all(repo_root, keys)?;
    for (label, art) in &artifacts {
        println!(
            "  {label}: {} (zip {} B, unzipped {} B)",
            art.zip_path.display(),
            art.zip_size_bytes,
            art.unzipped_size_bytes
        );
    }
    Ok((artifacts, manifest))
}

async fn cmd_run(repo_root: &Path, args: RunArgs) -> Result<()> {
    // The run pipeline (build + deploy + run) is scoped to exactly the cells the
    // run will invoke. `--lang` plus the refining `--only` / `--memory` / `--arch`
    // filters narrow that set, so a targeted run (e.g. one scenario at one memory)
    // only builds and deploys what it exercises. With no filters this is the full
    // matrix.
    let langs = parse_langs(&args.lang)?;
    let cells = select_cells(&args, &langs)?;
    if cells.is_empty() {
        bail!("no cells selected (check --lang/--only/--memory/--arch filters)");
    }

    // Only the artifacts those selected cells need (deduped): a single-scenario
    // run builds/deploys a handful of functions, not hundreds.
    let keys = config::unique_artifacts(&cells);

    // Build (or reuse existing dist/ with --skip-build).
    let (artifacts, manifest) = if args.skip_build {
        println!(
            "Skipping build; reusing {} artifacts from dist/",
            keys.len()
        );
        build::load_artifacts_from_dist(repo_root, &keys)?
    } else {
        build_artifacts_for_keys(repo_root, &keys)?
    };

    let aws = aws::Aws::load().await?;

    // Deploy (idempotent) so the run reflects the latest artifacts, unless
    // --skip-deploy, in which case the functions must already exist (the run's
    // invokes fail loud if one is missing). The size map for the results metadata
    // is then derived directly from the artifacts.
    let sizes = if args.skip_deploy {
        println!("Skipping deploy; invoking already-deployed functions.");
        // Fail loud NOW if any selected cell's function is missing, rather than
        // deferring to the first invoke: a missing function is non-transient, and
        // the run loop's cell-retry would otherwise burn its full 3x budget (with
        // misleading "cell retry" warnings) before aborting. Check each function by
        // exact name (GetFunction), never an account-wide sweep.
        let mut missing = missing_functions(&aws, &cells).await?;
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            bail!(
                "--skip-deploy: {} selected function(s) are not deployed: {}. \
                 Deploy them (drop --skip-deploy) or adjust the run filters.",
                missing.len(),
                missing.join(", ")
            );
        }
        deploy::sizes_from_artifacts(&artifacts, &cells)?
    } else {
        deploy::deploy(&aws, &artifacts, &cells).await?
    };

    let run_id = config::run_id();
    let results_dir = repo_root.join("results");
    std::fs::create_dir_all(&results_dir)
        .with_context(|| format!("creating results dir {}", results_dir.display()))?;
    let jsonl = results_dir.join(format!("run-{run_id}.jsonl.gz"));
    let meta_path = results_dir.join(format!("run-{run_id}.meta.json"));

    let started_at_unix_ms = config::now_unix_ms();
    let params = run::RunParams {
        profile: args.profile,
        pool: args.pool,
    };
    // Resolve each cell's count via the same function the run loop uses
    // (`Cell::iterations`), so the planned count and recorded sampling cannot
    // disagree with what runs. Tally cells by their resolved (cold, warm) into
    // buckets, which double as the meta's sampling record. `planned` is the sum of
    // the bucket invocation tallies.
    let mut bucket_map: std::collections::BTreeMap<(u32, u32), (usize, u64)> =
        std::collections::BTreeMap::new();
    for c in &cells {
        let (cold, warm) = c.iterations(params.profile);
        let entry = bucket_map.entry((cold, warm)).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += cold as u64 * (1 + warm as u64);
    }
    let iteration_buckets: Vec<IterationBucket> = bucket_map
        .into_iter()
        .map(|((cold, warm), (functions, invocations))| IterationBucket {
            cold_cycles: cold,
            warm_per_cycle: warm,
            functions,
            invocations,
        })
        .collect();
    let planned: u64 = iteration_buckets.iter().map(|b| b.invocations).sum();
    let mut meta = RunMeta {
        run_id: run_id.clone(),
        started_at_unix_ms,
        finished_at_unix_ms: None,
        status: RunStatus::Running,
        region: config::REGION.to_string(),
        account_id: aws.account_id.clone(),
        profile: args.profile,
        iteration_buckets: iteration_buckets.clone(),
        pool: args.pool,
        memory_mb: config::MEMORY_MB.to_vec(),
        total_functions: cells.len(),
        total_invocations_planned: planned,
        total_invocations_recorded: 0,
        build_manifest: manifest,
    };
    meta.write(&meta_path)?;

    let buckets_desc = iteration_buckets
        .iter()
        .map(|b| {
            format!(
                "{}x(1+{}) [{} fn]",
                b.cold_cycles, b.warm_per_cycle, b.functions
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "Run {run_id} [{:?} profile]: {} functions, {} invocations planned; per-cell counts: {}",
        args.profile,
        cells.len(),
        planned,
        buckets_desc
    );

    let writer = Arc::new(ResultsWriter::create(&jsonl)?);

    let outcome = run::run(&aws, &cells, params, &run_id, &sizes, writer.clone()).await;

    // Record what landed and the terminal status, so a reader can tell a complete
    // dataset from one truncated by an aborted run. The finish time and
    // recorded-row count are stamped for both outcomes; `status` distinguishes
    // them, so a failed run is never mislabeled as complete.
    meta.finished_at_unix_ms = Some(config::now_unix_ms());
    meta.total_invocations_recorded = writer.rows_written();
    meta.status = if outcome.is_ok() {
        RunStatus::Ok
    } else {
        RunStatus::Failed
    };
    meta.write(&meta_path)?;

    outcome?;
    println!(
        "Results: {} ({} invocations recorded)",
        jsonl.display(),
        meta.total_invocations_recorded
    );
    println!("Meta:    {}", meta_path.display());
    Ok(())
}

/// Returns the names of the given cells' functions that are NOT deployed,
/// checking each by exact name (GetFunction) concurrently. A non-NotFound error
/// (throttle / access-denied) aborts rather than being misread as "missing". Used
/// by the `--skip-deploy` and probe preflights, so neither depends on an
/// account-wide `ListFunctions` sweep.
async fn missing_functions(aws: &aws::Aws, cells: &[Cell]) -> Result<Vec<String>> {
    use futures::stream::{self, StreamExt, TryStreamExt};
    const CHECK_CONCURRENCY: usize = 8;
    let missing: Vec<String> = stream::iter(cells.iter().map(|c| {
        let aws = aws.clone();
        let name = c.function_name();
        async move { aws.function_exists(&name).await.map(|ok| (name, ok)) }
    }))
    .buffer_unordered(CHECK_CONCURRENCY)
    .try_filter_map(|(name, exists)| async move { Ok((!exists).then_some(name)) })
    .try_collect()
    .await?;
    Ok(missing)
}

async fn cmd_teardown(yes: bool) -> Result<()> {
    let aws = aws::Aws::load().await?;
    println!(
        "About to delete ALL lambdabench resources in account {} ({}):",
        aws.account_id,
        config::REGION
    );
    println!(
        "  - {} Lambda functions (matrix + synthetic probe, {}-*)",
        config::all_managed_function_names().len(),
        config::PREFIX
    );
    println!("  - IAM role {}", config::ROLE_NAME);
    println!("  - DynamoDB table {}", config::TABLE_NAME);
    println!("  - S3 bucket {} + object", aws.bucket_name());
    println!(
        "  - KMS key {} (scheduled, 7-day window)",
        config::KMS_ALIAS
    );

    if !yes {
        use std::io::Write;
        print!("Type 'yes' to confirm: ");
        std::io::stdout().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        if input.trim() != "yes" {
            bail!("aborted (no confirmation)");
        }
    }

    teardown::teardown(&aws).await
}

/// Applies the run subset filters to the full matrix.
fn select_cells(args: &RunArgs, langs: &[Lang]) -> Result<Vec<Cell>> {
    let only: Option<Vec<(Lang, Scenario)>> = match &args.only {
        None => None,
        Some(s) => {
            let mut v = Vec::new();
            for part in s.split(',') {
                let (l, sc) = part
                    .split_once(':')
                    .with_context(|| format!("--only entry '{part}' must be lang:scenario"))?;
                // Reuse the canonical parsers (keyed off `as_str`) so a newly
                // added language or scenario is accepted automatically, not
                // silently rejected by a stale local match.
                let lang =
                    Lang::parse(l).map_err(|e| anyhow::anyhow!("{e} in --only entry '{part}'"))?;
                let scenario = Scenario::parse(sc)
                    .map_err(|e| anyhow::anyhow!("{e} in --only entry '{part}'"))?;
                // Cross-validate against the matrix so a typo or impossible pair
                // fails loud here, rather than surfacing later as the generic
                // "no cells selected".
                if !lang.supports(scenario) {
                    bail!(
                        "--only entry '{part}' selects {}:{}, which is not part of the matrix \
                         ({} has no {} scenario)",
                        lang.as_str(),
                        scenario.as_str(),
                        lang.as_str(),
                        scenario.as_str()
                    );
                }
                if !langs.is_empty() && !langs.contains(&lang) {
                    bail!(
                        "--only entry '{part}' names lang '{}', which is excluded by \
                         --lang {:?}; the two filters select nothing in common",
                        lang.as_str(),
                        langs.iter().map(|l| l.as_str()).collect::<Vec<_>>()
                    );
                }
                v.push((lang, scenario));
            }
            Some(v)
        }
    };

    // Validate --arch up front so a typo (e.g. `x86`, `amd64`) fails loud with the
    // valid names rather than matching no cell, matching --only/--lang above.
    let arch = match &args.arch {
        None => None,
        Some(a) => Some(Arch::parse(a).map_err(anyhow::Error::msg)?),
    };
    // Likewise validate each --memory value against the swept tiers, so a typo
    // (e.g. `1000` for `1024`) fails loud rather than selecting zero cells.
    config::validate_memory_tiers(&args.memory).map_err(anyhow::Error::msg)?;

    let cells = config::cells_for_langs(langs)
        .into_iter()
        .filter(|c| match &only {
            Some(list) => list.iter().any(|(l, s)| *l == c.lang && *s == c.scenario),
            None => true,
        })
        .filter(|c| args.memory.is_empty() || args.memory.contains(&c.memory_mb))
        .filter(|c| arch.map(|a| a == c.arch).unwrap_or(true))
        .collect();
    Ok(cells)
}
