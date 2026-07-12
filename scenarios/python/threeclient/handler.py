# Scenario "threeclient": construct THREE AWS SDK clients (DynamoDB, KMS, S3) at
# init and call all three per invoke (DDB GetItem + KMS Encrypt + S3 GetObject).
# Cold cost vs `oneclient` shows what additional AWS clients add (extra middleware
# stacks plus a first TLS handshake per distinct endpoint). Read by direct
# comparison, not subtraction.
# All three clients come from the bundled boto3 for a fair comparison with Rust.

import os

import boto3
from botocore.config import Config

TABLE = os.environ["LAMBDABENCH_TABLE"]
KEY = os.environ["LAMBDABENCH_KEY"]
KMS_KEY_ID = os.environ["LAMBDABENCH_KMS_KEY_ID"]
BUCKET = os.environ["LAMBDABENCH_BUCKET"]
OBJECT_KEY = os.environ["LAMBDABENCH_OBJECT_KEY"]

# Init phase: build all three clients once. total_max_attempts=1 disables SDK
# retries so a throttle/transient surfaces as a hard failure, not a silently-
# retried inflated Duration. A failed run beats wrong data.
_no_retry = Config(retries={"total_max_attempts": 1})
_ddb = boto3.client("dynamodb", config=_no_retry)
_kms = boto3.client("kms", config=_no_retry)
_s3 = boto3.client("s3", config=_no_retry)


def handler(event, context):
    # 1. DynamoDB GetItem. Fail loud if the seeded item is absent: a missing item
    # means a broken benchmark setup, never a null fallback (matches the other
    # languages).
    ddb_out = _ddb.get_item(TableName=TABLE, Key={"pk": {"S": KEY}})
    item = ddb_out.get("Item")
    if not item:
        raise RuntimeError(f"seeded item not found for key {KEY}")

    # 2. KMS Encrypt of a short constant.
    kms_out = _kms.encrypt(KeyId=KMS_KEY_ID, Plaintext=b"hello")

    # 3. S3 GetObject of a small seeded object. Measure the raw byte length, not a
    # decoded str's length: a decode would add per-invoke work the other languages
    # do not do and count code points rather than bytes.
    s3_out = _s3.get_object(Bucket=BUCKET, Key=OBJECT_KEY)
    object_body = s3_out["Body"].read()

    return {
        "scenario": "threeclient",
        "ddb_payload": item.get("payload", {}).get("S"),
        "kms_ciphertext_len": len(kms_out["CiphertextBlob"]),
        "s3_object_len": len(object_body),
    }
