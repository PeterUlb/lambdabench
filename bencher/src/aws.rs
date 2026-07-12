//! AWS client bundle and shared helpers.
//!
//! All AWS access in the bencher goes through the clients constructed here,
//! pinned to the benchmark's region so an ambient environment can never change
//! the deployment target.

pub mod ddb;
pub mod ecr;
pub mod iam;
pub mod kms;
pub mod lambda;
pub mod logs;
mod rate_limiter;
pub mod s3;

pub use rate_limiter::RateLimiter;

use crate::config::{REGION, S3_BUCKET_SUFFIX};
use anyhow::{Context, Result};
use aws_config::retry::RetryConfig;
use aws_config::{BehaviorVersion, Region};
use aws_smithy_runtime_api::client::result::{CreateUnhandledError, SdkError};
use std::fmt::Debug;
use std::time::Duration;

/// Classifies an `SdkError` against a caller-supplied "is this not-found?"
/// predicate, the shared discipline for the bencher's idempotent operations: a
/// not-found result is success (resource already gone, or not yet created), while
/// any other error is surfaced with `context` rather than collapsed into
/// "missing". Returns `Ok(None)` for not-found and `Err(_)` otherwise, so a probe
/// can map `None`/`Some` to a bool and a delete can map `None` to `Ok(())`.
///
/// Each operation's error type exposes its own not-found check (e.g.
/// `is_resource_not_found_exception`, `is_no_such_entity_exception`,
/// `is_not_found`), so the predicate is passed in rather than hard-coded.
pub fn not_found_as_none<E, R, F>(
    err: SdkError<E, R>,
    is_not_found: F,
    context: impl Into<String>,
) -> Result<Option<E>>
where
    E: std::error::Error + Send + Sync + CreateUnhandledError + 'static,
    R: Debug + Send + Sync + 'static,
    F: FnOnce(&E) -> bool,
{
    let svc = err.into_service_error();
    if is_not_found(&svc) {
        Ok(None)
    } else {
        Err(anyhow::Error::new(svc).context(context.into()))
    }
}

/// Issue-rate ceiling for calls against the scarce 15 req/s "remainder"
/// control-plane quota, below it with headroom. Bounds the aggregate issue rate
/// directly, regardless of concurrency or how fast individual calls happen to be.
/// Polling in `wait_ready` is moved onto `GetFunction` (its own 100 req/s quota),
/// so the calls this gates are `UpdateFunctionConfiguration` (per cold-force) plus
/// `PublishVersion`/`DeleteFunction` on the SnapStart path.
pub const CONTROL_PLANE_TPS: u32 = 12;

/// CloudWatch Logs `DeleteLogGroup` is capped at 10 TPS per region, and teardown
/// issues one call per managed name (`config::all_managed_log_group_names`,
/// hundreds on a full matrix, most of them no-op deletes of never-created
/// groups). This is the paced issue rate, below 10 with headroom for jitter and
/// SDK retry.
pub const LOG_GROUP_DELETE_TPS: u32 = 8;

