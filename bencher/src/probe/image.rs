//! `probe download-scaling --with-image`: the container-image family, the
//! complement to the zip synthetic sweep in [`super::synthetic`].
//!
//! Per size it assembles ONE padded container image on the managed Python base
//! and deploys two functions from it, `image-untouched` (padding baked in but
//! never read, so Lambda's lazy block-level loading may skip it) and
//! `image-touched` (the handler reads the padding at init, forcing the blocks in),
//! measured with the same residual subtraction as the zip family. It documents
//! that packaging type moves where artifact size is paid: a `.zip` pays it in the
//! unreported pre-Init residual, a container image in the reported `Init Duration`
//! (see lifecycle.md "Zip vs container image").
//!
//! The only probe path that shells out to a container tool (`crane`) and pushes to
//! ECR. crane is daemonless (base pull + one-layer append + CMD + push, no
//! container-build daemon or VM), so unlike a `docker`/`finch` build it runs
//! unattended in the publish pipeline.

use super::sample::{Sample, aggregate, sample_cold_series};
use super::synthetic::{
    DownloadScalingArgs, SYNTH_MEMORY_MB, SyntheticSample, write_incompressible,
};
use crate::aws::Aws;
use crate::config::{self, Arch};
use anyhow::{Context, Result, bail};

/// Managed base image for the container-image family. Mirrors the `python` zip
/// family (same runtime), so the zip-vs-image comparison is apples-to-apples. It
/// carries a fixed ~200 MB base-layer floor, so `image_bytes` is base + padding;
/// the probe records the base size separately.
const SYNTH_IMAGE_BASE: &str = "public.ecr.aws/lambda/python:3.14";

/// Handler baked into the container-image family. Reads `padding.bin` fully at
/// IMPORT time (the Init phase) iff `LAMBDABENCH_TOUCH=1`, folding the bytes so
/// the read is not optimized away and every block is faulted in. The
/// `image-untouched` variant leaves the env unset, so the padding blocks are
/// never referenced and Lambda's lazy block-level loading may skip them entirely.
const SYNTH_IMAGE_HANDLER: &[u8] = b"import os\n\
if os.environ.get('LAMBDABENCH_TOUCH') == '1':\n\
\x20\x20\x20\x20with open('/var/task/padding.bin', 'rb') as _f:\n\
\x20\x20\x20\x20\x20\x20\x20\x20_c = 0\n\
\x20\x20\x20\x20\x20\x20\x20\x20while True:\n\
\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20_b = _f.read(1 << 20)\n\
\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20if not _b:\n\
\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20break\n\
\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20_c += len(_b)\n\
\x20\x20\x20\x20print('touched', _c, 'padding bytes at init')\n\
def handler(event, context):\n\
\x20\x20\x20\x20return {'ok': True}\n";

/// One container-image (variant × size) aggregated result. Mirrors
/// `SyntheticSample` but carries `image_bytes` (the real pushed image size ECR
/// reports) instead of `zip_bytes`. `size_mb` is the ADDED padding, the axis
/// comparable to the zip family; `image_bytes` is base + padding, so it always
/// carries the fixed base-image floor (see `base_image_bytes_est` on the output).
#[derive(serde::Serialize)]
struct ImageSample {
    /// `image-untouched` (padding baked in, never read at init, so lazy
    /// block-loading may skip it) or `image-touched` (handler reads all padding
    /// at init, forcing the blocks to be pulled).
    family: String,
    size_mb: u32,
    image_bytes: u64,
    n_samples: u32,
    w_cold_p50: f64,
    init_p50: f64,
    cold_dur_p50: f64,
    warm_rtt_p50: f64,
    residual_p50: f64,
    residual_min: f64,
    residual_max: f64,
}

/// Top-level container-image download-scaling output written to
/// `download-scaling --image-out`.
#[derive(serde::Serialize)]
struct ImageOutput {
    generated_at_unix_ms: u128,
    region: String,
    note: String,
    memory_mb: i32,
    arch: String,
    /// ESTIMATED bytes of the managed base image alone (no padding), derived as
    /// the smallest built image's `image_bytes` minus its padding. An estimate,
    /// not a direct measurement: the smallest image's non-padding content is ~base
    /// plus the tiny handler + zip/layer overhead, and the padding stores ~1:1 (it
    /// is incompressible). Good to within a few MB of the true base; used only to
    /// show the floor a reader subtracts from `image_bytes`, not for computation.
    base_image_bytes_est: u64,
    n_warm_per_sample: u32,
    samples: Vec<ImageSample>,
    /// The `python` zip family measured in the SAME run as the image samples
    /// above, so the zip-vs-image chart compares like with like (same session,
    /// vantage, date). NOT read from the separately-written
    /// `lifecycle-download-scaling.json`: that file is a distinct probe invocation
    /// and could drift in date/vantage. Embedding the co-measured baseline keeps
    /// the comparison a self-contained snapshot. Python only (it matches the
    /// image's managed Python base); the `rust` family is not carried here.
    zip_baseline: Vec<SyntheticSample>,
}

