//! IAM execution role management for the benchmarked functions.
//!
//! The role trusts Lambda, can write CloudWatch logs, and can read the single
//! benchmark DynamoDB item. Creation is idempotent.

use super::Aws;
use crate::config::{INLINE_POLICY_NAME, PREFIX, ROLE_NAME, TABLE_NAME};
use anyhow::{Context, Result};

const TRUST_POLICY: &str = r#"{
  "Version": "2012-10-17",
  "Statement": [
    { "Effect": "Allow", "Principal": { "Service": "lambda.amazonaws.com" }, "Action": "sts:AssumeRole" }
  ]
}"#;

const BASIC_EXEC_POLICY_ARN: &str =
    "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole";

/// Env var carrying the ARN of the managed POLICY to set as the permissions
/// boundary on the execution role. The hosted Fargate runner sets it (see
/// `deploy/cdk`) so the role it creates can never exceed a fixed ceiling, even
/// though the runner holds `iam:AttachRolePolicy`. Under the runner this is not
/// optional: the runner's `CreateRole`/boundary grant is itself gated on
/// `iam:PermissionsBoundary` equaling this policy, so a create that omits the
/// boundary is denied. Setting it here is what makes bencher's own `CreateRole`
/// satisfy that condition. Standalone `bencher` runs leave it unset and create the
/// role with no boundary (the caller already holds whatever IAM rights they ran
/// with).
const EXEC_ROLE_BOUNDARY_POLICY_ENV: &str = "LAMBDABENCH_EXEC_ROLE_BOUNDARY_POLICY_ARN";