/// Bundle of region-pinned AWS service clients used across the driver.
#[derive(Clone)]
pub struct Aws {
    pub lambda: aws_sdk_lambda::Client,
    pub iam: aws_sdk_iam::Client,
    pub ddb: aws_sdk_dynamodb::Client,
    pub kms: aws_sdk_kms::Client,
    pub s3: aws_sdk_s3::Client,
    pub ecr: aws_sdk_ecr::Client,
    pub logs: aws_sdk_cloudwatchlogs::Client,
    pub account_id: String,
    /// KMS key id for the three_client scenario, ensured during deploy and
    /// shared with the env-building / cold-trigger paths. Set once.
    pub kms_key_id: std::sync::Arc<std::sync::OnceLock<String>>,
    /// Shared rate ceiling across ALL control-plane calls. Lambda's 15/s
    /// "remainder" quota is shared across APIs, not per-API, so a single limiter
    /// spans `UpdateFunctionConfiguration`, `PublishVersion`, and `DeleteFunction`.
    /// Held across all clones so the cap is global to the run, not per-cell.
    pub control_plane_rate: std::sync::Arc<RateLimiter>,
    /// Published SnapStart versions whose owning cycle was cancelled before its
    /// inline `delete_version` could run. Drop guards push `(function_name,
    /// version)` here synchronously (Drop cannot await), and the run loop drains
    /// it on the way out, so a fail-fast cancellation never leaks code storage
    /// without relying on a detached `tokio::spawn` racing runtime shutdown.
    pub leaked_versions: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

impl Aws {
    /// Loads credentials from the environment and constructs all clients,
    /// resolving the caller's account id up front (also validates creds work).
    pub async fn load() -> Result<Self> {
        // The benchmark leans on Lambda's control plane: every cold-force does an
        // UpdateFunctionConfiguration (on the account-wide, unraisable 15 req/s
        // "remainder" quota) plus several readiness polls (moved onto
        // GetFunction's separate 100 req/s quota; see wait_ready). The scarce
        // control-plane calls are gated by a shared rate limiter
        // (CONTROL_PLANE_TPS) that caps the aggregate issue rate directly, so a
        // large invoke pool never bursts past it. Adaptive retry is the final
        // backstop: it backs off and retries throttled calls and proactively
        // slows our request rate when it detects throttling. Generous
        // attempts/backoff so a multi-hour run is never killed by a transient
        // rate-exceeded.
        let retry = RetryConfig::adaptive()
            .with_max_attempts(10)
            .with_initial_backoff(Duration::from_millis(500))
            .with_max_backoff(Duration::from_secs(20));
        let conf = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(REGION))
            .retry_config(retry)
            .load()
            .await;

        let sts = aws_sdk_sts::Client::new(&conf);
        let ident = sts
            .get_caller_identity()
            .send()
            .await
            .context("sts:GetCallerIdentity failed (are credentials valid and unexpired?)")?;
        let account_id = ident
            .account()
            .context("GetCallerIdentity returned no account")?
            .to_string();

        Ok(Self {
            lambda: aws_sdk_lambda::Client::new(&conf),
            iam: aws_sdk_iam::Client::new(&conf),
            ddb: aws_sdk_dynamodb::Client::new(&conf),
            kms: aws_sdk_kms::Client::new(&conf),
            s3: aws_sdk_s3::Client::new(&conf),
            ecr: aws_sdk_ecr::Client::new(&conf),
            logs: aws_sdk_cloudwatchlogs::Client::new(&conf),
            account_id,
            kms_key_id: std::sync::Arc::new(std::sync::OnceLock::new()),
            control_plane_rate: std::sync::Arc::new(RateLimiter::per_second(CONTROL_PLANE_TPS)),
            leaked_versions: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }

    /// A Lambda client sharing this bundle's resolved config (region, credentials,
    /// endpoint) but with retries DISABLED. The documentation probe (`probe.rs`)
    /// issues its timed invokes through this so a throttled `Invoke` fails loud
    /// instead of being retried into an inflated wall-clock (see DESIGN.md
    /// Measurement purity #1). The bundle's own `lambda` client keeps adaptive
    /// retry for the control-plane cold-force mechanism, a different layer where
    /// retry is correct.
    pub fn retryless_lambda_client(&self) -> aws_sdk_lambda::Client {
        let conf = self
            .lambda
            .config()
            .to_builder()
            .retry_config(RetryConfig::disabled())
            .build();
        aws_sdk_lambda::Client::from_conf(conf)
    }

    /// The globally-unique S3 bucket name (suffix + region + account id). The
    /// region is in the name because S3 bucket names are global and a name freed
    /// by deleting a bucket stays locked for a while, so a same-name CreateBucket
    /// in another region would fail. Keying on region lets the benchmark move
    /// regions cleanly.
    pub fn bucket_name(&self) -> String {
        format!("{}-{}-{}", S3_BUCKET_SUFFIX, REGION, self.account_id)
    }
}
