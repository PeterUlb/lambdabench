//! S3 bucket and object provisioning for the scenarios that read a seeded object
//! at init (`three_client`, `lettercount`, `batch`, `smithy_full`).
//!
//! Ensures the benchmark bucket exists, is hardened (Block Public Access +
//! TLS-only policy), and holds the seeded scenario payloads, and tears it down.
//! The lightweight `ensure_bucket` path also backs the synthetic download-scaling
//! probe, which needs only the bucket to stage padded zips.

use super::Aws;
use crate::config::{
    REGION, S3_BATCH_KEY, S3_BATCH_KEY_CARDINALITY, S3_BATCH_PAYLOAD_BYTES, S3_LETTERCOUNT_KEY,
    S3_LETTERCOUNT_PAYLOAD_BYTES, S3_OBJECT_BODY, S3_OBJECT_KEY, SEED_VERSION_META_KEY,
};
use anyhow::{Context, Result};
use aws_sdk_s3::primitives::ByteStream;

impl Aws {
    /// Creates the benchmark S3 bucket if we don't already own it (idempotent),
    /// seeding no objects. This is the minimal S3 provisioning the synthetic
    /// download-scaling probe needs: it stages padded zips under `lambda-code/` but
    /// reads none of the seeded scenario payloads, so it calls this rather than the
    /// full `ensure_seeded_bucket` (which additionally uploads ~17 MB of lettercount/batch
    /// payloads it would never use).
    pub async fn ensure_bucket(&self) -> Result<()> {
        let bucket = self.bucket_name();

        let needs_create = match self.s3.head_bucket().bucket(&bucket).send().await {
            Ok(_) => false,
            // Not found -> create; any other error is surfaced.
            Err(err) => {
                super::not_found_as_none(err, |e| e.is_not_found(), "s3:HeadBucket")?.is_none()
            }
        };
        if needs_create {
            // S3 region quirk: us-east-1 must NOT set a LocationConstraint, every
            // other region MUST set it to the region name (else
            // IllegalLocationConstraint).
            let mut req = self.s3.create_bucket().bucket(&bucket);
            if REGION != "us-east-1" {
                use aws_sdk_s3::types::{BucketLocationConstraint, CreateBucketConfiguration};
                let constraint = BucketLocationConstraint::from(REGION);
                req = req.create_bucket_configuration(
                    CreateBucketConfiguration::builder()
                        .location_constraint(constraint)
                        .build(),
                );
            }
            let create = req.send().await;
            if let Err(err) = create {
                let svc = err.into_service_error();
                // Tolerate races / re-runs where we already own it.
                let code = svc.meta().code().unwrap_or_default();
                if code != "BucketAlreadyOwnedByYou" && code != "BucketAlreadyExists" {
                    return Err(anyhow::Error::new(svc).context("s3:CreateBucket"));
                }
            }
        }
        self.harden_bucket(&bucket).await?;
        Ok(())
    }

    /// Applies Block Public Access (all four flags) and a bucket policy denying
    /// any non-TLS request, matching the posture the CDK site buckets ship with
    /// (`deploy/cdk/lib/site-stack.ts`). Idempotent, so it is safe to re-run on
    /// every deploy.
    ///
    /// New buckets are already private and SSE-S3-encrypted by default, so this
    /// does not fix an active exposure; it makes the private, TLS-only posture
    /// explicit rather than inherited from an account default. Versioning is NOT
    /// enabled (unlike the CDK site bucket): this bucket holds only regenerable
    /// fixtures and is teardown-deleted, and object versions would only complicate
    /// that delete.
    async fn harden_bucket(&self, bucket: &str) -> Result<()> {
        use aws_sdk_s3::types::PublicAccessBlockConfiguration;
        self.s3
            .put_public_access_block()
            .bucket(bucket)
            .public_access_block_configuration(
                PublicAccessBlockConfiguration::builder()
                    .block_public_acls(true)
                    .ignore_public_acls(true)
                    .block_public_policy(true)
                    .restrict_public_buckets(true)
                    .build(),
            )
            .send()
            .await
            .context("s3:PutPublicAccessBlock")?;

        // Deny every request that did not arrive over TLS (the enforce-SSL
        // equivalent of the CDK bucket's `enforceSSL: true`).
        let policy = format!(
            r#"{{"Version":"2012-10-17","Statement":[{{"Sid":"DenyInsecureTransport","Effect":"Deny","Principal":"*","Action":"s3:*","Resource":["arn:aws:s3:::{bucket}","arn:aws:s3:::{bucket}/*"],"Condition":{{"Bool":{{"aws:SecureTransport":"false"}}}}}}]}}"#
        );
        self.s3
            .put_bucket_policy()
            .bucket(bucket)
            .policy(policy)
            .send()
            .await
            .context("s3:PutBucketPolicy (deny non-TLS)")?;
        Ok(())
    }

