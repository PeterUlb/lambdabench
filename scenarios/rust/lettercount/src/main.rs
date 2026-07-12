//! Scenario "lettercount": CPU-bound work with no per-invoke I/O.
//!
//! At init the handler fetches a JSON document from S3 once (a ~1 MB array of
//! ASCII strings) and keeps the raw text in memory. Each warm invoke then does
//! pure in-memory work: parse the JSON array, and for every string count the
//! occurrences of each lowercase ASCII letter (`a`..`z`), summing into 26
//! per-letter totals returned as the response.
//!
//! Why this workload:
//!   1. It is in-language CPU work. The counting is a tight loop over the string's
//!      bytes/code units, running in each runtime's own execution (Rust machine
//!      code vs the V8 JIT) rather than a shared native library. Contrast a
//!      hashing or `JSON.stringify`-heavy workload, where the time is spent in
//!      native C++/OpenSSL both runtimes share, measuring the library not the
//!      language.
//!   2. The parse rebuilds a fresh object graph each invoke, so a GC runtime may
//!      show pauses in the warm tail under a constrained heap while a non-GC
//!      runtime stays flat.
//!
//! Fetching the payload at init keeps the warm measurement pure compute with no
//! network. Counting is restricted to ASCII `a`..`z` so Rust (iterating bytes) and
//! Node (iterating UTF-16 code units) do identical work and produce identical
//! totals.

use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::{Value, json};
use std::env;

/// Environment variables injected by the bencher at deploy time.
const ENV_BUCKET: &str = "LAMBDABENCH_BUCKET";
const ENV_OBJECT: &str = "LAMBDABENCH_LETTERCOUNT_KEY";

/// Holds the payload fetched once at init and reused across warm invokes.
struct State {
    payload: String,
}

/// Parse the JSON string array and count lowercase-ASCII letters across all
/// entries. Returns 26 totals (index 0 = 'a' .. 25 = 'z'). The parse is the
/// allocation source; the per-byte count is the in-language CPU work.
///
/// Deserialized into a typed `Vec<String>` (not a generic `serde_json::Value`
/// tree) so the parse's object graph matches the Node/Python handlers, whose
/// native parsers produce a flat string array. Same fairness rule as `batch`.
fn count_letters(payload: &str) -> Result<[u64; 26], Error> {
    let entries: Vec<String> = serde_json::from_str(payload)?;
    let mut totals = [0u64; 26];
    for s in &entries {
        for b in s.bytes() {
            if b.is_ascii_lowercase() {
                totals[(b - b'a') as usize] += 1;
            }
        }
    }
    Ok(totals)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Init phase: fetch the JSON payload from S3 once and hold it in memory.
    // Retries disabled: a throttle on the init-time fetch must surface as a hard
    // failure, not be silently retried into an inflated init_ms. A failed run
    // beats wrong data.
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .retry_config(aws_config::retry::RetryConfig::disabled())
        .load()
        .await;
    let s3 = aws_sdk_s3::Client::new(&config);
    let bucket = env::var(ENV_BUCKET).map_err(|_| format!("{ENV_BUCKET} not set"))?;
    let key = env::var(ENV_OBJECT).map_err(|_| format!("{ENV_OBJECT} not set"))?;

    let obj = s3.get_object().bucket(&bucket).key(&key).send().await?;
    let bytes = obj.body.collect().await?.into_bytes();
    let payload = String::from_utf8(bytes.to_vec())?;
    let state = State { payload };

    lambda_runtime::run(service_fn(|_event: LambdaEvent<Value>| async {
        let totals = count_letters(&state.payload)?;
        Ok::<Value, Error>(json!({ "scenario": "lettercount", "letter_counts": totals }))
    }))
    .await
}