/// The container-image download-scaling run: per size, build+push ONE padded
/// image, deploy two functions from it (`image-untouched` and `image-touched`),
/// measure the same residual as the zip family, and tear down. Writes
/// `--image-out`. Requires `crane` on PATH (fails loud if absent); this is the only
/// probe path with that dependency. crane is daemonless, so this runs in the
/// publish pipeline (ECS Fargate) the same as the zip families.
///
/// Cleanup discipline: created functions and pushed image tags are recorded in
/// ledgers as they are made (the tag the instant `crane mutate` succeeds), and
/// `teardown_synthetic_images` runs on both success and error before the result
/// propagates, so an error partway through still reclaims what was made. A hard
/// kill (SIGINT) between creating a resource and that teardown bypasses it; the
/// backstop is `bencher teardown`, which deletes these exact function names (the
/// size set is a fixed const, so teardown enumerates every name the probe can
/// create) and force-deletes the `lambdabench-synthdl` repo with its images. The
/// one thing no teardown reclaims is the local `/tmp` build context, which
/// `TempDirGuard` removes on every normal return but not on a signal kill (a
/// bounded, local, non-billing leak).
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_synthetic_image(
    repo_root: &std::path::Path,
    aws: &Aws,
    timed: &aws_sdk_lambda::Client,
    role_arn: &str,
    args: &DownloadScalingArgs,
    arch: Arch,
    sizes: &[u32],
    zip_baseline: Vec<SyntheticSample>,
) -> Result<()> {
    println!(
        "\n== lambdabench probe download-scaling (CONTAINER IMAGE) == account {} region {}",
        aws.account_id,
        config::REGION
    );
    println!(
        "image sizes: {:?} MB (added padding) @ {} MB / {}, base {}",
        sizes,
        SYNTH_MEMORY_MB,
        arch.as_str(),
        SYNTH_IMAGE_BASE
    );

    aws.ensure_ecr_repo().await.context("ensuring ECR repo")?;
    aws.crane_ecr_login()
        .await
        .context("logging crane into ECR (is crane installed and on PATH?)")?;

    // Track created functions and pushed image tags so teardown removes them even
    // if a later size fails partway through.
    let mut created_fns: Vec<String> = Vec::new();
    let mut created_tags: Vec<String> = Vec::new();
    let result = sample_all_images(
        aws,
        timed,
        role_arn,
        arch,
        sizes,
        &args.common,
        &mut created_fns,
        &mut created_tags,
    )
    .await;

    teardown_synthetic_images(aws, &created_fns, &created_tags).await;

    let (samples, base_image_bytes_est) = result?;
    let out = ImageOutput {
        generated_at_unix_ms: config::now_unix_ms(),
        region: config::REGION.to_string(),
        note: "illustrative, environment-dependent magnitudes (single client, single account; \
               not matrix data). Ephemeral padded CONTAINER-IMAGE functions, measured with the \
               same residual = W_cold - init - cold_duration - warm_rtt as the zip family (mostly \
               download + environment start, plus a small scheduling/placement remainder the warm-RTT \
               subtraction does not cancel). Two \
               variants from ONE image per size: image-untouched (padding baked in but never read, \
               so Lambda's lazy block-level loading may skip it) and image-touched (handler reads \
               all padding at init, forcing the blocks to be pulled). size_mb is the ADDED padding \
               (the axis comparable to the zip family); image_bytes is base + padding, so subtract \
               base_image_bytes_est (an estimate, not a measurement) for the added download. \
               zip_baseline is the python zip family measured in THIS same run, so the zip-vs-image \
               chart compares like with like (same session/vantage/date) rather than reusing the \
               separately-written zip file. Images are assembled and pushed with crane (daemonless), \
               so this refreshes in the publish pipeline like the zip families. See lifecycle.md \
               'Zip vs container image'."
            .to_string(),
        memory_mb: SYNTH_MEMORY_MB,
        arch: arch.as_str().to_string(),
        base_image_bytes_est,
        n_warm_per_sample: args.common.warm_per_sample,
        samples,
        zip_baseline,
    };
    let out_path =
        super::probe_out_path(&args.image_out, "download-scaling-image", &config::run_id());
    super::write_json(repo_root, &out_path, &out)?;
    Ok(())
}

