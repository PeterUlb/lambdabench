//! Scenario "hello": return a constant.
//!
//! This is the baseline that isolates pure runtime startup + handler dispatch
//! cost, with no I/O and no SDK initialization. The returned constant matches
//! the other languages' hello output, so the comparison across runtimes stays
//! apples-to-apples.

use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::{Value, json};

async fn handler(_event: LambdaEvent<Value>) -> Result<Value, Error> {
    Ok(json!({ "message": "hello", "scenario": "hello" }))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    lambda_runtime::run(service_fn(handler)).await
}
