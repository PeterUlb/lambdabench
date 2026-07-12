# Scenario "lettercount": CPU-bound work with no per-invoke I/O.
#
# At init the handler fetches a JSON document from S3 once (a ~1 MB array of
# ASCII strings) and keeps the raw text in memory. Each warm invoke does pure
# in-memory work: parse the JSON array, and for every string count the
# occurrences of each lowercase ASCII letter (a..z), summing into 26 per-letter
# totals returned as the response.
#
# Why this workload:
#   1. It is in-language CPU work. The counting is a tight loop over each string's
#      characters, running in CPython's bytecode interpreter (vs Rust machine code
#      / the V8 JIT) rather than a shared native library. A hashing-heavy workload
#      would spend most time in native C/OpenSSL shared by both, measuring the
#      library not the language.
#   2. json.loads rebuilds a fresh object graph each invoke, so under a constrained
#      heap a GC'd runtime may show pauses in the warm tail while a non-GC runtime
#      stays flat.
#
# Fetching the payload at init keeps the warm measurement pure compute. Counting is
# restricted to ASCII a..z so every language does identical work and produces
# identical totals.

import json
import os

import boto3
from botocore.config import Config

BUCKET = os.environ["LAMBDABENCH_BUCKET"]
OBJECT_KEY = os.environ["LAMBDABENCH_LETTERCOUNT_KEY"]

# total_max_attempts=1 disables SDK retries: a throttle on the init S3 fetch must
# fail hard rather than inflate init via a silent retry. Failed run beats wrong data.
_s3 = boto3.client("s3", config=Config(retries={"total_max_attempts": 1}))

# Init phase: fetch the JSON payload from S3 once and hold it in memory.
_payload = _s3.get_object(Bucket=BUCKET, Key=OBJECT_KEY)["Body"].read().decode("utf-8")


def handler(event, context):
    # Parse (allocates a fresh object graph, the GC fuel) then count lowercase
    # ASCII letters across all entries into 26 totals (index 0 = 'a' .. 25 = 'z').
    arr = json.loads(_payload)
    totals = [0] * 26
    for i, s in enumerate(arr):
        # Fail hard on a non-string entry rather than silently iterating dict keys
        # / miscounting, matching how the typed languages reject a malformed payload.
        if not isinstance(s, str):
            raise ValueError(f"lettercount entry {i} is not a string: {s!r}")
        for ch in s:
            c = ord(ch)
            if 97 <= c <= 122:
                totals[c - 97] += 1
    return {"scenario": "lettercount", "letter_counts": totals}
