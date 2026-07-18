//! `probe download-scaling`: synthetic padded-size sweep (zip family, and
//! optionally the container-image family in [`super::image`]).
//!
//! Deploys ephemeral padded-size functions (two runtime families, Python managed
//! and Rust custom-runtime), measures the download+start residual at each size,
//! and tears them down. This pushes the download term past the matrix's ~17 MB
//! real-artifact ceiling to chart where download starts to dominate. Padding with
//! inert bytes is NOT done in the matrix (it would measure download rather than
//! loaded-code cold start; see README), but it is the correct instrument here: the
//! residual subtraction removes init and the first-request duration, leaving only
//! steps 1-2 (download + environment start), the quantity artifact size is expected to
//! move.

use super::ProbeCommon;
use super::sample::{Sample, aggregate, sample_cold_series};
use crate::aws::Aws;
use crate::config::{self, Arch};
use anyhow::{Context, Result, bail};
use clap::Parser;
use std::path::PathBuf;

/// `probe download-scaling`: synthetic padded-size sweep (zip, and optionally image).
#[derive(Parser)]
pub(super) struct DownloadScalingArgs {
    #[command(flatten)]
    pub(super) common: ProbeCommon,
    /// Output path override for the synthetic download-scaling (zip) JSON. Omit to
    /// write a run-scoped, timestamped file under the repo-root `results/` dir
    /// (`results/lifecycle-download-scaling-<run_id>.json`), gitignored like the
    /// matrix runs and discovered by the site at build time.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Also run the CONTAINER-IMAGE family: per size, assemble and push one padded
    /// image to ECR (via crane) and deploy two functions from it, `image-untouched`
    /// (padding baked in but never read; lazy block-loading may skip it) and
    /// `image-touched` (handler reads the padding at init, forcing the blocks to be
    /// pulled). Off by default; requires crane on PATH (daemonless, so the publish
    /// pipeline runs it). Its zip baseline is co-measured in this same run.
    #[arg(long, default_value_t = false)]
    with_image: bool,
    /// Output path override for the container-image download-scaling JSON (distinct
    /// from `--out` so the zip chart's schema is untouched). Omit to write a
    /// run-scoped, timestamped file under the repo-root `results/` dir
    /// (`results/lifecycle-download-scaling-image-<run_id>.json`).
    #[arg(long)]
    pub(super) image_out: Option<PathBuf>,
}

/// Upper bound for a synthetic padding size: above this the padded package would
/// breach Lambda's 250 MB unzipped limit. Padding is stored (uncompressed), so zip
/// size ≈ unzipped size; 240 leaves headroom for the base entry + zip overhead.
/// The size set is a fixed const (`config::SYNTH_DEFAULT_SIZES_MB`), so the
/// `sizes_in_range` test enforces this bound rather than a runtime check.
#[cfg(test)]
const MAX_SYNTHETIC_MB: u32 = 240;

/// Memory tier for the synthetic functions. Fixed at 512 MB (a mid matrix tier):
/// the download+start residual is flat across memory (steps 1-2 run before the
/// configured CPU applies), so one tier suffices and the axis under test is size.
pub(super) const SYNTH_MEMORY_MB: i32 = 512;

/// Trivial Python handler for the managed-runtime synthetic series (the probe
/// measures download+start, not handler work, so the body is a constant).
const SYNTH_PY_HANDLER: &[u8] = b"def handler(event, context):\n    return {'ok': True}\n";

/// The two runtime FAMILIES the synthetic probe spans, so the download slope can
/// be confirmed identical across them (managed vs custom runtime). Python packs a
/// trivial `handler.py`; Rust reuses the already-compiled `hello` `bootstrap` from
/// `dist/` (a real custom-runtime executable that answers invokes), so no runtime
/// is hand-written and neither base file's size matters next to the padding.
#[derive(Clone, Copy)]
enum SynthRuntime {
    /// Managed `python3.14`.
    Python,
    /// Custom `provided.al2023` (the `hello` bootstrap).
    Rust,
}

impl SynthRuntime {
    /// Short family label used in function names and the chart series.
    fn family(self) -> &'static str {
        match self {
            SynthRuntime::Python => "python",
            SynthRuntime::Rust => "rust",
        }
    }
    fn runtime(self) -> &'static str {
        match self {
            SynthRuntime::Python => "python3.14",
            SynthRuntime::Rust => "provided.al2023",
        }
    }
    fn handler(self) -> &'static str {
        match self {
            SynthRuntime::Python => "handler.handler",
            SynthRuntime::Rust => "bootstrap",
        }
    }
}

