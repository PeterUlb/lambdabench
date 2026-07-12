//! Provisioning: IAM role + DynamoDB table, then create/update all functions.

use crate::aws::Aws;
use crate::build::Artifact;
use crate::config::Cell;
use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Concurrency for create/update of functions (independent operations).
const DEPLOY_CONCURRENCY: usize = 8;

/// Per-function artifact sizes recorded into each result row.
///
/// Intentionally NOT `Default`: a row's artifact sizes must be the real measured
/// bytes, never a silent zero fallback, so every path producing an
/// `ArtifactSizes` fails loud if the artifact is missing.
#[derive(Clone, Copy)]
pub struct ArtifactSizes {
    pub zip: u64,
    pub unzipped: u64,
}

/// Computes the per-cell artifact sizes from the built artifacts, without
/// touching AWS. Used by `run --skip-deploy`, which needs the size map for the
/// results metadata but does not re-provision. Same lookup `deploy` does inline.
/// Fails loud if a cell has no matching artifact rather than recording
/// placeholder sizes.
pub fn sizes_from_artifacts(
    artifacts: &BTreeMap<String, Artifact>,
    cells: &[Cell],
) -> Result<BTreeMap<String, ArtifactSizes>> {
    let mut sizes = BTreeMap::new();
    for cell in cells {
        let label = cell.artifact_key().label();
        let a = artifacts.get(&label).with_context(|| {
            format!(
                "no built artifact {label} for cell {} (cannot record real sizes)",
                cell.function_name()
            )
        })?;
        sizes.insert(
            cell.function_name(),
            ArtifactSizes {
                zip: a.zip_size_bytes,
                unzipped: a.unzipped_size_bytes,
            },
        );
    }
    Ok(sizes)
}

/// Ensures infra exists and every cell's function is created/updated with its
/// artifact. Returns the per-cell artifact sizes for the results metadata.
pub async fn deploy(
    aws: &Aws,
    artifacts: &BTreeMap<String, Artifact>,
    cells: &[Cell],
) -> Result<BTreeMap<String, ArtifactSizes>> {
    println!("Ensuring IAM execution role...");
    let role_arn = aws.ensure_role().await.context("ensuring role")?;
    println!("  role: {role_arn}");

    println!("Ensuring DynamoDB table + seed item...");
    aws.ensure_table().await.context("ensuring table")?;

    println!("Ensuring KMS key + S3 bucket/object (for three_client)...");
    let kms_key_id = aws.ensure_kms_key().await.context("ensuring KMS key")?;
    // Publish the resolved key id for the env-building / cold-trigger paths. The
    // OnceLock is set once per `Aws`; a second deploy on the same bundle that
    // resolved a DIFFERENT key id is a real inconsistency (functions wired to a
    // stale key), so fail loud rather than silently keep the old value as
    // `.set(...).ok()` would. `set` returns Err when already populated, so compare
    // against the stored value via `get`.
    if aws.kms_key_id.set(kms_key_id.clone()).is_err() {
        let stored = aws
            .kms_key_id
            .get()
            .expect("set failed, so it is populated");
        if *stored != kms_key_id {
            anyhow::bail!(
                "KMS key id already set to {stored} but deploy resolved {kms_key_id}; \
                 functions would be wired to a stale key"
            );
        }
    }
    aws.ensure_seeded_bucket()
        .await
        .context("ensuring S3 bucket/object")?;
    println!("  kms key: {kms_key_id}");
    println!("  s3 bucket: {}", aws.bucket_name());

    println!("Deploying {} functions...", cells.len());

    // Per-cell artifact sizes for the results metadata. Same lookup the
    // --skip-deploy path uses, so the two cannot disagree (fails loud if a cell
    // has no matching artifact).
    let sizes = sizes_from_artifacts(artifacts, cells)?;

    // Map each cell to its artifact's zip bytes (read once per artifact).
    let mut zip_cache: BTreeMap<String, Arc<Vec<u8>>> = BTreeMap::new();
    for art in artifacts.values() {
        let bytes = std::fs::read(&art.zip_path)
            .with_context(|| format!("reading artifact {}", art.zip_path.display()))?;
        zip_cache.insert(art.key.label(), Arc::new(bytes));
    }

    // Resolve (cell -> artifact bytes) up front so the async tasks are simple.
    let mut work: Vec<(Cell, Arc<Vec<u8>>)> = Vec::new();
    for cell in cells {
        let label = cell.artifact_key().label();
        let bytes = zip_cache
            .get(&label)
            .with_context(|| format!("no built artifact for {label} (run `build` first)"))?
            .clone();
        work.push((*cell, bytes));
    }

    let role_arn = Arc::new(role_arn);
    let results: Vec<Result<()>> = stream::iter(work.into_iter().map(|(cell, bytes)| {
        let aws = aws.clone();
        let role_arn = role_arn.clone();
        async move {
            aws.ensure_function(&cell, &role_arn, &bytes)
                .await
                .with_context(|| format!("deploying {}", cell.function_name()))?;
            println!("  deployed {}", cell.function_name());
            Ok(())
        }
    }))
    .buffer_unordered(DEPLOY_CONCURRENCY)
    .collect()
    .await;

    // Fail loud on any deployment error.
    for r in results {
        r?;
    }

    println!("All functions deployed.");
    Ok(sizes)
}
