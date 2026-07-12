//! KMS key provisioning for the scenarios that call KMS `Encrypt`
//! (`three_client` and `smithy_full`).
//!
//! Ensures a single symmetric encryption key exists, addressed by a stable
//! alias, and tears it down. The key is tagged at creation so teardown can
//! reclaim it even when the alias is missing (see `delete_kms_key`).

use super::Aws;
use crate::config::{KMS_ALIAS, KMS_TAG_KEY, KMS_TAG_VALUE};
use anyhow::{Context, Result};

impl Aws {
    /// Resolves the KMS key id behind our stable alias, scanning every page of
    /// `ListAliases`. Returns `None` if the alias is absent. A page error is
    /// surfaced, not silently ended: a dropped page could miss the alias and leave
    /// the key orphaned on create or unscheduled on teardown. `ctx` labels the call
    /// site in the error.
    async fn resolve_alias_key_id(&self, ctx: &'static str) -> Result<Option<String>> {
        let mut aliases = self.kms.list_aliases().into_paginator().items().send();
        while let Some(alias) = aliases.next().await {
            let a = alias.context(ctx)?;
            if a.alias_name() == Some(KMS_ALIAS)
                && let Some(id) = a.target_key_id()
            {
                return Ok(Some(id.to_string()));
            }
        }
        Ok(None)
    }

    /// Ensures a KMS key exists, addressed by our stable alias. Returns the key
    /// id. Idempotent: reuses the existing key if the alias is already present.
    pub async fn ensure_kms_key(&self) -> Result<String> {
        if let Some(id) = self.resolve_alias_key_id("kms:ListAliases").await? {
            return Ok(id);
        }

        // Not found: create a symmetric encryption key and point the alias at it.
        // Tag the key at creation: the runner's IAM policy scopes
        // kms:ScheduleKeyDeletion by this tag, not by alias, because teardown
        // removes the alias before scheduling. The tag is also what lets teardown
        // reclaim an aliasless orphan: if CreateAlias and the best-effort schedule
        // below both fail, `delete_kms_key`'s tag sweep is the only path that can
        // still find and delete this key.
        let created = self
            .kms
            .create_key()
            .description("lambdabench three_client encrypt key")
            .tags(
                aws_sdk_kms::types::Tag::builder()
                    .tag_key(KMS_TAG_KEY)
                    .tag_value(KMS_TAG_VALUE)
                    .build()?,
            )
            .send()
            .await
            .context("kms:CreateKey")?;
        let key_id = created
            .key_metadata()
            .context("CreateKey returned no key metadata")?
            .key_id()
            .to_string();

        // If the alias fails to attach, the key just created is orphaned: the next
        // `ensure_kms_key` searches only by alias, won't find it, and mints another
        // billable key, while teardown (which also resolves via the alias) can
        // never reach it. Best-effort schedule the orphan for deletion before
        // propagating the error, so a transient CreateAlias failure does not
        // silently accumulate cost.
        if let Err(err) = self
            .kms
            .create_alias()
            .alias_name(KMS_ALIAS)
            .target_key_id(&key_id)
            .send()
            .await
        {
            let _ = self
                .kms
                .schedule_key_deletion()
                .key_id(&key_id)
                .pending_window_in_days(7)
                .send()
                .await;
            return Err(anyhow::Error::new(err).context("kms:CreateAlias"));
        }

        Ok(key_id)
    }

