//! Teardown: deletes every benchmark function, its CloudWatch log groups, the
//! execution role, and the table. Only targets resources whose EXACT names the
//! project defines: the function/log-group sets are reconstructed from config
//! (`all_managed_function_names` / `all_managed_log_group_names`), and the role,
//! table, bucket, KMS key, and ECR repo by their fixed names. It never lists the
//! account and deletes by prefix, so a stranger's function that merely shares the
//! `lambdabench-` stem is never caught.
//!
//! Trade-off: a function whose matrix cell is not in the current config (a
//! language/scenario/memory tier absent from `all_cells`) is not in the
//! reconstructed set, so delete such orphans by hand after a matrix trim. The
//! synthetic probe has no such gap: its size set is a fixed const, so teardown
//! enumerates every name the probe can create.

use crate::aws::Aws;
use crate::config;
use anyhow::{Result, bail};
use futures::stream::{self, StreamExt};

const TEARDOWN_CONCURRENCY: usize = 8;

/// Deletes all benchmark resources. Caller is responsible for confirmation.
///
/// Best-effort across resource classes: a failure deleting one resource (or some
/// functions) does NOT abort the rest, so a single transient error cannot leave
/// the still-billing table, bucket, or KMS key behind. Every failure is collected
/// and reported, and teardown returns `Err` at the end if anything failed.
///
/// Functions are enumerated from config (`all_managed_function_names`: the full
/// matrix plus the synthetic probe's default-size families) and deleted by exact
/// name, so teardown only touches names the project defines.
///
/// `DeleteFunction` is on Lambda's 15 req/s "remainder" control-plane quota
/// (shared across all control-plane APIs, not per-API, not raisable), and this
/// pass issues one call per managed name, most of them fast `NotFound` no-ops for
/// never-deployed cells and ephemeral synthetic functions. Fast calls are exactly
/// the case a concurrency cap does not bound the rate for, so each delete's START
/// is gated on the shared control-plane rate limiter (`CONTROL_PLANE_TPS`, as the
/// run loop's `delete_version` uses); `TEARDOWN_CONCURRENCY` then caps how many
/// run in flight, mirroring the run loop's semaphore + rate-limiter pairing.
pub async fn teardown(aws: &Aws) -> Result<()> {
    let names = config::all_managed_function_names();
    println!("Deleting {} functions...", names.len());
    let results: Vec<Result<()>> = stream::iter(names.iter().map(|name| {
        let aws = aws.clone();
        let name = name.clone();
        async move {
            aws.control_plane_rate.acquire().await;
            aws.delete_function(&name).await
        }
    }))
    .buffer_unordered(TEARDOWN_CONCURRENCY)
    .collect()
    .await;
    let mut failures: Vec<String> = Vec::new();
    let mut deleted = 0usize;
    for result in &results {
        match result {
            Ok(()) => deleted += 1,
            Err(e) => failures.push(format!("function delete: {e:#}")),
        }
    }
    println!("  deleted {deleted}/{} functions", names.len());

    // Lambda leaves the `/aws/lambda/<function>` log groups behind on function
    // delete, so remove them explicitly or every run leaks one group per cell.
    // Same exact-name set as the functions above, under `/aws/lambda/`.
    let log_group_names = config::all_managed_log_group_names();
    match aws.delete_function_log_groups(&log_group_names).await {
        Ok(summary) => {
            println!("  deleted {}/{} log groups", summary.deleted, summary.total);
            failures.extend(summary.failures);
        }
        Err(e) => {
            println!("  [FAILED] log group delete: {e:#}");
            failures.push(format!("log group delete: {e:#}"));
        }
    }

    // Each resource delete runs regardless of earlier failures.
    let steps: [(&str, Result<()>); 5] = [
        ("IAM role", aws.delete_role().await),
        ("DynamoDB table", aws.delete_table().await),
        ("S3 bucket + object", aws.delete_s3().await),
        (
            "KMS key (scheduled, 7-day window) + alias",
            aws.delete_kms_key().await,
        ),
        // Reclaim the synthetic image family's ECR repo by its exact name (not a
        // prefix sweep), so the CDK-managed `lambdabench-runner` repo, which
        // shares the stem, is never touched.
        ("ECR repository", aws.delete_ecr_repo().await),
    ];
    for (label, result) in steps {
        match result {
            Ok(()) => println!("Deleted {label}."),
            Err(e) => {
                println!("  [FAILED] {label}: {e:#}");
                failures.push(format!("{label}: {e:#}"));
            }
        }
    }

    if failures.is_empty() {
        println!("Teardown complete.");
        Ok(())
    } else {
        bail!(
            "teardown incomplete: {} resource(s) failed to delete:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }
}