/// RAII guard that removes a temp build-context directory on drop, so the
/// directory (holding an up-to-240 MB `padding.bin`) is cleaned up on every exit
/// path from `build_and_push_image` (early error via `?`/`bail!` as well as
/// success). Best-effort: a removal failure is logged, never propagated.
struct TempDirGuard(std::path::PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        // remove_dir_all errors if the dir is already gone; only warn on a real
        // failure (dir still present after the attempt), not that benign case.
        if let Err(e) = std::fs::remove_dir_all(&self.0)
            && self.0.exists()
        {
            eprintln!(
                "   WARNING: failed to remove temp build context {}: {e}",
                self.0.display()
            );
        }
    }
}

/// Writes `padding.bin` of `size_mb` incompressible bytes into `dir` using the
/// shared `write_incompressible` fill. Separate from `build_padded_zip` (which
/// packs the padding into a zip); here the file becomes an image-layer entry,
/// where incompressibility keeps gzip layer compression on push from shrinking it
/// below the target and understating the download.
fn write_padding_file(dir: &std::path::Path, size_mb: u32) -> Result<()> {
    let target = size_mb as usize * 1024 * 1024;
    let mut f = std::io::BufWriter::new(
        std::fs::File::create(dir.join("padding.bin")).context("creating padding.bin")?,
    );
    write_incompressible(&mut f, target).context("writing padding.bin")?;
    std::io::Write::flush(&mut f).context("flushing padding.bin")?;
    Ok(())
}

/// Builds the single image layer tarball (`layer.tar` in `dir`) carrying
/// `handler.py` and `padding.bin` at `var/task/` (the resolved `${LAMBDA_TASK_ROOT}`
/// on the managed base). Returns the tarball path. The tar is UNCOMPRESSED, so the
/// incompressible `padding.bin` reaches the wire at full size (the download axis);
/// crane still gzips the layer on push, but SplitMix64 padding does not shrink.
/// `padding.bin` is appended from disk (streamed), never held in memory.
fn build_layer_tar(dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let tar_path = dir.join("layer.tar");
    let file = std::fs::File::create(&tar_path)
        .with_context(|| format!("creating {}", tar_path.display()))?;
    let mut builder = tar::Builder::new(std::io::BufWriter::new(file));
    builder
        .append_path_with_name(dir.join("handler.py"), "var/task/handler.py")
        .context("adding handler.py to image layer")?;
    builder
        .append_path_with_name(dir.join("padding.bin"), "var/task/padding.bin")
        .context("adding padding.bin to image layer")?;
    // into_inner writes the tar trailer and returns the BufWriter; flush it
    // explicitly so a failure writing the final buffered bytes (e.g. disk full on
    // a 240 MB layer) surfaces here rather than being swallowed by BufWriter::drop,
    // which would hand crane a truncated tar (same discipline as
    // write_padding_file).
    let mut w = builder.into_inner().context("finishing image layer tar")?;
    std::io::Write::flush(&mut w).context("flushing image layer tar")?;
    Ok(tar_path)
}

