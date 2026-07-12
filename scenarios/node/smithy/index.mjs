// Scenario "smithy": host the generated Smithy server SDK behind a Lambda
// handler via the API Gateway adapter, with NO AWS call. GetMenu increments a
// shared in-memory counter (carried on the service context object, the
// idiomatic SSDK shared-state path) and returns a constant, so the cold delta
// versus `hello` isolates pure Smithy server framework overhead.
//
// The bencher invokes this function with a synthetic API Gateway v2 event
// (GET /menu); no real API Gateway is involved.

import { getCoffeeShopServiceHandler, OrderNotFound } from "@com.example/coffee-shop-server";
import { convertEvent, convertResponse } from "@aws-smithy/server-apigateway";

// CoffeeShop operation implementations. Only GetMenu is exercised by the
// benchmark; the others satisfy the service contract. The `ctx` argument is the
// shared state, reused across warm invokes.
const coffeeShop = {
  GetMenu: async (_input, ctx) => {
    ctx.requests = (ctx.requests ?? 0) + 1;
    return { items: [{ type: "DRIP", description: `lambdabench #${ctx.requests}` }] };
  },
  CreateOrder: async (input) => ({
    id: "00000000-0000-0000-0000-000000000000",
    coffeeType: input.coffeeType,
    status: "IN_PROGRESS",
  }),
  GetOrder: async () => {
    throw new OrderNotFound({ message: "not implemented in benchmark" });
  },
};

const serviceHandler = getCoffeeShopServiceHandler(coffeeShop);
// Shared context, constructed once at init and reused across warm invokes.
const ctx = { requests: 0 };

export const handler = async (event) => {
  const httpRequest = convertEvent(event);
  const httpResponse = await serviceHandler.handle(httpRequest, ctx);
  return convertResponse(httpResponse);
};