    /// Schedules the KMS key for deletion (minimum 7-day window; KMS does not
    /// allow immediate deletion) and removes the alias. Used by teardown.
    ///
    /// A leftover alias still pointing at a now-pending-delete key would let the
    /// next deploy's `resolve_alias_key_id` resurrect it (KMS resolves aliases even
    /// for keys in `PendingDeletion`), wiring functions to a key that disappears
    /// mid-run. So a non-not-found alias-delete error is surfaced; an absent alias
    /// is success.
    ///
    /// The alias path alone cannot reclaim an aliasless orphan: if `ensure_kms_key`
    /// created and tagged a key but then both `CreateAlias` and its best-effort
    /// orphan-schedule failed, the key survives with our tag and no alias, so
    /// `resolve_alias_key_id` never finds it. A tag sweep (below) reclaims exactly
    /// that key, which is the reason the key is tagged at creation.
    pub async fn delete_kms_key(&self) -> Result<()> {
        let key_id = self
            .resolve_alias_key_id("kms:ListAliases (teardown)")
            .await?;
        if let Err(err) = self.kms.delete_alias().alias_name(KMS_ALIAS).send().await {
            super::not_found_as_none(
                err,
                |e| e.is_not_found_exception(),
                "kms:DeleteAlias (teardown)",
            )?;
        }
        if let Some(id) = key_id {
            self.schedule_key_deletion_idempotent(&id).await?;
        }
        // Reclaim any tagged-but-aliasless orphan left by a double CreateAlias +
        // orphan-schedule failure. This also re-encounters the just-scheduled
        // aliased key (now PendingDeletion), which the idempotent helper tolerates.
        self.reclaim_orphaned_kms_keys().await
    }

    /// Schedules a key for deletion, treating "already scheduled" as success.
    /// `KmsInvalidStateException` on a key already in `PendingDeletion` and a
    /// `NotFoundException` on a key deleted out from under us are both the desired
    /// end state, so they are not surfaced.
    async fn schedule_key_deletion_idempotent(&self, key_id: &str) -> Result<()> {
        if let Err(err) = self
            .kms
            .schedule_key_deletion()
            .key_id(key_id)
            .pending_window_in_days(7)
            .send()
            .await
        {
            super::not_found_as_none(
                err,
                |e| e.is_not_found_exception() || e.is_kms_invalid_state_exception(),
                "kms:ScheduleKeyDeletion (teardown)",
            )?;
        }
        Ok(())
    }

    /// Sweeps every customer KMS key carrying our creation tag and schedules any
    /// not already pending deletion. Closes the leak where a key created and
    /// tagged by `ensure_kms_key` never received its alias, so the alias-based
    /// teardown path cannot see it. Scans all pages of `ListKeys`; a page error is
    /// surfaced, not silently ended, so a partial scan cannot leave an orphan
    /// unreported. Each key needs a `ListResourceTags` call to read its tags,
    /// which is why this is a teardown-only cost.
    async fn reclaim_orphaned_kms_keys(&self) -> Result<()> {
        let mut keys = self.kms.list_keys().into_paginator().items().send();
        while let Some(entry) = keys.next().await {
            let entry = entry.context("kms:ListKeys (teardown orphan sweep)")?;
            let Some(id) = entry.key_id() else { continue };
            if self.key_has_creation_tag(id).await? {
                self.schedule_key_deletion_idempotent(id).await?;
            }
        }
        Ok(())
    }

    /// True if the key carries our `KMS_TAG_KEY=KMS_TAG_VALUE` creation tag. Scans
    /// all tag pages via the paginator. A key deleted mid-sweep
    /// (`NotFoundException`) is treated as untagged (nothing to reclaim); other
    /// errors surface.
    async fn key_has_creation_tag(&self, key_id: &str) -> Result<bool> {
        let mut tags = self
            .kms
            .list_resource_tags()
            .key_id(key_id)
            .into_paginator()
            .items()
            .send();
        while let Some(tag) = tags.next().await {
            let tag = match tag {
                Ok(tag) => tag,
                Err(err) => {
                    return super::not_found_as_none(
                        err,
                        |e| e.is_not_found_exception(),
                        "kms:ListResourceTags (teardown orphan sweep)",
                    )
                    .map(|_| false);
                }
            };
            if tag.tag_key() == KMS_TAG_KEY && tag.tag_value() == KMS_TAG_VALUE {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