/// Builds ONE padded container image of `size_mb` added padding and pushes it to
/// ECR, returning `(tag, image_bytes)`. Writes a temp build context (handler.py +
/// padding.bin + the assembled layer.tar) guarded by `TempDirGuard` so it is
/// removed on every exit path, then shells `crane mutate` once to pull the managed
/// base at the target platform, append the layer, set the handler CMD, and push the
/// result to the ECR tag. Reads the pushed size back from ECR. Fails loud if crane
/// is absent or exits non-zero (the only probe path with a crane dependency, and
/// the sole reason the runner image carries crane).
///
/// crane is daemonless (google/go-containerregistry): the whole "build" is a base
/// pull + one-layer append + config edit + push, no container engine, VM, or
/// privilege, so it runs unattended inside the ECS Fargate publish task. The
/// managed base's ENTRYPOINT (the runtime interface client) plus this CMD carry the
/// handler, so the function needs no runtime/handler set (see
/// create_function_from_image).
///
/// The tag is run-unique (`synthdl-image-{mb}mb-{arch}-{nonce}`) so a rerun never
/// collides with a stale immutable tag; the `image-untouched`/`image-touched`
/// functions both deploy from this one tag.
///
/// The tag is pushed into `created_tags` the instant `crane mutate` succeeds,
/// before the fallible `ecr_image_size` read, so a pushed image is always recorded
/// for teardown even if reading its size then fails.
async fn build_and_push_image(
    aws: &Aws,
    size_mb: u32,
    arch: Arch,
    nonce: &str,
    created_tags: &mut Vec<String>,
) -> Result<(String, u64)> {
    use std::process::Command;

    let ctx = std::env::temp_dir().join(format!(
        "lambdabench-imgctx-{}-{size_mb}mb",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&ctx);
    std::fs::create_dir_all(&ctx).with_context(|| format!("creating {}", ctx.display()))?;
    // Remove the context (and its large padding.bin + layer.tar) on every return path.
    let _ctx_guard = TempDirGuard(ctx.clone());

    std::fs::write(ctx.join("handler.py"), SYNTH_IMAGE_HANDLER).context("writing handler.py")?;
    write_padding_file(&ctx, size_mb)?;
    let layer_tar = build_layer_tar(&ctx)?;

    let tag = format!("synthdl-image-{size_mb}mb-{}-{nonce}", arch.as_str());
    let uri = aws.ecr_image_uri(&tag);
    let platform = format!("linux/{}", arch.oci_arch());

    println!("   building {size_mb} MB image ({uri})");
    // One `crane mutate`: pull SYNTH_IMAGE_BASE at --platform, append the layer,
    // set the CMD, and push the mutated image to `uri` (a full registry ref, so it
    // lands in our ECR repo, not the public base's registry).
    let out = Command::new("crane")
        .args([
            "mutate",
            SYNTH_IMAGE_BASE,
            "--platform",
            &platform,
            "--append",
            &layer_tar.to_string_lossy(),
            "--cmd",
            "handler.handler",
            "-t",
            &uri,
        ])
        .output()
        .context("spawning `crane mutate` (is crane installed and on PATH?)")?;
    if !out.status.success() {
        bail!(
            "`crane mutate` failed ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // The image now exists in ECR; record its tag for teardown before the fallible
    // size read, so a failure there cannot orphan a pushed image.
    created_tags.push(tag.clone());

    let image_bytes = aws.ecr_image_size(&tag).await?;
    Ok((tag, image_bytes))
}

/// Deploys and measures the container-image family: per size, build+push one
/// image, then deploy and sample two functions from it (`image-untouched` and
/// `image-touched`). Records created functions + pushed tags into the ledgers so a
/// mid-run failure still tears down what was made. Returns the samples plus the
/// measured base-image size (read once from the smallest built image, since the
/// base layer is identical across sizes).
#[allow(clippy::too_many_arguments)]
async fn sample_all_images(
    aws: &Aws,
    timed: &aws_sdk_lambda::Client,
    role_arn: &str,
    arch: Arch,
    sizes: &[u32],
    args: &super::ProbeCommon,
    created_fns: &mut Vec<String>,
    created_tags: &mut Vec<String>,
) -> Result<(Vec<ImageSample>, u64)> {
    let nonce = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let mut out = Vec::new();
    // The base layer is shared across sizes, so estimate the base floor as
    // (smallest image_bytes - its padding). The caller sorts sizes ascending, so
    // idx 0 is the smallest padding, minimizing the estimate's error. An estimate,
    // not a measurement (see ImageOutput doc).
    let mut base_image_bytes_est = 0u64;

    // `sizes` must be ascending for the idx==0 base-floor estimate below. The
    // caller passes `config::SYNTH_DEFAULT_SIZES_MB` verbatim, which is defined
    // ascending; assert it here rather than trust a const declared elsewhere,
    // since nothing enforces the ordering at the definition site.
    debug_assert!(
        sizes.windows(2).all(|w| w[0] <= w[1]),
        "sample_all_images expects ascending sizes (base-floor estimate assumes idx 0 is smallest)"
    );

    for (idx, &mb) in sizes.iter().enumerate() {
        println!("\n-- image {mb} MB");
        // build_and_push_image records the tag into created_tags itself (right
        // after push succeeds).
        let (tag, image_bytes) = build_and_push_image(aws, mb, arch, nonce, created_tags).await?;
        let uri = aws.ecr_image_uri(&tag);
        if idx == 0 {
            base_image_bytes_est = image_bytes.saturating_sub(mb as u64 * 1024 * 1024);
        }

        // Two functions from the SAME image URI, differing only by the touch env.
        // Family label from the shared constant (indexed by touch: [untouched,
        // touched]) and name from the shared formatter, so this create site and the
        // teardown enumerator can never drift.
        for touch in [false, true] {
            let family = config::SYNTH_IMAGE_FAMILIES[touch as usize];
            let name = config::synth_function_name(family, mb);
            println!("   deploying {name}");
            aws.create_function_from_image(
                &name,
                role_arn,
                &uri,
                SYNTH_MEMORY_MB,
                arch.lambda_arch(),
                touch,
            )
            .await
            .with_context(|| format!("deploying {name}"))?;
            created_fns.push(name.clone());

            // Buffer-then-commit per function: all N samples into a fresh Vec,
            // returned only if every sample succeeds, so a transient failure
            // re-runs this function's sampling (image + function stay in place).
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
            out.push(aggregate_image(family, mb, image_bytes, &samples));
        }
    }
    Ok((out, base_image_bytes_est))
}

/// Aggregates one (variant × size)'s samples into an `ImageSample`.
fn aggregate_image(
    family: &str,
    size_mb: u32,
    image_bytes: u64,
    samples: &[Sample],
) -> ImageSample {
    let a = aggregate(samples);
    ImageSample {
        family: family.to_string(),
        size_mb,
        image_bytes,
        n_samples: a.n_samples,
        w_cold_p50: a.w_cold_p50,
        init_p50: a.init_p50,
        cold_dur_p50: a.cold_dur_p50,
        warm_rtt_p50: a.warm_rtt_p50,
        residual_p50: a.residual_p50,
        residual_min: a.residual_min,
        residual_max: a.residual_max,
    }
}

/// Best-effort teardown of the image family: delete the deployed functions and
/// the pushed ECR image tags. Logs rather than propagating (the caller's real
/// result takes priority); `bencher teardown` (exact-name function delete +
/// `lambdabench-synthdl` repo delete) is the hard backstop.
async fn teardown_synthetic_images(aws: &Aws, fns: &[String], tags: &[String]) {
    if fns.is_empty() && tags.is_empty() {
        return;
    }
    println!(
        "\n-- tearing down {} image function(s) + {} image tag(s)",
        fns.len(),
        tags.len()
    );
    for name in fns {
        if let Err(e) = aws.delete_function(name).await {
            eprintln!("   WARNING: delete_function {name} failed: {e:#}");
        }
    }
    for tag in tags {
        if let Err(e) = aws.delete_ecr_image(tag).await {
            eprintln!("   WARNING: delete_ecr_image {tag} failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host-written `padding.bin` (used by the container-image family) hits
    /// the requested size exactly and is incompressible: re-deflating it must not
    /// shrink it meaningfully, or a container image's gzip'd layer would transfer
    /// far below the target and understate the download term.
    #[test]
    fn image_padding_hits_target_and_is_incompressible() {
        use flate2::{Compression, write::ZlibEncoder};
        let dir = std::env::temp_dir().join(format!("lambdabench-padtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for mb in [1u32, 4] {
            write_padding_file(&dir, mb).expect("write padding");
            let bytes = std::fs::read(dir.join("padding.bin")).unwrap();
            let target = mb as usize * 1024 * 1024;
            assert_eq!(
                bytes.len(),
                target,
                "{mb}MB padding must be exactly {target} bytes"
            );
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            std::io::Write::write_all(&mut enc, &bytes).unwrap();
            let compressed = enc.finish().unwrap();
            // Incompressible: deflate cannot shrink it below ~99% of the original.
            assert!(
                compressed.len() as f64 > bytes.len() as f64 * 0.99,
                "{mb}MB padding compressed to {} of {} bytes (too compressible)",
                compressed.len(),
                bytes.len()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The image family labels this probe deploys, in create order (`[false,
    /// true]` → untouched, touched), must equal the constant teardown enumerates,
    /// or teardown would miss them.
    #[test]
    fn image_families_match_config_in_create_order() {
        let from_probe = [false, true].map(|touch| config::SYNTH_IMAGE_FAMILIES[touch as usize]);
        assert_eq!(from_probe.as_slice(), config::SYNTH_IMAGE_FAMILIES);
        assert_eq!(
            config::SYNTH_IMAGE_FAMILIES,
            &["image-untouched", "image-touched"]
        );
    }
}
