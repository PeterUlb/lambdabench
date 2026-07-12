//! Scenario "oneclient": construct ONE AWS SDK client (DynamoDB) and call it
//! once (`GetItem`) per invoke.
//!
//! The DynamoDB client is constructed during the Lambda init phase (before the
//! handler loop starts) so that the cold-start measurement includes AWS config
//! resolution and client construction, matching how a real service is written.

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::AttributeValue;
use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::{Value, json};
use std::env;

/// Environment variables injected by the bencher at deploy time.
const ENV_TABLE: &str = "LAMBDABENCH_TABLE";
const ENV_KEY: &str = "LAMBDABENCH_KEY";

/// Reads the seeded item by its partition key and returns its attributes.
async fn handle(client: &Client, table: &str, key: &str) -> Result<Value, Error> {
    let out = client
        .get_item()
        .table_name(table)
        .key("pk", AttributeValue::S(key.to_string()))
        .send()
        .await?;

    // Fail loud if the seeded item is absent: a missing item means a broken
    // benchmark setup, never a null fallback (matches the other languages).
    let item = out.item().ok_or("seeded item not found")?;
    let payload = item.get("payload").and_then(|v| v.as_s().ok()).cloned();

    Ok(json!({
        "scenario": "oneclient",
        "key": key,
        "payload": payload,
    }))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Init phase: resolve config and build the client once, reused across warm
    // invokes. Retries disabled: a throttle/transient must surface as a hard
    // failure, not be silently retried into an inflated Duration. A failed run
    // beats wrong data.
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .retry_config(aws_config::retry::RetryConfig::disabled())
        .load()
        .await;
    let client = Client::new(&config);
    let table = env::var(ENV_TABLE).map_err(|_| format!("{ENV_TABLE} not set"))?;
    let key = env::var(ENV_KEY).map_err(|_| format!("{ENV_KEY} not set"))?;

    lambda_runtime::run(service_fn(|_event: LambdaEvent<Value>| {
        handle(&client, &table, &key)
    }))
    .await
}
