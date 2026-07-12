# Scenario "oneclient": construct ONE AWS SDK client (DynamoDB) at init and call
# it once (GetItem) per invoke. Cold cost vs `hello` isolates the cost of a
# single AWS client. boto3 is bundled (not runtime-provided) for a fair
# comparison with Rust's statically linked SDK and Node's bundled @aws-sdk.

import os

import boto3
from botocore.config import Config

TABLE = os.environ["LAMBDABENCH_TABLE"]
KEY = os.environ["LAMBDABENCH_KEY"]

# Init phase: build the client once. total_max_attempts=1 disables SDK retries
# (1 = the initial attempt, no retries) so a throttle/transient surfaces as a hard
# failure, not a silently-retried inflated Duration. A failed run beats wrong data.
_client = boto3.client("dynamodb", config=Config(retries={"total_max_attempts": 1}))


def handler(event, context):
    out = _client.get_item(TableName=TABLE, Key={"pk": {"S": KEY}})
    item = out.get("Item")
    # Fail loud if the seeded item is absent: a missing item means a broken
    # benchmark setup, never a null fallback (matches the other languages).
    if not item:
        raise RuntimeError(f"seeded item not found for key {KEY}")
    return {
        "scenario": "oneclient",
        "key": KEY,
        "payload": item.get("payload", {}).get("S"),
    }
