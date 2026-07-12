// Scenario "cache": a long-lived in-memory working set, churned every invoke,
// the dedicated garbage-collection probe.
//
// At init the handler allocates a large RETAINED live set: ENTRIES byte buffers
// of ENTRY_BYTES each (~100 MB), held in module state that persists across every
// warm invocation, the way a real handler holds an in-process cache, an LRU, a
// buffer pool, or loaded reference data for the life of the sandbox.
//
// Each warm invoke does two things:
//   1. Churn: replace CHURN of the entries with freshly allocated buffers
//      (eviction + insert). The replaced buffers become garbage while the live
//      set stays full, generating garbage against a large permanently-live heap.
//   2. Scan: read every 10th entry and sum a byte. This keeps the whole retained
//      set genuinely live and read, so V8 cannot elide it and must mark all of it.
//
// Why this workload (contrast `batch`): a tracing GC's per-cycle cost scales with
// the size of the live heap it must trace, not with the garbage. Keeping a large
// live set permanently resident while generating garbage against it makes every
// major-GC cycle expensive, the path `batch` never reaches (its object graph is
// transient). At the smaller fractional-vCPU tiers the collector competes with the
// handler for the one core, so the warm P99/P99.9 tail opens up while the median
// stays flatter, worst at the starved low-memory tiers and easing as vCPU grows. A
// non-GC runtime frees each replaced buffer immediately, so its tail stays flat.
// Read the absolute tail latencies on the dashboard, not a P99/P50 ratio.
//
// Deliberately an indexed ring of typed arrays, not a Map: the point is to isolate
// the GC/allocator, not to compare Map implementations or hashing speed. No S3, no
// AWS clients, no payload; fully self-contained.

// Number of buffers in the retained live set.
const ENTRIES = 200_000;
// Bytes per buffer; ENTRIES * ENTRY_BYTES ≈ 100 MB of permanently-live heap.
const ENTRY_BYTES = 512;
// Buffers replaced per invoke (garbage generated + new live, set stays full).
const CHURN = 40_000;

// The retained working set + ring cursor, built once at init and mutated in place
// across warm invokes. `live` is never released. That permanence keeps V8's
// tracing GC tracing the whole set every cycle.
const live = new Array(ENTRIES);
for (let i = 0; i < ENTRIES; i++) {
  const b = new Uint8Array(ENTRY_BYTES);
  b[0] = i & 0xff;
  live[i] = b;
}
let rot = 0;

// Replace CHURN entries then scan every 10th entry. The replaced buffers become
// garbage while the live set stays full, so the GC keeps tracing the whole ~100 MB
// set. The scan keeps it genuinely live and read.
function churnAndScan() {
  for (let c = 0; c < CHURN; c++) {
    rot = (rot + 1) % ENTRIES;
    const b = new Uint8Array(ENTRY_BYTES);
    b[0] = c & 0xff;
    live[rot] = b;
  }
  let sum = 0;
  for (let i = 0; i < ENTRIES; i += 10) {
    sum += live[i][0];
  }
  return sum;
}

export const handler = async () => {
  return {
    scenario: "cache",
    entries: ENTRIES,
    churned: CHURN,
    checksum: churnAndScan(),
  };
};