impl Aws {
    /// Ensures the execution role exists with the right policies. Returns its ARN.
    ///
    /// When `LAMBDABENCH_EXEC_ROLE_BOUNDARY_POLICY_ARN` is set (the hosted runner
    /// does this), that managed policy is applied as the role's permissions
    /// boundary on create, and a pre-existing role is converged onto it, so the
    /// role can never exceed the ceiling regardless of what policies are attached.
    ///
    /// Propagation: `CreateFunction` waits out trust-policy propagation (see
    /// `lambda::create_function`), but the inline DDB/KMS/S3 permissions attached
    /// below are NOT separately awaited and can lag by seconds on a brand-new role.
    /// Deploy and run are minutes apart in the normal pipeline, so the first
    /// invoke's permissions have settled by then; an `AccessDenied` on the first
    /// invoke of a freshly created role is inline-policy propagation lag, not a
    /// misconfiguration. The cell-retry (3 attempts, ~15 s backoff) may absorb a
    /// short lag but is not guaranteed to cover a deep one: a freshly created role
    /// invoked immediately can still exhaust the retries and abort. The mitigation
    /// is the deploy/run time gap, not the retry. If this becomes a real problem,
    /// probe inline-policy readiness here before returning rather than widening the
    /// cell-retry budget.
    pub async fn ensure_role(&self) -> Result<String> {
        // Optional permissions boundary (set by the hosted runner); see the
        // env-var doc comment above.
        let boundary = std::env::var(EXEC_ROLE_BOUNDARY_POLICY_ENV)
            .ok()
            .filter(|s| !s.is_empty());

        let existing = self.iam.get_role().role_name(ROLE_NAME).send().await;
        let arn = match existing {
            Ok(out) => {
                let role = out.role().context("GetRole returned no role")?;
                // Converge a pre-existing role onto the boundary: a role left by
                // an earlier run may carry no boundary (a standalone run) or a
                // different one, so the ceiling holds however the role was made.
                if let Some(b) = &boundary {
                    let current = role
                        .permissions_boundary()
                        .and_then(|p| p.permissions_boundary_arn());
                    if current != Some(b.as_str()) {
                        self.iam
                            .put_role_permissions_boundary()
                            .role_name(ROLE_NAME)
                            .permissions_boundary(b)
                            .send()
                            .await
                            .context("setting execution-role permissions boundary")?;
                    }
                }
                role.arn().to_string()
            }
            Err(err) => {
                let svc = err.into_service_error();
                if svc.is_no_such_entity_exception() {
                    let mut req = self
                        .iam
                        .create_role()
                        .role_name(ROLE_NAME)
                        .assume_role_policy_document(TRUST_POLICY)
                        .description("lambdabench Lambda execution role");
                    if let Some(b) = &boundary {
                        req = req.permissions_boundary(b);
                    }
                    let created = req.send().await.context("creating execution role")?;
                    created
                        .role()
                        .context("CreateRole returned no role")?
                        .arn()
                        .to_string()
                } else {
                    return Err(anyhow::Error::new(svc).context("GetRole failed"));
                }
            }
        };

        // Attach managed logging policy (idempotent).
        self.iam
            .attach_role_policy()
            .role_name(ROLE_NAME)
            .policy_arn(BASIC_EXEC_POLICY_ARN)
            .send()
            .await
            .context("attaching AWSLambdaBasicExecutionRole")?;

        // Inline policy granting exactly what the scenarios need: DynamoDB
        // GetItem + PutItem on the benchmark table (read scenarios + the
        // smithyfull write flow), KMS Encrypt, and S3 GetObject + PutObject on
        // the benchmark bucket.
        //
        // The KMS resource is "key/*" because this role is created before the key
        // (the role ARN is needed by CreateFunction). The kms:ResourceAliases
        // condition restricts Encrypt to keys carrying a <prefix>-* alias, so the
        // role cannot encrypt against unrelated CMKs. ForAnyValue:StringLike
        // matches when at least one of the key's aliases satisfies the pattern.
        let region = crate::config::REGION;
        let account = &self.account_id;
        let alias_wildcard = format!("alias/{PREFIX}-*");
        let table_arn = format!("arn:aws:dynamodb:{region}:{account}:table/{TABLE_NAME}");
        let bucket = self.bucket_name();
        let bucket_objects_arn = format!("arn:aws:s3:::{bucket}/*");
        let inline = format!(
            r#"{{
  "Version": "2012-10-17",
  "Statement": [
    {{ "Effect": "Allow", "Action": ["dynamodb:GetItem", "dynamodb:PutItem"], "Resource": "{table_arn}" }},
    {{ "Effect": "Allow",
       "Action": ["kms:Encrypt"],
       "Resource": "arn:aws:kms:{region}:{account}:key/*",
       "Condition": {{ "ForAnyValue:StringLike": {{ "kms:ResourceAliases": "{alias_wildcard}" }} }} }},
    {{ "Effect": "Allow", "Action": ["s3:GetObject", "s3:PutObject"], "Resource": "{bucket_objects_arn}" }}
  ]
}}"#
        );
        self.iam
            .put_role_policy()
            .role_name(ROLE_NAME)
            .policy_name(INLINE_POLICY_NAME)
            .policy_document(inline)
            .send()
            .await
            .context("putting inline scenario policy")?;

        Ok(arn)
    }

    /// Deletes the execution role and its policies. Used by teardown. A missing
    /// attachment/policy (NoSuchEntity) is tolerated for idempotence, but any
    /// other detach/delete-policy error (throttle, access-denied) is surfaced: a
    /// real failure here leaves a policy attached and then makes the final
    /// DeleteRole fail with DeleteConflict, so surfacing the original error keeps
    /// the root cause visible instead of masking it behind that conflict. A
    /// missing role is treated as success.
    pub async fn delete_role(&self) -> Result<()> {
        // Detach managed + delete inline before the role can be removed.
        if let Err(err) = self
            .iam
            .detach_role_policy()
            .role_name(ROLE_NAME)
            .policy_arn(BASIC_EXEC_POLICY_ARN)
            .send()
            .await
        {
            super::not_found_as_none(
                err,
                |e| e.is_no_such_entity_exception(),
                "iam:DetachRolePolicy (teardown)",
            )?;
        }
        if let Err(err) = self
            .iam
            .delete_role_policy()
            .role_name(ROLE_NAME)
            .policy_name(INLINE_POLICY_NAME)
            .send()
            .await
        {
            super::not_found_as_none(
                err,
                |e| e.is_no_such_entity_exception(),
                "iam:DeleteRolePolicy (teardown)",
            )?;
        }
        match self.iam.delete_role().role_name(ROLE_NAME).send().await {
            Ok(_) => Ok(()),
            Err(err) => super::not_found_as_none(
                err,
                |e| e.is_no_such_entity_exception(),
                "iam:DeleteRole (teardown)",
            )
            .map(|_| ()),
        }
    }
}