    pub async fn ensure_seeded_bucket(&self) -> Result<()> {
        let bucket = self.bucket_name();

        self.ensure_bucket().await?;

        // Seed the small three_client object.
        self.s3
            .put_object()
            .bucket(&bucket)
            .key(S3_OBJECT_KEY)
            .body(ByteStream::from_static(S3_OBJECT_BODY.as_bytes()))
            .send()
            .await
            .context("s3:PutObject (seed)")?;

        // Seed the large lettercount payload (~1 MB ASCII JSON). Deploy runs on
        // every `run`, so the payload is generated and uploaded only when the
        // stored object is missing or stale
        self.ensure_seed_object(
            &bucket,
            S3_LETTERCOUNT_KEY,
            &format!("lc1-{S3_LETTERCOUNT_PAYLOAD_BYTES}"),
            || generate_lettercount_payload(S3_LETTERCOUNT_PAYLOAD_BYTES),
        )
        .await?;

        // Seed the large batch payload (~16 MB JSON array of records). Read
        // once at init by the batch handlers, grouped-by key per invoke. Same
        // version-gated lazy seed as lettercount
        self.ensure_seed_object(
            &bucket,
            S3_BATCH_KEY,
            &format!("agg1-{S3_BATCH_PAYLOAD_BYTES}-{S3_BATCH_KEY_CARDINALITY}"),
            || generate_batch_payload(S3_BATCH_PAYLOAD_BYTES, S3_BATCH_KEY_CARDINALITY),
        )
        .await?;
        // The `authz` scenario seeds nothing here: it receives its signed JWT in
        // the invoke payload (build-time-generated fixture, gitignored), not from S3.
        Ok(())
    }

    /// Ensures the seed object at `key` exists and carries content matching
    /// `version`, generating and uploading the payload only when it is missing or
    /// stale. The payload is built lazily by `generate` so the common
    /// already-current path never materializes the (up to ~16 MB) document. The
    /// version is stored in object metadata and compared on HeadObject; bump the
    /// version tag whenever the generator's output changes so a same-length edit
    /// still re-seeds. A 404 means "needs upload"; any other HeadObject error is
    /// surfaced.
    async fn ensure_seed_object(
        &self,
        bucket: &str,
        key: &str,
        version: &str,
        generate: impl FnOnce() -> String,
    ) -> Result<()> {
        let current = match self.s3.head_object().bucket(bucket).key(key).send().await {
            Ok(out) => out
                .metadata()
                .and_then(|m| m.get(SEED_VERSION_META_KEY))
                .map(|v| v == version)
                .unwrap_or(false),
            Err(err) => super::not_found_as_none(
                err,
                |e| e.is_not_found(),
                format!("s3:HeadObject {key} (seed presence check)"),
            )
            .map(|opt| {
                // Not found -> object absent, so it is not current.
                debug_assert!(opt.is_none());
                false
            })?,
        };
        if current {
            return Ok(());
        }
        self.s3
            .put_object()
            .bucket(bucket)
            .key(key)
            .metadata(SEED_VERSION_META_KEY, version)
            .body(ByteStream::from(generate().into_bytes()))
            .send()
            .await
            .with_context(|| format!("s3:PutObject {key} (seed)"))?;
        Ok(())
    }

    /// Deletes a single object from the benchmark bucket, idempotently (a missing
    /// key is success). Used by the synthetic download-scaling probe to remove the
    /// staged `lambda-code/*.zip` it uploaded per ephemeral function, without the
    /// whole-bucket sweep `delete_s3` does.
    pub async fn delete_s3_object(&self, key: &str) -> Result<()> {
        let bucket = self.bucket_name();
        match self
            .s3
            .delete_object()
            .bucket(&bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => super::not_found_as_none(
                err,
                |e| e.meta().code() == Some("NoSuchBucket"),
                format!("s3:DeleteObject {key}"),
            )
            .map(|_| ()),
        }
    }

