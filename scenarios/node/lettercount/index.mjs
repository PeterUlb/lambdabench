// Scenario "lettercount": CPU-bound work with no per-invoke I/O.
//
// At init the handler fetches a JSON document from S3 once (a ~1 MB array of
// ASCII strings) and keeps the raw text in memory. Each warm invoke does pure
// in-memory work: parse the JSON array, and for every string count the
// occurrences of each lowercase ASCII letter (a..z), summing into 26 per-letter
// totals returned as the response.
//
// Why this workload:
//   1. It is in-language CPU work. The counting is a tight loop over each string's
//      UTF-16 code units, running in the V8 JIT (vs Rust machine code) rather than
//      a shared native library. A hashing or JSON.stringify-heavy workload would
//      spend most time in native C++/OpenSSL shared by both, measuring the library
//      not the language.
//   2. JSON.parse rebuilds a fresh object graph each invoke, so under a
//      constrained heap V8 may show GC pauses in the warm tail while a non-GC
//      runtime stays flat.
//
// Fetching the payload at init keeps the warm measurement pure compute. Counting
// is restricted to ASCII a..z so Node (iterating UTF-16 code units) and Rust
// (iterating bytes) do identical work and produce identical totals.

import { S3Client, GetObjectCommand } from "@aws-sdk/client-s3";

const BUCKET = process.env.LAMBDABENCH_BUCKET;
const OBJECT_KEY = process.env.LAMBDABENCH_LETTERCOUNT_KEY;

// maxAttempts:1 disables SDK retries: a throttle on the init S3 fetch must fail
// hard rather than inflate init_ms via a silent retry. Failed run beats wrong data.
const s3 = new S3Client({ maxAttempts: 1 });

// Init phase: fetch the JSON payload from S3 once and hold it in memory.
const payload = await (async () => {
  const out = await s3.send(new GetObjectCommand({ Bucket: BUCKET, Key: OBJECT_KEY }));
  return out.Body.transformToString();
})();

export const handler = async () => {
  // Parse (allocates a fresh object graph, the GC fuel) then count lowercase
  // ASCII letters across all entries into 26 totals (index 0 = 'a' .. 25 = 'z').
  const arr = JSON.parse(payload);
  const totals = new Array(26).fill(0);
  for (let i = 0; i < arr.length; i++) {
    const s = arr[i];
    // Fail hard on a non-string entry rather than miscounting silently, matching
    // the typed languages that reject a mistyped element at deserialization.
    if (typeof s !== "string") {
      throw new Error(`lettercount entry ${i} is not a string: ${JSON.stringify(s)}`);
    }
    for (let j = 0; j < s.length; j++) {
      const c = s.charCodeAt(j);
      if (c >= 97 && c <= 122) totals[c - 97]++;
    }
  }
  return { scenario: "lettercount", letter_counts: totals };
};
