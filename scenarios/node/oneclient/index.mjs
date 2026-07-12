// Scenario "oneclient": construct ONE AWS SDK client (DynamoDB) at init and call
// it once (GetItem) per invoke. Cold cost vs `hello` isolates the cost of a
// single AWS client. @aws-sdk/client-dynamodb is bundled (not runtime-provided)
// for a fair comparison with Rust's statically linked SDK.

import { DynamoDBClient, GetItemCommand } from "@aws-sdk/client-dynamodb";

const TABLE = process.env.LAMBDABENCH_TABLE;
const KEY = process.env.LAMBDABENCH_KEY;

// Init phase: build the client once. maxAttempts:1 disables SDK retries so a
// throttle/transient surfaces as a hard failure, not a silently-retried inflated
// Duration. A failed run beats wrong data.
const client = new DynamoDBClient({ maxAttempts: 1 });

export const handler = async () => {
  const out = await client.send(
    new GetItemCommand({
      TableName: TABLE,
      Key: { pk: { S: KEY } },
    })
  );
  // Fail loud if the seeded item is absent: a missing item means a broken
  // benchmark setup, never a null fallback (matches the other languages).
  if (!out.Item) {
    throw new Error(`seeded item not found for key ${KEY}`);
  }
  return {
    scenario: "oneclient",
    key: KEY,
    payload: out.Item.payload?.S ?? null,
  };
};
