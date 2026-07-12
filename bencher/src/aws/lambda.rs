//! Lambda function lifecycle: create/update, force-cold via env bump, invoke.

use super::Aws;
use crate::config::{Cell, SEED_KEY, Scenario, TABLE_NAME};
use anyhow::{Context, Result, bail};
use aws_sdk_lambda::operation::create_function::{CreateFunctionError, CreateFunctionOutput};
use aws_sdk_lambda::primitives::Blob;
use aws_sdk_lambda::types::{
    Architecture, Environment, FunctionCode, LastUpdateStatus, LogType, PackageType, Runtime,
    SnapStart, SnapStartApplyOn, SnapStartOptimizationStatus, State,
};
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
use aws_smithy_runtime_api::client::result::SdkError;
use std::collections::HashMap;
use std::time::Duration;

/// The result of a single Invoke, with the decoded log tail.
pub struct InvokeResult {
    pub status_code: i32,
    pub function_error: Option<String>,
    /// Raw decoded log tail (UTF-8).
    pub log_tail: String,
    /// The decoded response payload (the handler's return value, UTF-8). Used to
    /// validate the HTTP-envelope status of the Smithy-fronted scenarios, whose
    /// framework serializes an internal error as a 500 INSIDE this body while the
    /// Lambda invoke itself still returns normally. `None` if the invoke returned
    /// no payload.
    pub payload: Option<String>,
}

/// A Cell-free function specification for the synthetic download-scaling probe,
/// which deploys ephemeral padded-size functions that are NOT part of the matrix
/// (so they have no `Cell`). Carries only what `create_function_from_zip` needs.
pub struct SyntheticFn {
    pub name: String,
    pub runtime: &'static str,
    pub handler: &'static str,
    pub memory_mb: i32,
    pub arch: &'static str,
}

/// Zips at or above this size cannot be sent inline (the base64-encoded request
/// body would exceed Lambda's per-request limit), so they are staged via S3.
const INLINE_ZIP_LIMIT: usize = 45 * 1024 * 1024;