/// One synthetic (runtime × size) aggregated result.
#[derive(serde::Serialize, Clone)]
pub(super) struct SyntheticSample {
    /// Runtime family: `python` (managed) or `rust` (custom `provided.al2023`).
    family: String,
    size_mb: u32,
    zip_bytes: u64,
    n_samples: u32,
    w_cold_p50: f64,
    w_cold_min: f64,
    w_cold_max: f64,
    init_p50: f64,
    cold_dur_p50: f64,
    warm_rtt_p50: f64,
    w_warm_p50: f64,
    w_warm_min: f64,
    w_warm_max: f64,
    residual_p50: f64,
    residual_min: f64,
    residual_max: f64,
}

/// Top-level synthetic output written to `download-scaling --out`. No `account_id`,
/// same privacy rule as the matrix probe output.
#[derive(serde::Serialize)]
struct SyntheticOutput {
    generated_at_unix_ms: u128,
    region: String,
    note: String,
    memory_mb: i32,
    arch: String,
    n_warm_per_sample: u32,
    samples: Vec<SyntheticSample>,
}

/// Reads the compiled `hello` `bootstrap` bytes out of the already-built Rust
/// artifact zip (`dist/rust-hello-arm64-o3.zip`), so the Rust synthetic series
/// deploys a REAL custom-runtime executable that answers invokes rather than a
/// stub. Fails loud if the artifact is missing (build it first).
fn read_rust_bootstrap(repo_root: &std::path::Path, arch: Arch) -> Result<Vec<u8>> {
    let label = format!("rust-hello-{}-o3", arch.as_str());
    let zip_path = repo_root.join("dist").join(format!("{label}.zip"));
    let bytes = std::fs::read(&zip_path).with_context(|| {
        format!(
            "reading {} for the Rust synthetic base (build it: `bencher build --lang rust`)",
            zip_path.display()
        )
    })?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .with_context(|| format!("opening {}", zip_path.display()))?;
    let mut file = archive
        .by_name("bootstrap")
        .with_context(|| format!("no `bootstrap` entry in {}", zip_path.display()))?;
    let mut out = Vec::with_capacity(file.size() as usize);
    std::io::Read::read_to_end(&mut file, &mut out).context("reading bootstrap bytes")?;
    Ok(out)
}

/// Builds a zip whose deployed size is ~`size_mb`: the runtime's base entry
/// (`handler.py` for Python, the real `bootstrap` for Rust, `exec` set so Lambda
/// can run it) plus one `padding.bin` entry written with `Stored` (no zip
/// compression), so the zip size ≈ its uncompressed content. The padding is
/// incompressible pseudo-random bytes (a dependency-free SplitMix64 fill, since
/// there is no `rand` dep): a zero-filled blob would compress/dedupe below the zip
/// at AWS's storage and transport layer, understating download cost and collapsing
/// the residual. Incompressible bytes make the deployed size the true download
/// size. The blob is sized to hit the target after the base entry and zip
/// overhead.
fn build_padded_zip(rt: SynthRuntime, base: &[u8], size_mb: u32) -> Result<Vec<u8>> {
    use std::io::Write;
    use zip::write::FileOptions;

    let (base_name, base_mode) = match rt {
        SynthRuntime::Python => ("handler.py", 0o644),
        SynthRuntime::Rust => ("bootstrap", 0o755),
    };
    let target = size_mb as usize * 1024 * 1024;
    // Leave room for the base entry and both entries' zip metadata headers. Fail
    // loud rather than ship an under-sized zip: if the base entry (e.g. the real
    // Rust `hello` bootstrap) plus overhead already meets or exceeds the requested
    // size, `size_mb` would mislabel the true artifact size. Unreachable with the
    // current `SYNTH_DEFAULT_SIZES_MB` (min 1 MB >> base + overhead); a guard for a
    // future too-small entry in that const.
    let overhead = base.len() + 1024;
    if overhead >= target {
        bail!(
            "synthetic size {size_mb} MB is too small for the {base_name} base entry \
             ({} bytes + overhead); raise this SYNTH_DEFAULT_SIZES_MB entry",
            base.len()
        );
    }
    let pad = target - overhead;

    let mut buf = std::io::Cursor::new(Vec::with_capacity(target));
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let stored = |mode: u32| -> FileOptions<()> {
            FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .unix_permissions(mode)
        };
        zip.start_file(base_name, stored(base_mode))?;
        zip.write_all(base)?;
        zip.start_file("padding.bin", stored(0o644))?;
        write_incompressible(&mut zip, pad)?;
        zip.finish()?;
    }
    Ok(buf.into_inner())
}

