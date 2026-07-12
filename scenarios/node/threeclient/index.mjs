// Scenario "threeclient": construct THREE AWS SDK clients (DynamoDB, KMS, S3) at
// init and call all three per invoke (DDB GetItem + KMS Encrypt + S3 GetObject).
// Cold cost vs `oneclient` shows what additional AWS clients add (extra middleware
// stacks plus a first TLS handshake per distinct endpoint). Read by direct
// comparison, not subtraction.
// All three SDK clients are bundled for a fair comparison with Rust.

import { DynamoDBClient, GetItemCommand } from "@aws-sdk/client-dynamodb";
import { KMSClient, EncryptCommand } from "@aws-sdk/client-kms";
import { S3Client, GetObjectCommand } from "@aws-sdk/client-s3";

const TABLE = process.env.LAMBDABENCH_TABLE;
const KEY = process.env.LAMBDABENCH_KEY;
const KMS_KEY_ID = process.env.LAMBDABENCH_KMS_KEY_ID;
const BUCKET = process.env.LAMBDABENCH_BUCKET;
const OBJECT_KEY = process.env.LAMBDABENCH_OBJECT_KEY;

// Init phase: build all three clients once. maxAttempts:1 disables SDK retries
// so a throttle/transient surfaces as a hard failure, not a silently-retried
// inflated Duration. A failed run beats wrong data.
const ddb = new DynamoDBClient({ maxAttempts: 1 });
const kms = new KMSClient({ maxAttempts: 1 });
const s3 = new S3Client({ maxAttempts: 1 });

export const handler = async () => {
  // 1. DynamoDB GetItem. Fail loud if the seeded item is absent: a missing item
  // means a broken benchmark setup, never a null fallback (matches the other
  // languages).
  const ddbOut = await ddb.send(
    new GetItemCommand({ TableName: TABLE, Key: { pk: { S: KEY } } })
  );
  if (!ddbOut.Item) {
    throw new Error(`seeded item not found for key ${KEY}`);
  }

  // 2. KMS Encrypt of a short constant.
  const kmsOut = await kms.send(
    new EncryptCommand({ KeyId: KMS_KEY_ID, Plaintext: Buffer.from("hello") })
  );

  // 3. S3 GetObject of a small seeded object. Read as raw bytes and measure the
  // byte length, matching the other languages: a UTF-8 decode would add per-invoke
  // work they do not do and count UTF-16 code units rather than bytes.
  const s3Out = await s3.send(
    new GetObjectCommand({ Bucket: BUCKET, Key: OBJECT_KEY })
  );
  const objectBody = await s3Out.Body.transformToByteArray();

  return {
    scenario: "threeclient",
    ddb_payload: ddbOut.Item.payload?.S ?? null,
    kms_ciphertext_len: kmsOut.CiphertextBlob?.length ?? 0,
    s3_object_len: objectBody.length,
  };
};
