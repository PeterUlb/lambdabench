//! CloudWatch Logs cleanup for the benchmark functions.
//!
//! Lambda auto-creates a log group named `/aws/lambda/<function-name>` on a
//! function's first invoke, and does NOT remove it when the function is deleted.
//! Teardown must delete the groups explicitly, or every run leaks one per cell
//! (each holding still-billing retained log data). The names are the managed
//! function names under `/aws/lambda/` (`config::all_managed_log_group_names`), so
//! teardown deletes them by exact name rather than listing
//! `/aws/lambda/lambdabench-*`.

use super::{Aws, LOG_GROUP_DELETE_TPS, RateLimiter};
use anyhow::Result;

impl Aws {
    /// Deletes a single log group. Treats a missing group as success
    /// (idempotent), but surfaces any other error so teardown can report how
    /// many deletions actually succeeded.
    async fn delete_log_group(&self, name: &str) -> Result<()> {
        match self
            .logs
            .delete_log_group()
            .log_group_name(name)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => super::not_found_as_none(
                err,
                |e| e.is_resource_not_found_exception(),
                format!("deleting log group {name}"),
            )
            .map(|_| ()),
        }
    }

    /// Deletes the given Lambda log groups, paced under CloudWatch's 10 TPS
    /// `DeleteLogGroup` limit (`LOG_GROUP_DELETE_TPS`). Callers pass the exact
    /// group names (`config::all_managed_log_group_names`); a group that was never
    /// created (function never invoked) is treated as success. Returns the number
    /// deleted alongside the collected per-group failures for the caller to fold
    /// into the teardown outcome. Best-effort: a single failure does not abort the
    /// rest.
    pub async fn delete_function_log_groups(&self, names: &[String]) -> Result<DeleteSummary> {
        let mut summary = DeleteSummary {
            total: names.len(),
            deleted: 0,
            failures: Vec::new(),
        };
        // Teardown owns this whole pass, so a fresh limiter (not a shared field):
        // its slot state should not persist beyond the sweep.
        let limiter = RateLimiter::per_second(LOG_GROUP_DELETE_TPS);
        for name in names {
            limiter.acquire().await;
            match self.delete_log_group(name).await {
                Ok(()) => summary.deleted += 1,
                Err(e) => summary.failures.push(format!("log group delete: {e:#}")),
            }
        }
        Ok(summary)
    }
}

/// Outcome of a log-group delete pass: how many groups were targeted, how many
/// were deleted, and the human-readable failures for the rest.
pub struct DeleteSummary {
    pub total: usize,
    pub deleted: usize,
    pub failures: Vec<String>,
}