/// Writes `len` incompressible pseudo-random bytes to `sink`, a chunk at a time,
/// so a second full-size buffer is never held. Dependency-free SplitMix64 with a
/// fixed seed (there is no `rand` dep), so builds are deterministic.
///
/// Incompressibility is the point: a zero-filled or repetitive blob would
/// compress/dedupe below the zip (`Stored`) or in a container image's gzip layer
/// on push, so the transferred size would fall far below the target and understate
/// the download term. SplitMix64 output does not compress, so the on-the-wire size
/// is the true download size. Shared by the zip padding (`build_padded_zip`) and
/// the container-image padding (`write_padding_file`).
pub(super) fn write_incompressible<W: std::io::Write>(
    sink: &mut W,
    len: usize,
) -> std::io::Result<()> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut chunk = vec![0u8; 1024 * 1024];
    let mut written = 0usize;
    while written < len {
        for word in chunk.chunks_exact_mut(8) {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            word.copy_from_slice(&z.to_le_bytes());
        }
        let n = (len - written).min(chunk.len());
        sink.write_all(&chunk[..n])?;
        written += n;
    }
    Ok(())
}

/// The synthetic download-scaling run: deploy each padded size, measure, then tear
/// down (on success AND on error, via `teardown_synthetic`). Serial, like the
/// matrix probe.
pub(super) async fn run_synthetic(
    repo_root: &std::path::Path,
    args: DownloadScalingArgs,
) -> Result<()> {
    let arch = Arch::parse(&args.common.arch).map_err(anyhow::Error::msg)?;

    // The size set is methodology, not a CLI knob (like the matrix's iteration
    // `Profile`): the exact set the published chart is built from, and the set
    // `bencher teardown` enumerates to reclaim these functions. Fixing it in config
    // makes teardown provably complete (no run can create a size teardown does not
    // know). `sizes_in_range` pins every entry within Lambda's package limit at
    // test time.
    let sizes = config::SYNTH_DEFAULT_SIZES_MB;

    let aws = Aws::load().await?;
    println!(
        "== lambdabench probe download-scaling == account {} region {}",
        aws.account_id,
        config::REGION
    );
    println!(
        "sizes: {:?} MB @ {} MB / {}, {} cold sample(s) x (1 prewarm + 1 cold + {} warm) each",
        sizes,
        SYNTH_MEMORY_MB,
        arch.as_str(),
        args.common.cold_samples,
        args.common.warm_per_sample
    );

    let role_arn = aws.ensure_role().await.context("ensuring exec role")?;
    // Padded synthetic zips exceed the inline limit and are staged to S3, so the
    // bucket must exist. Ensure it here (idempotent, bucket-only, no scenario seed
    // objects) rather than assume a prior matrix deploy created it: the probe is
    // otherwise self-contained, and `bencher teardown` removes the bucket, so a
    // probe run after a teardown would otherwise fail with NoSuchBucket when
    // staging.
    aws.ensure_bucket()
        .await
        .context("ensuring S3 bucket for staged synthetic zips")?;
    let timed = aws.retryless_lambda_client();

    // Two runtime families to confirm the download slope is identical across them:
    // Python (managed) uses a trivial handler; Rust (custom provided.al2023)
    // reuses the already-compiled hello bootstrap from dist/.
    let rust_bootstrap = read_rust_bootstrap(repo_root, arch)?;
    let runtimes: [(SynthRuntime, Vec<u8>); 2] = [
        (SynthRuntime::Python, SYNTH_PY_HANDLER.to_vec()),
        (SynthRuntime::Rust, rust_bootstrap),
    ];

    // Track every created (function, staged-s3-key) so teardown removes them even
    // if a later size fails partway through.
    let mut created: Vec<(String, String)> = Vec::new();
    let result = sample_all(
        &aws,
        &timed,
        &role_arn,
        arch,
        &runtimes,
        sizes,
        &args.common,
        &mut created,
    )
    .await;

    // Best-effort teardown regardless of outcome; `bencher teardown` deletes these
    // exact names as a backstop if this misses anything.
    teardown_synthetic(&aws, &created).await;

    let samples = result?;

    // The image family (if requested) plots against a zip baseline co-measured in
    // THIS run, not the separately-written zip JSON, so the shared chart stays a
    // single-session snapshot (see ImageOutput::zip_baseline). Capture the python
    // family (matching the image's managed Python base) before `samples` moves into
    // the zip output below.
    let zip_baseline: Vec<SyntheticSample> = samples
        .iter()
        .filter(|s| s.family == "python")
        .cloned()
        .collect();

    let out = SyntheticOutput {
        generated_at_unix_ms: config::now_unix_ms(),
        region: config::REGION.to_string(),
        note: "illustrative, environment-dependent magnitudes (single client, single account; \
               not matrix data). Ephemeral padded functions isolate steps 1-2 (download + \
               environment start): residual = W_cold - init - cold_duration - warm_rtt (mostly \
               download + environment start, plus a small scheduling/placement remainder the \
               warm-RTT subtraction does not cancel), which no REPORT line reports. Two runtime families (python managed, \
               rust custom provided.al2023) confirm \
               the download slope is family-independent. Padding is inert (it is the right \
               instrument for isolating download here, and is deliberately never used in the \
               matrix; see README)."
            .to_string(),
        memory_mb: SYNTH_MEMORY_MB,
        arch: arch.as_str().to_string(),
        n_warm_per_sample: args.common.warm_per_sample,
        samples,
    };
    let out_path = super::probe_out_path(&args.out, "download-scaling", &config::run_id());
    super::write_json(repo_root, &out_path, &out)?;

    // Container-image family (opt-in). Runs after the zip families and writes a
    // separate JSON carrying its own co-measured `zip_baseline`, so the shared
    // zip-vs-image chart keeps both series on one session/vantage/date. The only
    // probe path that shells out to `crane` (daemonless, so it runs in the publish
    // pipeline); the matrix build/deploy/run and the download-start probe never
    // require it.
    if args.with_image {
        super::image::run_synthetic_image(
            repo_root,
            &aws,
            &timed,
            &role_arn,
            &args,
            arch,
            sizes,
            zip_baseline,
        )
        .await?;
    }
    Ok(())
}

