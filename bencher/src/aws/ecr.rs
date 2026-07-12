//! ECR repository + image lifecycle for the synthetic download-scaling probe's
//! container-image family.
//!
//! The image family deploys ephemeral padded-size Lambda functions from a
//! container image (rather than a zip) so the probe can compare the
//! download+prepare cold-start cost of an image against a zip at the same padded
//! sizes. This module owns everything AWS-side about the image itself: the
//! private ECR repository the images are pushed to, reading back the true pushed
//! size, and teardown. The image assembly + push (`crane mutate`, the only
//! shell-out in the tool) lives in `probe.rs`, since it is a build step, not an
//! AWS SDK call. crane is daemonless, so this path runs unattended in the ECS
//! Fargate publish task (no Docker/finch VM).
//!
//! The repository carries the `lambdabench-` stem for consistency with the other
//! benchmark resources, but teardown reclaims it by exact name
//! (`delete_ecr_repo`), not a prefix sweep: the bencher owns only this one repo,
//! and a prefix sweep would also catch the CDK-managed `lambdabench-runner` repo
//! it does not own.

use super::Aws;
use crate::config::{ECR_REPO, REGION};
use anyhow::{Context, Result};
use aws_sdk_ecr::types::{ImageIdentifier, ImageTagMutability};

/// Lifecycle policy applied to the repo as a storage backstop: expire untagged
/// images after a day, and cap the tagged `synthdl-` images. Per-run teardown is
/// the primary reclaim; this catches images orphaned by an aborted run.
const LIFECYCLE_POLICY: &str = r#"{
  "rules": [
    {
      "rulePriority": 1,
      "description": "expire untagged images",
      "selection": { "tagStatus": "untagged", "countType": "sinceImagePushed", "countUnit": "days", "countNumber": 1 },
      "action": { "type": "expire" }
    },
    {
      "rulePriority": 2,
      "description": "cap tagged synthdl images",
      "selection": { "tagStatus": "tagged", "tagPrefixList": ["synthdl-"], "countType": "imageCountMoreThan", "countNumber": 20 },
      "action": { "type": "expire" }
    }
  ]
}"#;

impl Aws {
    /// The ECR registry host for this account/region
    /// (`<acct>.dkr.ecr.<region>.amazonaws.com`).
    pub fn ecr_registry_host(&self) -> String {
        format!("{}.dkr.ecr.{}.amazonaws.com", self.account_id, REGION)
    }

    /// The full image URI for a given tag in the benchmark repo.
    pub fn ecr_image_uri(&self, tag: &str) -> String {
        format!("{}/{}:{}", self.ecr_registry_host(), ECR_REPO, tag)
    }

    /// Ensures the benchmark ECR repository exists (idempotent), with immutable
    /// tags and the storage-backstop lifecycle policy. A pre-existing repository
    /// (`RepositoryAlreadyExistsException`) is treated as success. Returns the
    /// repository name.
    pub async fn ensure_ecr_repo(&self) -> Result<String> {
        match self
            .ecr
            .create_repository()
            .repository_name(ECR_REPO)
            .image_tag_mutability(ImageTagMutability::Immutable)
            .send()
            .await
        {
            Ok(_) => {}
            Err(err) => {
                let svc = err.into_service_error();
                // Idempotent: another run (or a prior invocation) already made it.
                if !svc.is_repository_already_exists_exception() {
                    return Err(anyhow::Error::new(svc)
                        .context(format!("creating ECR repository {ECR_REPO}")));
                }
            }
        }

        // Apply the lifecycle policy on every ensure so a repo left by an older
        // run converges onto the current backstop. Idempotent (last write wins).
        self.ecr
            .put_lifecycle_policy()
            .repository_name(ECR_REPO)
            .lifecycle_policy_text(LIFECYCLE_POLICY)
            .send()
            .await
            .with_context(|| format!("putting lifecycle policy on {ECR_REPO}"))?;

        Ok(ECR_REPO.to_string())
    }

