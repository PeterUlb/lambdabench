# Scenario "batch": a deserialize-heavy batch record processor (the canonical
# Kinesis/SQS-consumer shape).
#
# At init the handler fetches a large (~16 MB) JSON array of event records from
# S3 and keeps the raw text in memory. Each warm invoke parses the whole batch
# and groups-by `key` into a dict of running sum + count, then returns the
# per-group totals.
#
# Read this scenario on two independent axes:
#   - MEDIAN = each language's standard JSON-parser speed, which dominates. Python
#     parses with the stdlib json module (C-accelerated, but the resulting objects
#     carry heavy per-object overhead). We keep the stdlib parser; a faster
#     third-party decoder would compare libraries, not languages.
#   - TAIL (P99/P99.9) at the smaller memory tiers = allocation + GC. json.loads
#     builds the full record graph, and the group dict plus the per-group output
#     list are all live simultaneously for the whole invoke, a large transient
#     heap CPython's refcounting + cyclic GC must reclaim. Combined with Python's
#     large per-object footprint this tail is pronounced at the smaller tiers. A
#     non-GC runtime drops it at end of scope.
#
# Contrast lettercount, which counts into a fixed 26-element list (nothing grows).
# Fetching the batch at init keeps the warm measurement pure compute. The group-by
# (parse + dict insert/update + arithmetic) is in-language work, not a native
# library, so the comparison is fair.

import json
import os

import boto3
from botocore.config import Config

BUCKET = os.environ["LAMBDABENCH_BUCKET"]
OBJECT_KEY = os.environ["LAMBDABENCH_BATCH_KEY"]

# total_max_attempts=1 disables SDK retries: a throttle on the init S3 fetch must
# fail hard rather than inflate init via a silent retry. Failed run beats wrong data.
_s3 = boto3.client("s3", config=Config(retries={"total_max_attempts": 1}))

# Init phase: fetch the batch from S3 once and hold the raw text in memory.
_payload = _s3.get_object(Bucket=BUCKET, Key=OBJECT_KEY)["Body"].read().decode("utf-8")


def handler(event, context):
    # Parse the whole batch (allocates the full record graph, the GC fuel), then
    # group-by key into sum + count. The dict and the parsed records are all live
    # for the duration of the call.
    records = json.loads(_payload)
    groups = {}
    total = 0
    for i, r in enumerate(records):
        # Validate each record's shape so a malformed batch is a hard error, not
        # silently wrong output, matching the typed languages that reject bad data
        # at deserialization. `bool` is a subclass of `int`, so reject it
        # explicitly to keep `value` a real number like the other languages.
        if (
            not isinstance(r, dict)
            or not isinstance(r.get("key"), str)
            or not isinstance(r.get("value"), (int, float))
            or isinstance(r.get("value"), bool)
        ):
            raise ValueError(f"batch record {i} malformed: {r!r}")
        k = r["key"]
        agg = groups.get(k)
        if agg is None:
            agg = [0, 0]  # [sum, count]
            groups[k] = agg
        v = r["value"]
        agg[0] += v
        agg[1] += 1
        total += v
    # Build the per-group summary (allocation proportional to group count),
    # mirroring what a real batch processor hands downstream.
    per_group = [{"key": k, "sum": agg[0], "count": agg[1]} for k, agg in groups.items()]
    return {
        "scenario": "batch",
        "records": len(records),
        "groups": len(groups),
        "total": total,
        "per_group": per_group,
    }