/// Deploys and measures every (runtime × size) in order, recording created
/// resources into `created` as it goes (so a mid-run failure still tears down what
/// was made).
#[allow(clippy::too_many_arguments)]
async fn sample_all(
    aws: &Aws,
    timed: &aws_sdk_lambda::Client,
    role_arn: &str,
    arch: Arch,
    runtimes: &[(SynthRuntime, Vec<u8>)],
    sizes: &[u32],
    args: &ProbeCommon,
    created: &mut Vec<(String, String)>,
) -> Result<Vec<SyntheticSample>> {
    let mut out = Vec::new();
    for (rt, base) in runtimes {
        for &mb in sizes {
            // Shared name formatter so this create site and the teardown
            // enumerator (`config::all_managed_function_names`) can never drift.
            let name = config::synth_function_name(rt.family(), mb);
            println!("\n-- {name} (building {mb} MB zip)");
            let zip = build_padded_zip(*rt, base, mb)?;
            let zip_bytes = zip.len() as u64;
            let spec = crate::aws::lambda::SyntheticFn {
                name: name.clone(),
                runtime: rt.runtime(),
                handler: rt.handler(),
                memory_mb: SYNTH_MEMORY_MB,
                arch: arch.lambda_arch(),
            };
            let s3_key = aws
                .create_function_from_zip(&spec, role_arn, &zip)
                .await
                .with_context(|| format!("deploying {name}"))?;
            created.push((name.clone(), s3_key));

            // Buffer-then-commit per size: all N samples into a fresh Vec, returned
            // only if every sample succeeds, so a transient failure re-runs the
            // whole size (the function stays deployed across retries). Synthetic
            // functions are trivial handlers, never HTTP-fronted.
            let samples = sample_cold_series(
                aws,
                timed,
                &name,
                "{}",
                false,
                args.cold_samples,
                args.warm_per_sample,
            )
            .await?;
            out.push(aggregate_synthetic(rt.family(), mb, zip_bytes, &samples));
        }
    }
    Ok(out)
}

