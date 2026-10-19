// Express + DynamoDB GetItem Web Function, used to check what a wrapper that did not load undici
// would save. Bundled with esbuild's defaults for --platform=node (the SDK's CommonJS build).
// Each response reports, in ms since this node process started: the entry's first line, listen(),
// this environment's first request and when its first GetItem finished, whether undici, tls and
// crypto are loaded, the CPU time used, and the host CPU.
const firstLineMs = performance.now();
import express from "express";
import { DynamoDBClient, GetItemCommand } from "@aws-sdk/client-dynamodb";
import { readFileSync } from "node:fs";

const loaded = (m) => process.moduleLoadList.includes(`NativeModule ${m}`);
const snap = () => ({
  undici: loaded("internal/deps/undici/undici"),
  tls: loaded("tls"),
  cpuMs: Math.round((process.cpuUsage().user + process.cpuUsage().system) / 1000),
});
const atFirstLine = snap();
const CPU_PARTS = { "0xd0c": "Graviton2", "0xd40": "Graviton3", "0xd4f": "Graviton4" };
const cpuPart = readFileSync("/proc/cpuinfo", "utf8").match(/^CPU part\s*:\s*(\S+)/m)?.[1];
const cpu = CPU_PARTS[cpuPart] ?? cpuPart ?? "unknown";
const bootId = Math.random().toString(36).slice(2, 10);
const r1 = (v) => (v == null ? v : +v.toFixed(1));

const client = new DynamoDBClient({});
const app = express();
let listenMs, firstReqMs, firstGetDoneMs, firstGetMs, reqN = 0;
let atListen, afterFirstGet;

app.get("/item", async (req, res) => {
  reqN++;
  const t0 = performance.now();
  firstReqMs ??= t0;
  const out = await client.send(new GetItemCommand({ TableName: process.env.TABLE, Key: { pk: { S: "seed" } } }));
  const t1 = performance.now();
  if (firstGetDoneMs === undefined) {
    firstGetDoneMs = t1;
    firstGetMs = t1 - t0;
    afterFirstGet = snap();
  }
  const ms = Math.min(+(req.query.sleep ?? 0), 5000);
  setTimeout(() => {
    res.json({
      variant: process.env.VARIANT,
      bootId,
      cpu,
      reqN,
      item: !!out.Item,
      bootstrapMs: r1(performance.nodeTiming.bootstrapComplete),
      firstLineMs: r1(firstLineMs),
      listenMs: r1(listenMs),
      firstReqMs: r1(firstReqMs),
      firstGetMs: r1(firstGetMs),
      firstGetDoneMs: r1(firstGetDoneMs),
      thisGetMs: r1(t1 - t0),
      atFirstLine,
      atListen,
      afterFirstGet,
      now: snap(),
      fixsim: globalThis.__fixsim ?? null,
    });
  }, ms);
});

app.listen(3000, "0.0.0.0", () => {
  listenMs = performance.now();
  atListen = snap();
});
