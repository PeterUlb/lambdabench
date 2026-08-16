//! Scenario "hello": return a constant.
//!
//! This is the baseline that isolates runtime startup + handler dispatch
//! cost, with no I/O and no SDK initialization. The returned constant matches
//! the other languages' hello output, so only runtime overhead differs.

use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::{Value, json};

async fn handler(_event: LambdaEvent<Value>) -> Result<Value, Error> {
    Ok(json!({ "message": "hello", "scenario": "hello" }))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    lambda_runtime::run(service_fn(handler)).await
}
