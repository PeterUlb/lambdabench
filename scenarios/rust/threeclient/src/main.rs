//! Scenario "threeclient": construct THREE AWS SDK clients (DynamoDB, KMS, S3)
//! at init and call all three per invoke.
//!
//! Each client is built during the Lambda init phase and reused across warm
//! invokes. At invoke time the handler performs three operations: a DynamoDB
//! `GetItem`, a KMS `Encrypt` of a short constant, and an S3 `GetObject` of a
//! small seeded object. Comparing its cold start against `oneclient` shows what
//! additional AWS clients add (extra middleware stacks plus a first TLS
//! handshake per distinct endpoint). Read by direct comparison, not subtraction.

use aws_sdk_dynamodb::types::AttributeValue;
use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::{Value, json};
use std::env;

/// Environment variables injected by the bencher at deploy time.
const ENV_TABLE: &str = "LAMBDABENCH_TABLE";
const ENV_KEY: &str = "LAMBDABENCH_KEY";
const ENV_KMS_KEY: &str = "LAMBDABENCH_KMS_KEY_ID";
const ENV_BUCKET: &str = "LAMBDABENCH_BUCKET";
const ENV_OBJECT: &str = "LAMBDABENCH_OBJECT_KEY";

/// Holds the three clients plus the resource identifiers, built once at init.
struct Clients {
    ddb: aws_sdk_dynamodb::Client,
    kms: aws_sdk_kms::Client,
    s3: aws_sdk_s3::Client,
    table: String,
    key: String,
    kms_key_id: String,
    bucket: String,
    object_key: String,
}

/// Calls DynamoDB GetItem, KMS Encrypt, and S3 GetObject, returning a small
/// summary so the benchmark can confirm all three succeeded.
async fn handle(c: &Clients) -> Result<Value, Error> {
    // 1. DynamoDB GetItem.
    let ddb_out = c
        .ddb
        .get_item()
        .table_name(&c.table)
        .key("pk", AttributeValue::S(c.key.clone()))
        .send()
        .await?;
    // Fail loud if the seeded item is absent: a missing item means a broken
    // benchmark setup, never a null fallback (matches oneclient and the other
    // languages).
    let item = ddb_out.item().ok_or("seeded item not found")?;
    let payload = item.get("payload").and_then(|v| v.as_s().ok()).cloned();

    // 2. KMS Encrypt of a short constant ("hello").
    let kms_out = c
        .kms
        .encrypt()
        .key_id(&c.kms_key_id)
        .plaintext(aws_sdk_kms::primitives::Blob::new(b"hello".to_vec()))
        .send()
        .await?;
    let ciphertext_len = kms_out
        .ciphertext_blob()
        .map(|b| b.as_ref().len())
        .unwrap_or(0);

    // 3. S3 GetObject of a small seeded object. Measure the raw byte length; a
    // decode to a String would add per-invoke work the other languages do not do.
    let s3_out =
        c.s3.get_object()
            .bucket(&c.bucket)
            .key(&c.object_key)
            .send()
            .await?;
    let body = s3_out.body.collect().await?;
    let object_len = body.into_bytes().len();

    Ok(json!({
        "scenario": "threeclient",
        "ddb_payload": payload,
        "kms_ciphertext_len": ciphertext_len,
        "s3_object_len": object_len,
    }))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Init phase: resolve config once, then build all three clients. Retries
    // disabled: a throttle/transient must surface as a hard failure, not be
    // silently retried into an inflated Duration. A failed run beats wrong data.
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .retry_config(aws_config::retry::RetryConfig::disabled())
        .load()
        .await;
    let clients = Clients {
        ddb: aws_sdk_dynamodb::Client::new(&config),
        kms: aws_sdk_kms::Client::new(&config),
        s3: aws_sdk_s3::Client::new(&config),
        table: env::var(ENV_TABLE).map_err(|_| format!("{ENV_TABLE} not set"))?,
        key: env::var(ENV_KEY).map_err(|_| format!("{ENV_KEY} not set"))?,
        kms_key_id: env::var(ENV_KMS_KEY).map_err(|_| format!("{ENV_KMS_KEY} not set"))?,
        bucket: env::var(ENV_BUCKET).map_err(|_| format!("{ENV_BUCKET} not set"))?,
        object_key: env::var(ENV_OBJECT).map_err(|_| format!("{ENV_OBJECT} not set"))?,
    };

    lambda_runtime::run(service_fn(|_event: LambdaEvent<Value>| handle(&clients))).await
}