/// Where a function's deployment code comes from: an S3-staged zip, or an inline
/// zip. `ensure_function` decides this once and applies it to whichever builder
/// (Create's `FunctionCode` struct vs. Update's inline fields) the caller needs.
#[derive(Clone, Copy)]
enum CodeSource<'a> {
    S3(&'a str),
    Zip(&'a [u8]),
}

/// Shared poll budget for the two GetFunction readiness loops (`wait_ready` and
/// `wait_version_active`): 600 attempts x 500 ms = 300 s, matching boto3's
/// FunctionUpdated/FunctionActive waiters (delay 5 s x 60 attempts). A config
/// update's settle time is control-plane-bound and can spike well past a minute
/// under congestion; a tighter ceiling times out on normal-but-slow updates and
/// aborts the whole run. Kept as one pair so the two loops cannot drift apart.
const POLL_MAX_ATTEMPTS: u32 = 600;
const POLL_INTERVAL: Duration = Duration::from_millis(500);

fn poll_budget_secs() -> u32 {
    POLL_MAX_ATTEMPTS * POLL_INTERVAL.as_millis() as u32 / 1000
}

impl Aws {
    /// Creates the function if absent, or updates its code + base configuration
    /// if present. Idempotent across deploys. Waits until the function is ready.
    /// Large zips are uploaded to S3 and referenced, since they exceed the
    /// inline request-body limit.
    pub async fn ensure_function(&self, cell: &Cell, role_arn: &str, zip: &[u8]) -> Result<()> {
        let name = cell.function_name();

        // For oversized artifacts, stage the zip in S3 and reference it.
        let s3_key = if zip.len() >= INLINE_ZIP_LIMIT {
            Some(self.stage_code_in_s3(&name, zip).await?)
        } else {
            None
        };
        let code_source = match s3_key.as_deref() {
            Some(key) => CodeSource::S3(key),
            None => CodeSource::Zip(zip),
        };
        let function_code = || match code_source {
            CodeSource::S3(key) => FunctionCode::builder()
                .s3_bucket(self.bucket_name())
                .s3_key(key)
                .build(),
            CodeSource::Zip(z) => FunctionCode::builder()
                .zip_file(Blob::new(z.to_vec()))
                .build(),
        };

        // Any non-NotFound error is a real failure to surface.
        let exists = match self.lambda.get_function().function_name(&name).send().await {
            Ok(_) => true,
            Err(err) => super::not_found_as_none(
                err,
                |e| e.is_resource_not_found_exception(),
                format!("GetFunction probe for {name}"),
            )?
            .is_some(),
        };

        // The create/update calls below also draw on the 15 req/s control-plane
        // quota but are deliberately NOT gated by `control_plane_rate`: deploy is
        // a one-time bounded burst (DEPLOY_CONCURRENCY) that adaptive retry already absorbs.
        if exists {
            let upd = self
                .lambda
                .update_function_code()
                .function_name(&name)
                .architectures(Architecture::from(cell.arch.lambda_arch()));
            let upd = match code_source {
                CodeSource::S3(key) => upd.s3_bucket(self.bucket_name()).s3_key(key),
                CodeSource::Zip(z) => upd.zip_file(Blob::new(z.to_vec())),
            };
            // Capture the revision before each update so we can wait for *our*
            // change to land (a revision different from this one), not a stale
            // prior `Successful`. See `wait_ready`.
            let pre_code = self.current_revision(&name).await?;
            upd.send()
                .await
                .with_context(|| format!("updating code for {name}"))?;
            self.wait_ready(&name, pre_code.as_deref()).await?;

            let pre_cfg = self.current_revision(&name).await?;
            self.lambda
                .update_function_configuration()
                .function_name(&name)
                .runtime(Runtime::from(cell.lang.runtime()))
                .memory_size(cell.memory_mb)
                .timeout(30)
                .environment(self.environment(cell)?)
                .snap_start(snap_start_for(cell))
                .send()
                .await
                .with_context(|| format!("updating config for {name}"))?;
            self.wait_ready(&name, pre_cfg.as_deref()).await?;
        } else {
            // Build the environment once up front: a wiring error fails loud
            // here, and each create attempt reuses the same clonable value.
            let env = self.environment(cell)?;
            let create = || {
                self.lambda
                    .create_function()
                    .function_name(&name)
                    .runtime(Runtime::from(cell.lang.runtime()))
                    .role(role_arn)
                    .handler(cell.handler())
                    .code(function_code())
                    .architectures(Architecture::from(cell.arch.lambda_arch()))
                    .memory_size(cell.memory_mb)
                    .timeout(30)
                    .environment(env.clone())
                    .snap_start(snap_start_for(cell))
                    .publish(true)
                    .send()
            };
            retry_create_role_propagation(&name, create).await?;
            self.wait_ready(&name, None).await?;
        }
        Ok(())
    }

    /// Creates a Cell-free function from a raw zip (the synthetic download-scaling
    /// probe's ephemeral padded functions), always staging in S3 (they are large by
    /// construction) and returning the staged key for later teardown. Same
    /// role-propagation retry and `wait_ready` discipline as `ensure_function`'s
    /// create path, but with no environment or SnapStart. Assumes a fresh,
    /// run-unique name; a pre-existing one surfaces as a create conflict.
    ///
    /// The caller records the key only on success, so on any failure this method
    /// cleans up after itself (a create failure deletes the zip; a post-create
    /// `wait_ready` failure deletes both function and zip) before surfacing the error.
    pub async fn create_function_from_zip(
        &self,
        spec: &SyntheticFn,
        role_arn: &str,
        zip: &[u8],
    ) -> Result<String> {
        let s3_key = self.stage_code_in_s3(&spec.name, zip).await?;
        let code = FunctionCode::builder()
            .s3_bucket(self.bucket_name())
            .s3_key(&s3_key)
            .build();
        let create = || {
            self.lambda
                .create_function()
                .function_name(&spec.name)
                .runtime(Runtime::from(spec.runtime))
                .role(role_arn)
                .handler(spec.handler)
                .code(code.clone())
                .architectures(Architecture::from(spec.arch))
                .memory_size(spec.memory_mb)
                .timeout(30)
                .send()
        };

        // Create failed for good, but the staged zip is already in S3 and the
        // caller will never learn of it (records only on success), so clean it
        // up here before surfacing the error.
        if let Err(err) = retry_create_role_propagation(&spec.name, create).await {
            self.cleanup_staged_zip(&s3_key, &spec.name).await;
            return Err(err);
        }
        // The caller records this function only once THIS returns Ok, so a
        // `wait_ready` failure would orphan both it and the staged zip: tear both
        // down before propagating.
        if let Err(wait_err) = self.wait_ready(&spec.name, None).await {
            if let Err(fn_err) = self.delete_function(&spec.name).await {
                eprintln!(
                    "   WARNING: failed to delete {} after wait_ready failure: {fn_err:#}",
                    spec.name
                );
            }
            self.cleanup_staged_zip(&s3_key, &spec.name).await;
            return Err(wait_err);
        }
        Ok(s3_key)
    }

    /// Creates a Cell-free function from a container IMAGE (the synthetic
    /// download-scaling probe's image family). The image must already be pushed to
    /// ECR; `image_uri` is the full `<acct>.dkr.ecr.<region>.amazonaws.com/<repo>:<tag>`.
    /// When `touch` is set, the function carries `LAMBDABENCH_TOUCH=1`, which the
    /// baked-in handler reads at import time to fault in the padding blocks (the
    /// forced-download variant); otherwise it carries no environment (the
    /// lazy-loaded variant). The two variants deploy from the SAME image URI,
    /// differing only by this env var.
    ///
    /// Unlike the zip path, an image function sets NEITHER `runtime` NOR `handler`
    /// (both are zip-only; specifying a runtime for an image errors). The base
    /// image's ENTRYPOINT (the runtime interface client) plus its `CMD` carry the
    /// handler. Reuses the same role-propagation retry and `wait_ready` discipline
    /// as `create_function_from_zip`; `wait_ready`'s `State::Active` wait also
    /// covers the image-optimization phase (Pending -> Active) Lambda runs after
    /// the image is uploaded.
    ///
    /// Assumes the function does not already exist (fresh, run-unique names); a
    /// pre-existing name surfaces as a create conflict. On a `wait_ready` failure
    /// (e.g. a bad image reported as `Failed`), the created function is deleted
    /// before surfacing the error, since the caller records it for teardown only
    /// on success.
    pub async fn create_function_from_image(
        &self,
        name: &str,
        role_arn: &str,
        image_uri: &str,
        memory_mb: i32,
        arch: &'static str,
        touch: bool,
    ) -> Result<()> {
        // The touched variant reads padding.bin at init; the env var is what turns
        // that on. The untouched variant gets no env. `bump_cold_nonce` preserves
        // whatever env is here across every cold-force cycle, so the touch flag
        // survives sampling.
        let env = if touch {
            let mut vars = HashMap::new();
            vars.insert("LAMBDABENCH_TOUCH".to_string(), "1".to_string());
            Some(Environment::builder().set_variables(Some(vars)).build())
        } else {
            None
        };

        let code = FunctionCode::builder().image_uri(image_uri).build();
        let create = || {
            let mut req = self
                .lambda
                .create_function()
                .function_name(name)
                .package_type(PackageType::Image)
                .role(role_arn)
                .code(code.clone())
                .architectures(Architecture::from(arch))
                .memory_size(memory_mb)
                .timeout(30);
            if let Some(env) = &env {
                req = req.environment(env.clone());
            }
            req.send()
        };

        retry_create_role_propagation(name, create).await?;

        // `wait_ready` blocks until State::Active (post image-optimization) and
        // fails loud on a Failed image. On failure, delete the function we made so
        // the caller (which records it only on our success) does not orphan it.
        if let Err(wait_err) = self.wait_ready(name, None).await {
            if let Err(fn_err) = self.delete_function(name).await {
                eprintln!(
                    "   WARNING: failed to delete {name} after wait_ready failure: {fn_err:#}"
                );
            }
            return Err(wait_err);
        }
        Ok(())
    }

    /// Best-effort deletion of a staged code zip after a create/publish failure,
    /// so a partial failure does not orphan a large S3 object. Logs rather than
    /// propagates: the caller is already returning the real error.
    async fn cleanup_staged_zip(&self, s3_key: &str, fn_name: &str) {
        if let Err(cleanup_err) = self.delete_s3_object(s3_key).await {
            eprintln!(
                "   WARNING: failed to delete staged zip {s3_key} after a failed deploy of \
                 {fn_name}: {cleanup_err:#}"
            );
        }
    }

    /// Uploads a deployment zip to the benchmark S3 bucket under `lambda-code/`
    /// and returns the object key, for functions whose zip is too large to send
    /// inline.
    async fn stage_code_in_s3(&self, name: &str, zip: &[u8]) -> Result<String> {
        let key = format!("lambda-code/{name}.zip");
        self.s3
            .put_object()
            .bucket(self.bucket_name())
            .key(&key)
            .body(aws_sdk_s3::primitives::ByteStream::from(zip.to_vec()))
            .send()
            .await
            .with_context(|| format!("uploading code zip for {name} to s3"))?;
        Ok(key)
    }

    /// Builds the function environment: scenario DDB/S3/KMS wiring and quiet
    /// logging. The cold nonce is NOT set here; it is bumped on `$LATEST` at run
    /// time by `bump_cold_nonce`, which preserves the rest of this environment.
    ///
    /// Fails loud if a KMS-using scenario is wired before the KMS key id has been
    /// discovered, rather than deploying with a placeholder that would fail at
    /// runtime. `ensure_kms_key` must run before building such a function's env.
    fn environment(&self, cell: &Cell) -> Result<Environment> {
        let mut vars = HashMap::new();
        if cell.scenario.needs_ddb() {
            vars.insert("LAMBDABENCH_TABLE".to_string(), TABLE_NAME.to_string());
            vars.insert("LAMBDABENCH_KEY".to_string(), SEED_KEY.to_string());
        }
        // Any S3-reading scenario gets the bucket name. The IAM role grants
        // GetObject on the whole bucket, so this is the only wiring S3-only
        // (lettercount) functions need.
        if cell.scenario.needs_s3() {
            vars.insert("LAMBDABENCH_BUCKET".to_string(), self.bucket_name());
        }
        if cell.scenario == Scenario::LetterCount {
            // LetterCount reads its own large payload object; no KMS, no DDB.
            vars.insert(
                "LAMBDABENCH_LETTERCOUNT_KEY".to_string(),
                crate::config::S3_LETTERCOUNT_KEY.to_string(),
            );
        }
        if cell.scenario == Scenario::Batch {
            // Batch reads its own large batch object at init; no KMS, no DDB.
            vars.insert(
                "LAMBDABENCH_BATCH_KEY".to_string(),
                crate::config::S3_BATCH_KEY.to_string(),
            );
        }
        // Authz needs no env wiring: it receives the JWT in the invoke payload
        // and embeds the public verification key in the handler binary. No S3,
        // KMS, or DDB.
        if cell.scenario.needs_kms_s3() {
            // The KMS key id is discovered during deploy and stored on the Aws
            // bundle; it MUST be set before any KMS-using function is wired.
            // Fail loud rather than substituting a placeholder that would deploy
            // a function guaranteed to fail at runtime.
            let key_id = self.kms_key_id.get().cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "KMS key id not set while wiring {} (a KMS-using scenario); \
                     ensure_kms_key must run before building this function's environment",
                    cell.function_name()
                )
            })?;
            vars.insert("LAMBDABENCH_KMS_KEY_ID".to_string(), key_id);
            vars.insert(
                "LAMBDABENCH_OBJECT_KEY".to_string(),
                crate::config::S3_OBJECT_KEY.to_string(),
            );
            // Write targets for the smithyfull CreateOrder flow. Each function
            // writes its OWN receipt key / order PK (derived from the function
            // name), never a shared one: a shared key concentrates every
            // function's PUTs on one S3 object and draws `503 SlowDown`. The
            // writes stay idempotent and bounded (overwritten in place).
            //
            // The function name LEADS the S3 key, because S3 partitions on the
            // lexicographic leading prefix. A shared leading literal lands every
            // PUT on one cold partition until S3 splits it (minutes), 503ing under
            // burst meanwhile; leading with the high-cardinality name spreads the
            // writes from the start. (This is separate from the 2018 request-rate
            // change, which removed the need to randomize for STEADY-STATE
            // throughput but not the cold-partition burst problem.) DDB hashes the
            // whole PK, so only the S3 key order matters.
            let name = cell.function_name();
            vars.insert(
                "LAMBDABENCH_ORDER_PK".to_string(),
                format!("{}-{name}", crate::config::ORDER_PK),
            );
            vars.insert(
                "LAMBDABENCH_RECEIPT_KEY".to_string(),
                format!("{name}/{}", crate::config::S3_RECEIPT_KEY),
            );
        }
        // Quieter logs from the AWS SDKs / smithy server keep the tail focused
        // on the REPORT line.
        vars.insert("RUST_LOG".to_string(), "error".to_string());
        Ok(Environment::builder().set_variables(Some(vars)).build())
    }

    /// Forces the next invoke to be a cold start by bumping a `COLD_NONCE`
    /// environment variable, then waits until *this specific update* has landed
    /// (control-plane: `RevisionId` advanced, `LastUpdateStatus == Successful`).
    /// That is NOT a guarantee the old sandbox is gone: retirement is
    /// asynchronous on the data plane and cannot be observed here, so the caller
    /// MUST still verify the invoke actually landed cold and re-force on a warm
    /// hit (see `force_cold_invoke`'s retry loop in run.rs). Thin wrapper over
    /// `bump_cold_nonce`, which carries the revision-keying detail.
    pub async fn force_cold(&self, cell: &Cell, nonce: &str) -> Result<()> {
        self.force_cold_by_name(&cell.function_name(), nonce).await
    }

    /// Name-based [`force_cold`](Self::force_cold), for callers that hold a bare
    /// function name rather than a matrix `Cell` (the synthetic download-scaling
    /// probe). Same env-bump cold trigger.
    pub async fn force_cold_by_name(&self, name: &str, nonce: &str) -> Result<()> {
        self.bump_cold_nonce(name, nonce).await
    }

    /// Bumps ONLY the `COLD_NONCE` env var on `$LATEST` (preserving every other
    /// var) and waits until that update has landed (control-plane readiness only;
    /// the old sandbox retires asynchronously and is not awaited here, so callers
    /// forcing a cold start must still verify the invoke landed cold). Shared by
    /// `force_cold` and `publish_cold_version` (which bumps before publishing so
    /// the new version is not deduped).
    ///
    /// It fetches the current environment and revision, then bumps only the nonce.
    /// It must NOT rebuild the environment from scratch (`UpdateFunctionConfiguration`
    /// replaces the whole Environment): the run phase lacks deploy-time state like
    /// the KMS key id, so re-deriving would clobber correct values with placeholders.
    ///
    /// See `wait_ready` for why waiting keys on the `RevisionId` changing, not just
    /// on `LastUpdateStatus == Successful`.
    async fn bump_cold_nonce(&self, name: &str, nonce: &str) -> Result<()> {
        let out = self
            .lambda
            .get_function()
            .function_name(name)
            .send()
            .await
            .with_context(|| format!("GetFunction (bump_cold_nonce) for {name}"))?;
        let cfg = out
            .configuration()
            .with_context(|| format!("GetFunction for {name} returned no configuration"))?;
        let pre = cfg.revision_id().map(|s| s.to_string());
        let mut vars: HashMap<String, String> = cfg
            .environment()
            .and_then(|e| e.variables())
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        vars.insert("COLD_NONCE".to_string(), nonce.to_string());
        let nonce_only_env = Environment::builder().set_variables(Some(vars)).build();
        // The Update is the only call here on the scarce 15 req/s "remainder"
        // quota, so gate it behind the control-plane rate limiter. The readiness
        // poll below runs on GetFunction's separate quota and is not gated.
        self.control_plane_rate.acquire().await;
        self.lambda
            .update_function_configuration()
            .function_name(name)
            .environment(nonce_only_env)
            .send()
            .await
            .with_context(|| format!("bumping COLD_NONCE for {name}"))?;
        self.wait_ready(name, pre.as_deref()).await?;
        Ok(())
    }

    /// Produces a deterministically-cold SnapStart sample target: publishes a fresh
    /// function version and returns its number. SnapStart applies only to published
    /// versions, and the snapshot is created at publish time, so a brand-new
    /// version's first invoke is guaranteed to be a cold restore, with no
    /// warm-sandbox retry loop (unlike the `$LATEST` env-bump path in `force_cold`).
    ///
    /// `COLD_NONCE` is bumped on `$LATEST` first so the config change makes
    /// `PublishVersion` mint a new version rather than dedupe an unchanged one. Then
    /// it waits for the published version to reach `State::Active` (snapshot ready).
    /// The caller must `delete_version` the returned version to reclaim storage.
    pub async fn publish_cold_version(&self, cell: &Cell, nonce: &str) -> Result<String> {
        let name = cell.function_name();

        // Bump COLD_NONCE on $LATEST (preserving every other env var), so the
        // published version carries a changed config and is not deduped. Same
        // mechanism as the non-SnapStart cold trigger.
        self.bump_cold_nonce(&name, nonce).await?;

        // Publish a new version (it then transitions Pending -> Active as its
        // snapshot is created). PublishVersion is on the same scarce 15 req/s quota
        // as the Update above, so gate it behind the rate limiter too.
        self.control_plane_rate.acquire().await;
        let published = self
            .lambda
            .publish_version()
            .function_name(&name)
            .send()
            .await
            .with_context(|| format!("publishing version for {name}"))?;
        let version = published
            .version()
            .filter(|v| *v != "$LATEST")
            .with_context(|| format!("PublishVersion for {name} returned no numeric version"))?
            .to_string();

        self.wait_version_active(&name, &version).await?;
        Ok(version)
    }

    /// Polls a specific function *version* until `State::Active` (snapshot ready)
    /// or `Failed`. Versions are immutable, so unlike `wait_ready` there is no
    /// revision/LastUpdateStatus race to guard against.
    ///
    /// Once active, it asserts `SnapStartOptimizationStatus::On`: a non-SnapStart
    /// version also reaches Active, so a state-only check would miss SnapStart
    /// silently failing to apply (which would then cold-start with `Init Duration`
    /// instead of `Restore Duration`). Failing here catches it at publish rather
    /// than producing a mislabeled sample at invoke.
    async fn wait_version_active(&self, name: &str, version: &str) -> Result<()> {
        for _ in 0..POLL_MAX_ATTEMPTS {
            let out = self
                .lambda
                .get_function()
                .function_name(name)
                .qualifier(version)
                .send()
                .await
                .with_context(|| format!("GetFunction {name}:{version}"))?;
            let cfg = out.configuration().with_context(|| {
                format!("GetFunction {name}:{version} returned no configuration")
            })?;
            match cfg.state() {
                Some(State::Active) => {
                    // Confirm the snapshot is actually applied; anything but On
                    // means SnapStart did not take on this version.
                    let opt = cfg.snap_start().and_then(|s| s.optimization_status());
                    if opt != Some(&SnapStartOptimizationStatus::On) {
                        bail!(
                            "version {name}:{version} is Active but SnapStart is not applied \
                             (optimization_status={opt:?}); a restore would not occur"
                        );
                    }
                    return Ok(());
                }
                Some(State::Failed) => bail!(
                    "version {name}:{version} entered Failed state: {}",
                    cfg.state_reason().unwrap_or("unknown")
                ),
                _ => {}
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        bail!(
            "version {name}:{version} did not become Active within {}s",
            poll_budget_secs()
        )
    }

    /// Deletes a single published function version to reclaim code storage. Used by
    /// the SnapStart run path after each cold cycle so a run that publishes many
    /// versions stays bounded. Best-effort: on failure the caller logs and continues
    /// without failing the cell. On the scarce 15 req/s quota, so gated behind the
    /// rate limiter like the other run-loop control-plane calls.
    pub async fn delete_version(&self, name: &str, version: &str) -> Result<()> {
        self.control_plane_rate.acquire().await;
        self.lambda
            .delete_function()
            .function_name(name)
            .qualifier(version)
            .send()
            .await
            .with_context(|| format!("deleting version {name}:{version}"))?;
        Ok(())
    }

    /// Returns the function's current `RevisionId`, used as the "before" marker
    /// when waiting for an update to land. Uses `GetFunction` (its own 100 req/s
    /// quota) rather than `GetFunctionConfiguration` (the scarce 15 req/s
    /// remainder bucket the Update calls also draw from).
    async fn current_revision(&self, name: &str) -> Result<Option<String>> {
        let out = self
            .lambda
            .get_function()
            .function_name(name)
            .send()
            .await
            .with_context(|| format!("GetFunction (pre-update) for {name}"))?;
        Ok(out
            .configuration()
            .and_then(|c| c.revision_id())
            .map(|s| s.to_string()))
    }

    /// Polls until the function is Active and its last update Successful, erroring
    /// on a failed update. Polls `GetFunction` (its own 100 req/s quota), NOT
    /// `GetFunctionConfiguration` (the scarce 15 req/s bucket the Updates draw
    /// from); both carry the same `RevisionId`/`State`/`LastUpdateStatus`, so this
    /// keeps almost all of the 15/s budget free for the Updates.
    ///
    /// When `prev_revision` is supplied, the observed `RevisionId` must also DIFFER
    /// from it, so we wait for a new update to land rather than accept a stale prior
    /// `Successful` (see `force_cold`). `None` (initial create) accepts any
    /// settled-and-active state.
    async fn wait_ready(&self, name: &str, prev_revision: Option<&str>) -> Result<()> {
        for _ in 0..POLL_MAX_ATTEMPTS {
            let out = self
                .lambda
                .get_function()
                .function_name(name)
                .send()
                .await
                .with_context(|| format!("GetFunction for {name}"))?;
            let cfg = out
                .configuration()
                .with_context(|| format!("GetFunction for {name} returned no configuration"))?;

            let state = cfg.state();
            let upd = cfg.last_update_status();

            if matches!(upd, Some(LastUpdateStatus::Failed)) {
                bail!(
                    "function {name} update failed: {}",
                    cfg.last_update_status_reason().unwrap_or("unknown")
                );
            }
            let active = matches!(state, Some(State::Active));
            // Mirror boto3's waiters: an update is settled at LastUpdateStatus
            // == Successful (FunctionUpdated). On create (no prev_revision) the
            // primary signal is State == Active (FunctionActive); a just-created
            // function may briefly report no LastUpdateStatus, so accept that
            // only on the create path, never on an update.
            let settled = matches!(upd, Some(LastUpdateStatus::Successful))
                || (prev_revision.is_none() && upd.is_none());
            // If we know the pre-update revision, the observed revision must have
            // moved past it; otherwise we may be reading the prior (already
            // settled) revision before our update has registered.
            let revision_advanced = match prev_revision {
                Some(prev) => cfg.revision_id() != Some(prev),
                None => true,
            };
            if active && settled && revision_advanced {
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        bail!(
            "function {name} did not become ready within {}s",
            poll_budget_secs()
        )
    }

    /// Invokes the function once with `LogType=Tail`, returning status, any
    /// function error, and the decoded log tail. `Some(qualifier)` invokes a
    /// specific version (the SnapStart path); `None` invokes `$LATEST`.
    ///
    /// `LogType=Tail` returns only the last 4 KB of logs. The `REPORT` line is last
    /// and short, so it survives the cap; handlers keep logging minimal (Rust sets
    /// `RUST_LOG=error`) so a chatty invoke cannot push it out, and if it ever did,
    /// `parse_report` fails loud rather than guessing a timing.
    pub async fn invoke_tail(&self, cell: &Cell, qualifier: Option<&str>) -> Result<InvokeResult> {
        self.invoke_tail_timed(cell, qualifier)
            .await
            .map(|(r, _)| r)
    }

    /// Like [`invoke_tail`](Self::invoke_tail) but also returns caller-side
    /// wall-clock around ONLY the `Invoke` `send()`, not the decode or fail-loud
    /// checks after it. The matrix never records this value (it uses in-Lambda
    /// REPORT timings); it exists for the documentation probe, which isolates the
    /// pre-Init download+start cost that only the caller's wall-clock can see.
    pub async fn invoke_tail_timed(
        &self,
        cell: &Cell,
        qualifier: Option<&str>,
    ) -> Result<(InvokeResult, Duration)> {
        self.invoke_tail_timed_with(&self.lambda, cell, qualifier)
            .await
    }

    /// [`invoke_tail_timed`](Self::invoke_tail_timed) through a caller-supplied
    /// Lambda client. The driver's client keeps adaptive retry, which would absorb
    /// a throttled `Invoke` into an inflated wall-clock; the probe passes a
    /// retry-DISABLED client so a throttle fails loud instead. Decode + fail-loud
    /// REPORT handling stays shared with the non-timed path.
    pub async fn invoke_tail_timed_with(
        &self,
        client: &aws_sdk_lambda::Client,
        cell: &Cell,
        qualifier: Option<&str>,
    ) -> Result<(InvokeResult, Duration)> {
        self.invoke_tail_timed_by_name(
            client,
            &cell.function_name(),
            &invoke_payload(cell),
            qualifier,
        )
        .await
    }

    /// Name-based [`invoke_tail_timed_with`](Self::invoke_tail_timed_with): times a
    /// `LogType=Tail` invoke of a bare function name with a caller-supplied payload.
    /// The Cell variant delegates here after computing its scenario payload; the
    /// synthetic probe calls it directly with a `{}` payload.
    pub async fn invoke_tail_timed_by_name(
        &self,
        client: &aws_sdk_lambda::Client,
        name: &str,
        payload: &str,
        qualifier: Option<&str>,
    ) -> Result<(InvokeResult, Duration)> {
        let mut req = client
            .invoke()
            .function_name(name)
            .log_type(LogType::Tail)
            .payload(Blob::new(payload.as_bytes().to_vec()));
        if let Some(q) = qualifier {
            req = req.qualifier(q);
        }
        // Time only the round trip; the decode + validation below is local CPU
        // work that would otherwise pollute the measurement.
        let start = std::time::Instant::now();
        let out = req
            .send()
            .await
            .with_context(|| format!("invoking {name}"))?;
        let elapsed = start.elapsed();

        let status_code = out.status_code();
        let function_error = out.function_error().map(|s| s.to_string());
        let payload = out
            .payload()
            .map(|b| String::from_utf8_lossy(b.as_ref()).into_owned());

        let log_tail = match out.log_result() {
            Some(b64) => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .context("decoding base64 log tail")?;
                String::from_utf8_lossy(&bytes).into_owned()
            }
            // A missing tail is normally a hard error (we asked for LogType=Tail),
            // but a failed invoke (FunctionError or non-200) is the real cause, so
            // surface that instead of masking it with the tail-absence message.
            None => match &function_error {
                Some(err) => bail!(
                    "invoke of {name} reported FunctionError={err} (status {status_code}) and returned no LogResult"
                ),
                None if status_code != 200 => bail!(
                    "invoke of {name} returned status {status_code} (expected 200) and no LogResult"
                ),
                None => bail!("invoke of {name} returned no LogResult (LogType=Tail expected)"),
            },
        };

        Ok((
            InvokeResult {
                status_code,
                function_error,
                log_tail,
                payload,
            },
            elapsed,
        ))
    }

    /// Whether a function exists, by exact name. Returns `false` only on
    /// `ResourceNotFound`; any other error (throttle / access-denied / network) is
    /// surfaced so callers fail loud rather than mistaking it for absence. Used by
    /// the `--skip-deploy` preflight.
    pub async fn function_exists(&self, name: &str) -> Result<bool> {
        match self.lambda.get_function().function_name(name).send().await {
            Ok(_) => Ok(true),
            Err(err) => Ok(super::not_found_as_none(
                err,
                |e| e.is_resource_not_found_exception(),
                format!("GetFunction existence check for {name}"),
            )?
            .is_some()),
        }
    }

    /// Deletes a benchmark function (used by teardown). Treats a missing function
    /// as success (idempotent) but surfaces any other error, so teardown can report
    /// deletions that actually succeeded rather than attempts.
    pub async fn delete_function(&self, name: &str) -> Result<()> {
        match self
            .lambda
            .delete_function()
            .function_name(name)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => super::not_found_as_none(
                err,
                |e| e.is_resource_not_found_exception(),
                format!("deleting function {name}"),
            )
            .map(|_| ()),
        }
    }
}

/// Retries a `CreateFunction` call while Lambda reports the IAM role as not yet
/// assumable, then wraps any other (or exhausted) failure as `"creating {name}"`.
/// A freshly created IAM role is not immediately assumable by Lambda; CreateFunction
/// validates the role and, on the first deploy after role creation, frequently fails
/// with `InvalidParameterValue: The role defined for the function cannot be assumed
/// by Lambda`. The SDK's adaptive retry does not cover that error class, so retry it
/// here with backoff until IAM propagation catches up.
async fn retry_create_role_propagation<F, Fut>(name: &str, create: F) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<CreateFunctionOutput, SdkError<CreateFunctionError, HttpResponse>>>,
{
    const ROLE_PROPAGATION_MAX_ATTEMPTS: u32 = 30;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        return match create().await {
            Ok(_) => Ok(()),
            Err(err) => {
                let svc = err.into_service_error();
                // Match the role-not-ready error precisely: typed error class
                // first, message substring second. Either signal alone is
                // fragile (the message can be reworded; the typed class also
                // covers many unrelated parameter errors), so require both, and
                // if the typed class matches but the message no longer does,
                // fall through to surface the error rather than silently
                // retrying a real bug.
                let assume_role_not_ready = svc.is_invalid_parameter_value_exception()
                    && svc
                        .meta()
                        .message()
                        .map(|m| m.contains("cannot be assumed by Lambda"))
                        .unwrap_or(false);
                if assume_role_not_ready && attempt < ROLE_PROPAGATION_MAX_ATTEMPTS {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                Err(anyhow::Error::new(svc).context(format!("creating {name}")))
            }
        };
    }
}

/// The SnapStart configuration to apply to a cell. SnapStart-enabled cells turn
/// it on for published versions; every other cell explicitly turns it off (so a
/// re-deploy that flips the flag clears a stale `PublishedVersions` setting).
fn snap_start_for(cell: &Cell) -> SnapStart {
    let apply_on = if cell.snapstart {
        SnapStartApplyOn::PublishedVersions
    } else {
        SnapStartApplyOn::None
    };
    SnapStart::builder().apply_on(apply_on).build()
}

/// The invocation payload for a cell. The `smithy`/`smithyfull` scenarios are
/// fronted by a Smithy server SDK whose Lambda adapter dictates the event shape:
/// Rust/Node use the AWS Smithy apigateway adapter (API Gateway **v2** HTTP
/// event), while Java's smithy-java `LambdaEndpoint` expects the API Gateway
/// **v1** proxy event (`path`/`httpMethod`/`body`). All other scenarios take a
/// plain JSON object regardless of language.
///
/// `pub(crate)` so the probe's shared name-based `take_sample` (which no longer
/// holds a `Cell`) can compute the same payload before invoking by name.
pub(crate) fn invoke_payload(cell: &Cell) -> String {
    use crate::config::Lang;
    match (cell.scenario, cell.lang) {
        // --- Java smithy scenarios: API Gateway v1 proxy event ---
        // `LambdaEndpoint.ProxyRequest` deserializes path/httpMethod/body/
        // isBase64Encoded and `multiValueHeaders` (NOT a single-value `headers`
        // field, which it ignores), so headers are sent as multi-value arrays.
        (Scenario::Smithy, Lang::Java) => r#"{
  "path": "/menu",
  "httpMethod": "GET",
  "multiValueHeaders": { "accept": ["application/json"] },
  "isBase64Encoded": false
}"#
        .to_string(),
        (Scenario::SmithyFull, Lang::Java) => r#"{
  "path": "/order",
  "httpMethod": "POST",
  "multiValueHeaders": { "content-type": ["application/json"], "accept": ["application/json"] },
  "body": "{\"coffeeType\":\"LATTE\"}",
  "isBase64Encoded": false
}"#
        .to_string(),
        // --- Rust/Node smithy scenarios: API Gateway v2 HTTP event ---
        // `smithy` (framework only) routes a no-input GET /menu.
        (Scenario::Smithy, _) => r#"{
  "version": "2.0",
  "routeKey": "GET /menu",
  "rawPath": "/menu",
  "rawQueryString": "",
  "headers": { "accept": "application/json" },
  "requestContext": {
    "http": { "method": "GET", "path": "/menu", "protocol": "HTTP/1.1", "sourceIp": "127.0.0.1" }
  },
  "isBase64Encoded": false
}"#
        .to_string(),
        // `smithyfull` (realistic) routes POST /order with a real JSON body, so
        // the SSDK deserializes + validates the input and serializes a
        // constrained response, exercising the framework's actual work.
        (Scenario::SmithyFull, _) => r#"{
  "version": "2.0",
  "routeKey": "POST /order",
  "rawPath": "/order",
  "rawQueryString": "",
  "headers": { "content-type": "application/json", "accept": "application/json" },
  "requestContext": {
    "http": { "method": "POST", "path": "/order", "protocol": "HTTP/1.1", "sourceIp": "127.0.0.1" }
  },
  "body": "{\"coffeeType\":\"LATTE\"}",
  "isBase64Encoded": false
}"#
        .to_string(),
        // `authz` receives the JWT in the invoke payload, as a real authorizer
        // does. The token is a build-time fixture (gitignored) signed by the same
        // key the handlers embed the public half of. Interpolated raw because a
        // compact JWT contains no characters needing JSON escaping.
        (Scenario::Authz, _) => format!(r#"{{"token":"{}"}}"#, AUTHZ_TOKEN.trim()),
        // Non-HTTP scenarios take a plain JSON object.
        _ => "{}".to_string(),
    }
}

/// The signed RS256 JWT sent in every `authz` invoke payload. Build-time-generated
/// fixture (gitignored, see `bencher/fixtures/README.md`); a compact JWT is URL-safe
/// base64 with two dots, so it needs no JSON escaping.
const AUTHZ_TOKEN: &str = include_str!("../../fixtures/authz_token.txt");
