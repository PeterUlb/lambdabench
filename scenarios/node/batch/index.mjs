// Scenario "batch": a deserialize-heavy batch record processor (the canonical
// Kinesis/SQS-consumer shape).
//
// At init the handler fetches a large (~16 MB) JSON array of event records from
// S3 and keeps the raw text in memory. Each warm invoke parses the whole batch
// and groups-by `key` into a Map of running sum + count, then returns the
// per-group totals.
//
// Read this scenario on two independent axes:
//   - MEDIAN = each language's standard JSON-parser speed, which dominates. Node
//     parses with V8's native JSON.parse (fast C++), between Rust's serde and the
//     slower reflection-based parsers. We keep the built-in parser; a faster
//     third-party decoder would compare libraries, not languages.
//   - TAIL (P99/P99.9) at the smaller memory tiers = allocation + GC. JSON.parse
//     builds the full record graph, and the group Map plus the per-group output
//     array are all live simultaneously for the whole invoke, a large transient
//     heap V8 must allocate, promote to old-gen, then collect at invoke end: the
//     major-GC path that can stall the warm tail. A non-GC runtime just drops it.
//
// Contrast lettercount, which counts into a fixed 26-element array (nothing
// grows). Fetching the batch at init keeps the warm measurement pure compute. The
// group-by (parse + Map insert/update + arithmetic) is in-language work, not a
// native library, so the comparison is fair.

import { S3Client, GetObjectCommand } from "@aws-sdk/client-s3";

const BUCKET = process.env.LAMBDABENCH_BUCKET;
const OBJECT_KEY = process.env.LAMBDABENCH_BATCH_KEY;

// maxAttempts:1 disables SDK retries: a throttle on the init S3 fetch must fail
// hard rather than inflate init_ms via a silent retry. Failed run beats wrong data.
const s3 = new S3Client({ maxAttempts: 1 });

// Init phase: fetch the batch from S3 once and hold the raw text in memory.
const payload = await (async () => {
  const out = await s3.send(new GetObjectCommand({ Bucket: BUCKET, Key: OBJECT_KEY }));
  return out.Body.transformToString();
})();

export const handler = async () => {
  // Parse the whole batch (allocates the full record graph, the GC fuel), then
  // group-by key into sum + count. The map and the parsed records are all live
  // for the duration of the call.
  const records = JSON.parse(payload);
  const groups = new Map();
  let total = 0;
  for (let i = 0; i < records.length; i++) {
    const r = records[i];
    // Validate each record's shape rather than letting a missing field flow
    // through as `total += undefined` -> NaN. The typed languages fail on bad
    // data (struct deserialize / missing-key raise), so Node must too: a
    // malformed batch is a hard error, not silently wrong output.
    if (typeof r?.key !== "string" || typeof r.value !== "number") {
      throw new Error(`batch record ${i} malformed: ${JSON.stringify(r)}`);
    }
    let agg = groups.get(r.key);
    if (agg === undefined) {
      agg = { sum: 0, count: 0 };
      groups.set(r.key, agg);
    }
    agg.sum += r.value;
    agg.count += 1;
    total += r.value;
  }
  // Build the per-group summary (allocation proportional to group count),
  // mirroring what a real batch processor hands downstream.
  const perGroup = [];
  for (const [key, agg] of groups) {
    perGroup.push({ key, sum: agg.sum, count: agg.count });
  }
  return {
    scenario: "batch",
    records: records.length,
    groups: groups.size,
    total,
    per_group: perGroup,
  };
};
