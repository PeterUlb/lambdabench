//! DynamoDB table provisioning and seeding for the DDB-using scenarios
//! (`oneclient`, `threeclient`, `smithyfull`).
//!
//! A single on-demand table holds one seeded item. The scenarios read it by its
//! partition key.

use super::Aws;
use crate::config::{SEED_KEY, SEED_PAYLOAD, TABLE_NAME, TABLE_PK};
use anyhow::{Context, Result, bail};
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, KeySchemaElement, KeyType,
    ScalarAttributeType, TableStatus,
};
use std::time::Duration;

impl Aws {
    /// Ensures the table exists (on-demand), waits until ACTIVE, and seeds the
    /// single benchmark item. Idempotent.
    pub async fn ensure_table(&self) -> Result<()> {
        let exists = self
            .ddb
            .describe_table()
            .table_name(TABLE_NAME)
            .send()
            .await;

        match exists {
            Ok(out) => {
                // A table left in DELETING by a preceding teardown is not usable:
                // treating it as "present" and falling through to
                // wait_table_active would race the delete completing (DescribeTable
                // starts returning ResourceNotFound and the wait bails). Drain the
                // delete first, then recreate, so a deploy right after a teardown is
                // reliable.
                let status = out.table().and_then(|t| t.table_status()).cloned();
                if status == Some(TableStatus::Deleting) {
                    self.wait_table_gone().await?;
                    self.create_table().await?;
                }
                // Any other status (ACTIVE, CREATING, UPDATING) is handled by the
                // wait_table_active poll below.
            }
            Err(err) => {
                let svc = err.into_service_error();
                if svc.is_resource_not_found_exception() {
                    self.create_table().await?;
                } else {
                    return Err(anyhow::Error::new(svc).context("DescribeTable failed"));
                }
            }
        }

        self.wait_table_active().await?;
        self.seed_item().await?;
        Ok(())
    }

    /// Creates the on-demand benchmark table. Caller is responsible for waiting
    /// until it becomes ACTIVE.
    async fn create_table(&self) -> Result<()> {
        self.ddb
            .create_table()
            .table_name(TABLE_NAME)
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(TABLE_PK)
                    .attribute_type(ScalarAttributeType::S)
                    .build()
                    .context("building attribute definition")?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(TABLE_PK)
                    .key_type(KeyType::Hash)
                    .build()
                    .context("building key schema")?,
            )
            .billing_mode(BillingMode::PayPerRequest)
            .send()
            .await
            .context("creating DynamoDB table")?;
        Ok(())
    }

    /// Polls until a DELETING table is fully gone (DescribeTable returns
    /// ResourceNotFound), so it can be recreated. Budget matches wait_table_active
    /// (60 × 2 s = 120 s); a still-deleting table after that is an anomaly.
    async fn wait_table_gone(&self) -> Result<()> {
        for _ in 0..60 {
            match self
                .ddb
                .describe_table()
                .table_name(TABLE_NAME)
                .send()
                .await
            {
                Ok(_) => tokio::time::sleep(Duration::from_secs(2)).await,
                Err(err) => {
                    let svc = err.into_service_error();
                    if svc.is_resource_not_found_exception() {
                        return Ok(());
                    }
                    return Err(
                        anyhow::Error::new(svc).context("DescribeTable while waiting for delete")
                    );
                }
            }
        }
        bail!("table {} did not finish DELETING in time", TABLE_NAME)
    }

    /// Polls until the table reports ACTIVE (create is eventually consistent).
    /// Budget: 60 polls × 2 s = 120 s, ample for a fresh on-demand table, which
    /// activates in seconds; this is a one-time deploy-path wait, separate from
    /// the 300 s Lambda config-update poll in `lambda.rs::wait_ready`.
    async fn wait_table_active(&self) -> Result<()> {
        for _ in 0..60 {
            let out = self
                .ddb
                .describe_table()
                .table_name(TABLE_NAME)
                .send()
                .await
                .context("DescribeTable while waiting for ACTIVE")?;
            if let Some(status) = out.table().and_then(|t| t.table_status())
                && status == &TableStatus::Active
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        bail!("table {} did not become ACTIVE in time", TABLE_NAME)
    }

    /// Writes the single seeded item the scenarios read.
    async fn seed_item(&self) -> Result<()> {
        self.ddb
            .put_item()
            .table_name(TABLE_NAME)
            .item(TABLE_PK, AttributeValue::S(SEED_KEY.to_string()))
            .item("payload", AttributeValue::S(SEED_PAYLOAD.to_string()))
            .send()
            .await
            .context("seeding benchmark item")?;
        Ok(())
    }

    /// Deletes the benchmark table. Used by teardown. A missing table is treated
    /// as success (idempotent); any other error is surfaced so teardown can
    /// report incomplete cleanup.
    pub async fn delete_table(&self) -> Result<()> {
        match self.ddb.delete_table().table_name(TABLE_NAME).send().await {
            Ok(_) => Ok(()),
            Err(err) => super::not_found_as_none(
                err,
                |e| e.is_resource_not_found_exception(),
                "dynamodb:DeleteTable (teardown)",
            )
            .map(|_| ()),
        }
    }
}