    /// Logs `crane` into this account's ECR registry using an ECR authorization
    /// token, so a subsequent `crane mutate ... -t <ecr-uri>` push is
    /// authenticated. The token is fetched through the SDK (same pinned
    /// creds/region), decoded from its `AWS:<password>` base64 form, and fed to
    /// `crane auth login --password-stdin` over the child's stdin, so the password
    /// never appears in argv or a file. One login per run covers the whole probe
    /// (tokens last 12h).
    pub async fn crane_ecr_login(&self) -> Result<()> {
        use base64::Engine;
        use std::io::Write;
        use std::process::{Command, Stdio};

        let out = self
            .ecr
            .get_authorization_token()
            .send()
            .await
            .context("ecr:GetAuthorizationToken")?;
        let data = out
            .authorization_data()
            .first()
            .context("GetAuthorizationToken returned no authorization data")?;
        let token_b64 = data
            .authorization_token()
            .context("authorization data carried no token")?;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(token_b64)
            .context("decoding ECR authorization token")?;
        let decoded = String::from_utf8(decoded).context("ECR authorization token is not UTF-8")?;
        // Token decodes to `AWS:<password>`; split on the first colon only (the
        // password itself may contain colons).
        let password = decoded
            .split_once(':')
            .map(|(_, pw)| pw)
            .context("ECR authorization token was not in AWS:<password> form")?;

        let host = self.ecr_registry_host();
        let mut child = Command::new("crane")
            .args([
                "auth",
                "login",
                "--username",
                "AWS",
                "--password-stdin",
                &host,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning `crane auth login` (is crane installed and on PATH?)")?;
        child
            .stdin
            .take()
            .context("crane auth login: no stdin handle")?
            .write_all(password.as_bytes())
            .context("writing ECR password to crane auth login stdin")?;
        let output = child
            .wait_with_output()
            .context("waiting for `crane auth login`")?;
        if !output.status.success() {
            anyhow::bail!(
                "`crane auth login` failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    /// Reads back the true pushed size of an image tag (`image_size_in_bytes`
    /// from `DescribeImages`), the download-size axis for the image family (the
    /// analog of the zip family's `zip_bytes`). The requested padding MB is only
    /// the target; layer compression and base-image layers make the transferred
    /// size differ, so record what ECR actually stored.
    pub async fn ecr_image_size(&self, tag: &str) -> Result<u64> {
        let out = self
            .ecr
            .describe_images()
            .repository_name(ECR_REPO)
            .image_ids(ImageIdentifier::builder().image_tag(tag).build())
            .send()
            .await
            .with_context(|| format!("describing ECR image {ECR_REPO}:{tag}"))?;
        // A single exact tag returns exactly one detail; anything else (0 or >1)
        // means an assumption broke, so fail loud rather than take the first.
        let details = out.image_details();
        if details.len() != 1 {
            anyhow::bail!(
                "DescribeImages for {ECR_REPO}:{tag} returned {} image details (expected exactly 1)",
                details.len()
            );
        }
        let size = details[0]
            .image_size_in_bytes()
            .with_context(|| format!("DescribeImages for {ECR_REPO}:{tag} returned no size"))?;
        if size < 0 {
            anyhow::bail!("DescribeImages for {ECR_REPO}:{tag} returned negative size {size}");
        }
        Ok(size as u64)
    }

    /// Best-effort deletion of a single image tag from the benchmark repo. A
    /// missing repository or image is treated as success (idempotent teardown);
    /// any other error is surfaced.
    pub async fn delete_ecr_image(&self, tag: &str) -> Result<()> {
        match self
            .ecr
            .batch_delete_image()
            .repository_name(ECR_REPO)
            .image_ids(ImageIdentifier::builder().image_tag(tag).build())
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => super::not_found_as_none(
                err,
                |e| e.is_repository_not_found_exception(),
                format!("deleting ECR image {ECR_REPO}:{tag}"),
            )
            .map(|_| ()),
        }
    }

    /// Deletes the ECR repository the bencher owns (`ECR_REPO`) and every image in
    /// it (`force(true)`). Idempotent: a missing repository is treated as success.
    ///
    /// Targets the exact repo by name rather than sweeping the `lambdabench-`
    /// prefix. The bencher creates only this one repository (the image family
    /// varies by tag, not by repo), so a prefix sweep reclaims nothing extra while
    /// risking a sibling `lambdabench-*` repo it does not own: notably the
    /// CDK-managed `lambdabench-runner` repo (the runner image the ECS task pulls),
    /// whose deletion would break the next run's image pull.
    pub async fn delete_ecr_repo(&self) -> Result<()> {
        match self
            .ecr
            .delete_repository()
            .repository_name(ECR_REPO)
            .force(true)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => super::not_found_as_none(
                err,
                |e| e.is_repository_not_found_exception(),
                format!("deleting ECR repository {ECR_REPO}"),
            )
            .map(|_| ()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{ECR_REPO, RESOURCE_PREFIX};

    /// The repo name carries the `lambdabench-` stem for consistency with every
    /// other benchmark resource. Teardown reclaims it by exact name, not prefix
    /// match, but the shared stem keeps the naming uniform.
    #[test]
    fn ecr_repo_carries_resource_prefix() {
        assert!(ECR_REPO.starts_with(RESOURCE_PREFIX));
    }
}
