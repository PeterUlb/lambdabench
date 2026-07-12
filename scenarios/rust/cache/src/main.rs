//! Scenario "cache": a long-lived in-memory working set, churned every invoke,
//! the dedicated garbage-collection probe.
//!
//! At init the handler allocates a large RETAINED live set: `ENTRIES` byte
//! buffers of `ENTRY_BYTES` each (~100 MB), held in state that persists across
//! every warm invocation, the way a real handler holds an in-process cache, an
//! LRU, a buffer pool, or loaded reference data for the life of the sandbox.
//!
//! Each warm invoke does two things:
//!   1. Churn: replace `CHURN` of the entries with freshly allocated buffers
//!      (eviction + insert). The replaced buffers become garbage while the live
//!      set stays full, generating garbage against a large permanently-live heap.
//!   2. Scan: read every 10th entry and sum a byte. This keeps the whole retained
//!      set genuinely live and read, so the compiler cannot elide it and a tracing
//!      GC has to mark all of it.
//!
//! Why this workload (contrast `batch`): a tracing GC's per-cycle cost scales with
//! the size of the live heap it must trace, not with the garbage. Keeping a large
//! live set permanently resident while generating garbage against it makes every
//! GC cycle expensive, the path `batch` never reaches (its object graph is
//! transient and Go's compact representation stays far from the ceiling). At the
//! smaller fractional-vCPU tiers a concurrent collector cannot run on a spare core
//! and steals time from the handler, so the warm P99/P99.9 tail blows up while the
//! median stays flat, worst at the starved low-memory tiers and easing as vCPU
//! grows. A non-GC runtime frees each replaced buffer immediately, so its tail
//! stays flat at every tier. Read the absolute tail latencies on the dashboard,
//! not a P99/P50 ratio.
//!
//! Deliberately an indexed ring of buffers, not a hashmap: the point is to isolate
//! the GC/allocator, not to compare hashmap implementations or hashing speed. No
//! S3, no AWS clients, no payload; fully self-contained.

use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::{Value, json};
use std::sync::Mutex;

/// Number of buffers in the retained live set.
const ENTRIES: usize = 200_000;
/// Bytes per buffer. ENTRIES * ENTRY_BYTES ≈ 100 MB of permanently-live heap.
const ENTRY_BYTES: usize = 512;
/// Buffers replaced per invoke (garbage generated + new live, set stays full).
const CHURN: usize = 40_000;

/// The retained working set plus a ring cursor, built once at init and mutated
/// in place across warm invokes.
struct State {
    live: Vec<Vec<u8>>,
    rot: usize,
}

/// Replace `CHURN` entries then scan every 10th entry. The replaced buffers become
/// garbage while the live set stays full, so a tracing GC keeps tracing the whole
/// ~100 MB set. The scan keeps it genuinely live and read.
fn churn_and_scan(state: &mut State) -> u64 {
    for c in 0..CHURN {
        state.rot = (state.rot + 1) % ENTRIES;
        let mut b = vec![0u8; ENTRY_BYTES];
        b[0] = (c & 0xff) as u8;
        let idx = state.rot;
        state.live[idx] = b;
    }
    let mut sum: u64 = 0;
    let mut i = 0;
    while i < ENTRIES {
        sum += state.live[i][0] as u64;
        i += 10;
    }
    sum
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Init phase: allocate the retained live set once. No I/O, no AWS clients.
    let mut live: Vec<Vec<u8>> = Vec::with_capacity(ENTRIES);
    for i in 0..ENTRIES {
        let mut b = vec![0u8; ENTRY_BYTES];
        b[0] = (i & 0xff) as u8;
        live.push(b);
    }
    let state = Mutex::new(State { live, rot: 0 });

    lambda_runtime::run(service_fn(|_event: LambdaEvent<Value>| async {
        let checksum = {
            let mut st = state.lock().expect("cache state mutex poisoned");
            churn_and_scan(&mut st)
        };
        Ok::<Value, Error>(json!({
            "scenario": "cache",
            "entries": ENTRIES,
            "churned": CHURN,
            "checksum": checksum,
        }))
    }))
    .await
}
