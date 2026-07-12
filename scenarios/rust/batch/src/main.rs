//! Scenario "batch": a deserialize-heavy batch record processor (the canonical
//! Kinesis/SQS-consumer shape).
//!
//! At init the handler fetches a large (~16 MB) JSON array of event records from
//! S3 and keeps the raw text in memory. Each warm invoke parses the whole batch
//! into records, then groups-by `key` into a map of running sum + count, and
//! returns the per-group totals.
//!
//! Read this scenario on two independent axes:
//!   - MEDIAN = each language's standard JSON-parser speed, which dominates. Rust
//!     uses `serde` with a compile-time-monomorphized `Deserialize`, the fast end
//!     of the spread. Decoding into a typed `Vec<Record>` (not a generic
//!     `serde_json::Value` tree) keeps the parse representative of how a real Go/
//!     Java/Python handler decodes: a flat record array, not a tagged node tree.
//!     We keep `serde_json`, Rust's de-facto standard decoder; a faster crate
//!     would compare libraries, not languages.
//!   - TAIL (P99/P99.9) at the smaller memory tiers = allocation + GC. The parsed
//!     records and the group map are all live simultaneously for the whole invoke,
//!     a large transient object graph a tracing-GC runtime (Node, JVM, Go, Python)
//!     must trace then collect, while this non-GC runtime drops it at end of scope.
//!     The GC tail is pronounced on Java/Python, mild on Go; even this runtime
//!     shows some low-tier tail from CPU-throttling/OS noise, so read the tail in
//!     absolute ms, not as a ratio.
//!
//! Contrast `lettercount`, which counts into a fixed `[u64; 26]` (nothing grows).
//! Fetching the batch at init keeps the warm measurement pure compute. The
//! group-by (parse + HashMap insert/update + arithmetic) is in-language work, not
//! a native library, so the comparison is fair.

use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;

/// Environment variables injected by the bencher at deploy time.
const ENV_BUCKET: &str = "LAMBDABENCH_BUCKET";
const ENV_OBJECT: &str = "LAMBDABENCH_BATCH_KEY";

/// One event record in the batch.
#[derive(Deserialize)]
struct Record {
    key: String,
    value: i64,
}

/// Running aggregate per group.
#[derive(Default)]
struct Agg {
    sum: i64,
    count: u64,
}

/// Holds the raw batch text fetched once at init and reused across warm invokes.
struct State {
    payload: String,
}

/// Parse the whole batch and group-by `key` into sum + count. The parse allocates
/// the full record graph and the map holds an entry per distinct key, both live
/// for the duration of the call, which is the GC fuel.
fn process_batch(payload: &str) -> Result<Value, Error> {
    // Deserializing into the typed `Vec<Record>` rejects a missing, null, or
    // mistyped `key`/`value` at parse time, so a malformed batch fails loud
    // rather than grouping silently-wrong data, matching the other handlers.
    let records: Vec<Record> = serde_json::from_str(payload)?;
    let mut groups: HashMap<String, Agg> = HashMap::new();
    let mut total: i64 = 0;
    // Cross-language fairness: Java's `computeIfAbsent` and Go's map insert reuse
    // the parsed key reference, allocating only at first sight of each distinct
    // key. Rust's `entry(key.clone())` would clone on every record, an unfair
    // per-record tax on the median. Look up first, clone the key only on miss.
    for r in &records {
        if let Some(entry) = groups.get_mut(&r.key) {
            entry.sum += r.value;
            entry.count += 1;
        } else {
            groups.insert(
                r.key.clone(),
                Agg {
                    sum: r.value,
                    count: 1,
                },
            );
        }
        total += r.value;
    }
    // Emit per-group totals plus headline figures. Building the output allocates
    // proportional to group count, mirroring what a real batch processor hands
    // downstream.
    let mut per_group: Vec<Value> = Vec::with_capacity(groups.len());
    for (k, agg) in &groups {
        per_group.push(json!({ "key": k, "sum": agg.sum, "count": agg.count }));
    }
    Ok(json!({
        "scenario": "batch",
        "records": records.len(),
        "groups": groups.len(),
        "total": total,
        "per_group": per_group,
    }))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Init phase: fetch the batch from S3 once and hold it in memory. Retries
    // disabled: a throttle on the init-time fetch must surface as a hard failure,
    // not be silently retried into an inflated init_ms. A failed run beats wrong
    // data.
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
        process_batch(&state.payload)
    }))
    .await
}