/// Aggregates one (runtime × size)'s samples into a `SyntheticSample` (p50s +
/// residual spread).
fn aggregate_synthetic(
    family: &str,
    size_mb: u32,
    zip_bytes: u64,
    samples: &[Sample],
) -> SyntheticSample {
    let a = aggregate(samples);
    SyntheticSample {
        family: family.to_string(),
        size_mb,
        zip_bytes,
        n_samples: a.n_samples,
        w_cold_p50: a.w_cold_p50,
        w_cold_min: a.w_cold_min,
        w_cold_max: a.w_cold_max,
        init_p50: a.init_p50,
        cold_dur_p50: a.cold_dur_p50,
        warm_rtt_p50: a.warm_rtt_p50,
        w_warm_p50: a.w_warm_p50,
        w_warm_min: a.w_warm_min,
        w_warm_max: a.w_warm_max,
        residual_p50: a.residual_p50,
        residual_min: a.residual_min,
        residual_max: a.residual_max,
    }
}

/// Best-effort teardown of the synthetic functions and their staged S3 zips.
/// Logs failures rather than propagating: the caller's real result takes
/// priority, and `bencher teardown`'s exact-name delete is the hard backstop.
async fn teardown_synthetic(aws: &Aws, created: &[(String, String)]) {
    if created.is_empty() {
        return;
    }
    println!("\n-- tearing down {} synthetic function(s)", created.len());
    for (name, s3_key) in created {
        if let Err(e) = aws.delete_function(name).await {
            eprintln!("   WARNING: delete_function {name} failed: {e:#}");
        }
        if let Err(e) = aws.delete_s3_object(s3_key).await {
            eprintln!("   WARNING: delete_s3_object {s3_key} failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A padded zip lands within a hair of its target size (Stored, so no
    /// compression shrinks it) and is a valid, readable archive with the runtime's
    /// base entry + padding. Checked for both runtime families.
    #[test]
    fn padded_zip_hits_target_and_is_valid() {
        let cases = [
            (
                SynthRuntime::Python,
                "handler.py",
                &b"def handler(e, c):\n    return {}\n"[..],
            ),
            (
                SynthRuntime::Rust,
                "bootstrap",
                &b"\x7fELF fake bootstrap bytes"[..],
            ),
        ];
        for (rt, base_name, base) in cases {
            for mb in [1u32, 5] {
                let bytes = build_padded_zip(rt, base, mb).expect("build padded zip");
                let target = mb as usize * 1024 * 1024;
                // Stored means zip size ≈ target; allow a small overhead window.
                let delta = bytes.len().abs_diff(target);
                assert!(
                    delta < 4096,
                    "{base_name} {mb}MB: got {} bytes, target {target}, delta {delta}",
                    bytes.len()
                );
                let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("valid zip");
                let names: Vec<String> = (0..zip.len())
                    .map(|i| zip.by_index(i).unwrap().name().to_string())
                    .collect();
                assert!(names.iter().any(|n| n == base_name));
                assert!(names.iter().any(|n| n == "padding.bin"));
            }
        }
    }

    /// The zip family labels this probe deploys must equal the constant teardown
    /// enumerates (`config::SYNTH_ZIP_FAMILIES`), or teardown would miss them.
    #[test]
    fn zip_families_match_config() {
        let from_probe = [SynthRuntime::Python.family(), SynthRuntime::Rust.family()];
        assert_eq!(from_probe.as_slice(), config::SYNTH_ZIP_FAMILIES);
    }

    /// The deployed name equals the shared formatter for its family+size, so this
    /// create site and the teardown enumerator can never format a name differently.
    #[test]
    fn deployed_name_matches_shared_formatter() {
        assert_eq!(
            config::synth_function_name(SynthRuntime::Python.family(), 50),
            "lambdabench-synthdl-python-50mb"
        );
    }

    /// Every configured synthetic size stays within Lambda's package limit
    /// (`1..=MAX_SYNTHETIC_MB`). The size set is a fixed code const, so this bound
    /// is checked here rather than against runtime input.
    #[test]
    fn sizes_in_range() {
        for &mb in config::SYNTH_DEFAULT_SIZES_MB {
            assert!(
                (1..=MAX_SYNTHETIC_MB).contains(&mb),
                "synthetic size {mb} MB out of range (1..={MAX_SYNTHETIC_MB})"
            );
        }
    }
}