    /// Deletes the seeded object and the bucket. Used by teardown. A bucket that
    /// is already gone is treated as success (idempotent, matching the other
    /// teardown deletes); any other error is surfaced so teardown can report
    /// incomplete cleanup.
    pub async fn delete_s3(&self) -> Result<()> {
        let bucket = self.bucket_name();
        // A bucket must be empty before it can be deleted: remove the seeded
        // object plus any staged lambda-code/ zips (uploaded during deploy).
        let mut pages = self
            .s3
            .list_objects_v2()
            .bucket(&bucket)
            .into_paginator()
            .send();
        while let Some(page) = pages.next().await {
            // A missing bucket means a prior teardown already removed it: stop the
            // scan and treat it as done. Surface any other page error rather than
            // silently end the scan, since leaving objects behind makes the
            // DeleteBucket fail with `BucketNotEmpty` and the bucket (plus staged
            // code zips) leaks.
            let out = match page {
                Ok(out) => out,
                Err(err) => {
                    return super::not_found_as_none(
                        err,
                        |e| e.is_no_such_bucket(),
                        "s3:ListObjectsV2 (teardown)",
                    )
                    .map(|_| ());
                }
            };
            for obj in out.contents() {
                if let Some(key) = obj.key() {
                    self.s3
                        .delete_object()
                        .bucket(&bucket)
                        .key(key)
                        .send()
                        .await
                        .with_context(|| format!("s3:DeleteObject {key} (teardown)"))?;
                }
            }
        }
        // DeleteBucket does not model `NoSuchBucket` as a typed variant in the SDK
        // (it surfaces as an unhandled error carrying the wire code), so classify
        // it by error code rather than a generated predicate.
        match self.s3.delete_bucket().bucket(&bucket).send().await {
            Ok(_) => Ok(()),
            Err(err) => super::not_found_as_none(
                err,
                |e| e.meta().code() == Some("NoSuchBucket"),
                "s3:DeleteBucket (teardown)",
            )
            .map(|_| ()),
        }
    }
}

/// Builds the deterministic `lettercount` payload: a JSON array of lowercase-ASCII
/// strings, grown until the serialized document is at least `min_bytes`. ASCII-only
/// so the handlers' per-entry letter count agrees across Rust (bytes), Node (UTF-16
/// code units), and Python (code points), which match only for ASCII. Generated
/// in-process and deterministic, so every deploy seeds the same bytes.
fn generate_lettercount_payload(min_bytes: usize) -> String {
    // A small lowercase-ASCII word pool builds each entry deterministically, so
    // the payload is varied but reproducible.
    const WORDS: [&str; 8] = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    ];

    build_json_array(min_bytes, |i| {
        // Each entry is a short lowercase-ASCII sentence of a few words plus the
        // index, kept distinct across entries. JSON-quoted; the words and digits
        // contain no characters needing escaping.
        let w0 = WORDS[i % WORDS.len()];
        let w1 = WORDS[(i / WORDS.len()) % WORDS.len()];
        let w2 = WORDS[(i / (WORDS.len() * WORDS.len())) % WORDS.len()];
        let entry =
            format!(r#""{w0} {w1} {w2} the quick brown fox jumps over the lazy dog record{i}""#);
        // Cross-language fairness depends on ASCII-only content: the handlers
        // count over bytes (Rust), UTF-16 code units (Node), and code points
        // (Python), which agree ONLY for ASCII. A non-ASCII character would make
        // the three report different totals with no error, silently invalidating
        // the comparison. Assert here (runs once per entry at seed time) so a
        // future WORDS edit that sneaks in a non-ASCII byte fails the deploy.
        assert!(
            entry.is_ascii(),
            "lettercount payload entry {i} is not ASCII ({entry:?}); the cross-language \
             letter count is only fair for ASCII-only input"
        );
        entry
    })
}

/// Builds the deterministic `batch` payload: a JSON array of event records,
/// each `{"key":"k<NNNN>","value":<n>}`, grown until the serialized document is
/// at least `min_bytes`. Keys cycle over `cardinality` distinct values, so the
/// handlers' group-by produces a map of that many entries. Deterministic, so
/// every deploy seeds the same batch. Records are objects (not strings) so parsing
/// builds a real object graph per record, the GC fuel.
fn generate_batch_payload(min_bytes: usize, cardinality: usize) -> String {
    build_json_array(min_bytes, |i| {
        // key cycles 0..cardinality; value is a small varying integer. No
        // characters needing JSON escaping.
        let key = i % cardinality;
        let value = (i % 997) + 1;
        format!(r#"{{"key":"k{key:04}","value":{value}}}"#)
    })
}

/// Builds a deterministic JSON array whose serialized form is at least
/// `min_bytes`, appending `entry(i)` for i = 0, 1, 2, … until the closed array
/// reaches that size. Shared skeleton for the `lettercount` and `batch`
/// payloads, which differ only in how each element is rendered. `entry` must
/// return a complete, already-escaped JSON value (string or object).
fn build_json_array(min_bytes: usize, mut entry: impl FnMut(usize) -> String) -> String {
    let mut out = String::with_capacity(min_bytes + 4096);
    out.push('[');
    let mut i = 0usize;
    while out.len() < min_bytes {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&entry(i));
        i += 1;
    }
    out.push(']');
    out
}
