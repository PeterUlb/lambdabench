// Scenario "smithyfull": the realistic shape, a Smithy server framework (via the
// API Gateway adapter) hosting a real CreateOrder handler. The SSDK
// deserializes + validates the request body, the handler does a real write flow
// (KMS Encrypt a signature → DDB PutItem the order → S3 PutObject a receipt),
// and the SSDK serializes a constraint-validated response. Represents a typical
// production request handler.

import { DynamoDBClient, PutItemCommand } from "@aws-sdk/client-dynamodb";
import { KMSClient, EncryptCommand } from "@aws-sdk/client-kms";
import { S3Client, PutObjectCommand } from "@aws-sdk/client-s3";
import { getCoffeeShopServiceHandler, OrderNotFound } from "@com.example/coffee-shop-server";
import { convertEvent, convertResponse } from "@aws-smithy/server-apigateway";

const TABLE = process.env.LAMBDABENCH_TABLE;
const KMS_KEY_ID = process.env.LAMBDABENCH_KMS_KEY_ID;
const BUCKET = process.env.LAMBDABENCH_BUCKET;
const ORDER_PK = process.env.LAMBDABENCH_ORDER_PK;
const RECEIPT_KEY = process.env.LAMBDABENCH_RECEIPT_KEY;

// Init phase: build the three AWS clients and the Smithy service handler once.
// maxAttempts:1 disables SDK retries so a throttle/transient surfaces as a hard
// failure, not a silently-retried inflated Duration. A failed run beats wrong data.
const ddb = new DynamoDBClient({ maxAttempts: 1 });
const kms = new KMSClient({ maxAttempts: 1 });
const s3 = new S3Client({ maxAttempts: 1 });

const coffeeShop = {
  // Trivial stub: the benchmark exercises CreateOrder, not GetMenu.
  GetMenu: async () => ({ items: [{ type: "DRIP", description: "lambdabench" }] }),
  // CreateOrder is the realistic request path. The SSDK has already
  // deserialized + validated the input (coffeeType, required enum). We run a
  // real write flow with fixed keys (idempotent overwrite, no data growth),
  // then return a structured order whose id is a @pattern+@length-constrained
  // Uuid, so the SSDK runs constraint validation while serializing.
  CreateOrder: async (input) => {
    // 1. KMS Encrypt a signature for the order.
    const enc = await kms.send(
      new EncryptCommand({
        KeyId: KMS_KEY_ID,
        Plaintext: Buffer.from(`order:${input.coffeeType}`),
      })
    );

    // 2. DDB PutItem: write the order.
    await ddb.send(
      new PutItemCommand({
        TableName: TABLE,
        Item: {
          pk: { S: ORDER_PK },
          coffeeType: { S: input.coffeeType },
          status: { S: "IN_PROGRESS" },
          signature: { B: enc.CiphertextBlob },
        },
      })
    );

    // 3. S3 PutObject: write a receipt.
    await s3.send(
      new PutObjectCommand({
        Bucket: BUCKET,
        Key: RECEIPT_KEY,
        Body: `receipt for ${input.coffeeType} order`,
      })
    );

    return {
      id: "00000000-0000-0000-0000-000000000000",
      coffeeType: input.coffeeType,
      status: "IN_PROGRESS",
    };
  },
  GetOrder: async () => {
    throw new OrderNotFound({ message: "not implemented in benchmark" });
  },
};

const serviceHandler = getCoffeeShopServiceHandler(coffeeShop);

export const handler = async (event) => {
  const httpRequest = convertEvent(event);
  const httpResponse = await serviceHandler.handle(httpRequest, {});
  return convertResponse(httpResponse);
};
