// In-region probe client, deployed as an Event Function in the same region as the targets.
// Every action times requests from inside the region and returns (or writes to S3) one row per
// request. HTTP timings use one TLS connection per worker: `wait` is request sent -> first
// response byte on an established connection (the TLS handshake is reported separately).
//
// Actions (the event's `action` field):
//   seq          n sequential GETs on one keep-alive connection
//   burst        `concurrency` simultaneous GETs, one fresh connection each
//   load         closed loop: `concurrency` workers GET until `durationMs` (optional step `schedule`)
//   event-invoke force an Event Function cold, then Invoke it once cold + `warm` times warm
//   event-url    force an Event Function cold, then n sequential GETs through its Function URL
// `sign: true` signs HTTP requests with SigV4 (service "lambda"), for IamAuth endpoints.
// With `s3Key`, the rows go to s3://$RESULT_BUCKET/<s3Key> instead of the response.
import https from "node:https";
import { SignatureV4 } from "@smithy/signature-v4";
import { Sha256 } from "@aws-crypto/sha256-js";
import { S3Client, PutObjectCommand } from "@aws-sdk/client-s3";
import {
  LambdaClient,
  InvokeCommand,
  UpdateFunctionConfigurationCommand,
  GetFunctionCommand,
} from "@aws-sdk/client-lambda";

const lambda = new LambdaClient({ maxAttempts: 1 });
const s3 = new S3Client({});
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const now = () => performance.now();

const signer = new SignatureV4({
  service: "lambda",
  region: process.env.AWS_REGION,
  sha256: Sha256,
  credentials: {
    accessKeyId: process.env.AWS_ACCESS_KEY_ID,
    secretAccessKey: process.env.AWS_SECRET_ACCESS_KEY,
    sessionToken: process.env.AWS_SESSION_TOKEN,
  },
});

async function signedHeaders(url) {
  const u = new URL(url);
  const req = await signer.sign({
    method: "GET",
    protocol: "https:",
    hostname: u.hostname,
    path: u.pathname,
    query: Object.fromEntries(u.searchParams),
    headers: { host: u.hostname, "x-amz-content-sha256": "UNSIGNED-PAYLOAD" },
  });
  return req.headers;
}

// One GET with phase timings and the probe server's environment headers.
async function get(url, agent, sign) {
  const headers = sign ? await signedHeaders(url) : {};
  return new Promise((resolve) => {
    const t0 = now();
    let tTls = null;
    const req = https.request(
      url,
      { headers, agent: agent ?? new https.Agent({ keepAlive: false }), timeout: 60000 },
      (res) => {
        const tFirst = now();
        let body = "";
        res.setEncoding("utf8");
        res.on("data", (c) => (body += c));
        res.on("end", () => {
          const h = res.headers;
          const num = (k) => +(h[k] ?? -1);
          const counter = body.match(/#(\d+)/); // Rust/Node smithy echo a per-sandbox request counter
          resolve({
            t0,
            status: res.statusCode,
            total: now() - t0,
            wait: tFirst - (tTls ?? t0),
            tls: tTls ? tTls - t0 : 0,
            boot: h["x-boot-id"] ?? null,
            reqN: h["x-req-n"] ? num("x-req-n") : counter ? +counter[1] : -1,
            inflight: num("x-inflight"),
            uptime: num("x-uptime-ms"),
            handler: num("x-handler-ms"),
            moduleStart: num("x-module-start-ms"),
            listen: num("x-listen-ms"),
            firstReq: num("x-first-req-ms"),
            errType: h["lambda-web-error-type"] ?? null,
            cpu: h["x-cpu"] ?? null,
          });
        });
      },
    );
    req.on("socket", (s) => {
      if (s.connecting) s.once("secureConnect", () => (tTls = now()));
    });
    req.on("timeout", () => req.destroy(new Error("timeout")));
    req.on("error", (e) => resolve({ t0, status: -1, total: now() - t0, err: e.message }));
    req.end();
  });
}

async function seq({ url, n, sign = false }) {
  const agent = new https.Agent({ keepAlive: true, maxSockets: 1 });
  const out = [];
  for (let i = 0; i < n; i++) out.push(await get(url, agent, sign));
  agent.destroy();
  return out;
}

async function burst({ url, concurrency, sign = false }) {
  const T0 = now();
  const res = await Promise.all(Array.from({ length: concurrency }, () => get(url, null, sign)));
  return res.map((r) => ({ ...r, t0: r.t0 - T0 }));
}

async function load({ url, concurrency, durationMs, schedule, sign = false }) {
  const agent = new https.Agent({ keepAlive: true, maxSockets: 2000 });
  const T0 = now();
  const out = [];
  const target = (t) => {
    if (!schedule) return concurrency;
    let c = 0;
    for (const [at, n] of schedule) if (t >= at) c = n;
    return c;
  };
  const workers = schedule ? Math.max(...schedule.map((s) => s[1])) : concurrency;
  await Promise.all(
    Array.from({ length: workers }, async (_, i) => {
      while (now() - T0 < durationMs) {
        if (i >= target(now() - T0)) {
          await sleep(20);
          continue;
        }
        const r = await get(url, agent, sign);
        r.t0 -= T0;
        r.worker = i;
        out.push(r);
        if (r.status === 429 || r.status === 503 || r.status < 0) await sleep(50);
      }
    }),
  );
  agent.destroy();
  return out;
}

// Force a cold start by changing an environment variable. Reads and readiness polls use
// GetFunction, which has its own quota, so only the Update lands on the account-wide 15 req/s
// control-plane bucket that bencher also uses.
async function forceCold(fn, tag) {
  const cfg = (await lambda.send(new GetFunctionCommand({ FunctionName: fn }))).Configuration;
  const env = { ...(cfg.Environment?.Variables ?? {}), COLD_NONCE: tag };
  await lambda.send(
    new UpdateFunctionConfigurationCommand({ FunctionName: fn, Environment: { Variables: env } }),
  );
  for (;;) {
    await sleep(500);
    const c = (await lambda.send(new GetFunctionCommand({ FunctionName: fn }))).Configuration;
    if (c.LastUpdateStatus === "Successful") return;
  }
}

async function eventInvoke({ fn, cycles, warm = 5, payload = "{}", tail = true, minGapMs = 3000 }) {
  const out = [];
  for (let c = 0; c < cycles; c++) {
    const tStart = Date.now();
    await forceCold(fn, `${Date.now()}-${c}`);
    for (let i = 0; i <= warm; i++) {
      const t0 = now();
      const r = await lambda.send(
        new InvokeCommand({
          FunctionName: fn,
          LogType: tail ? "Tail" : "None",
          Payload: Buffer.from(payload),
        }),
      );
      const wall = now() - t0;
      const log = Buffer.from(r.LogResult ?? "", "base64").toString();
      const m = (k) => {
        const x = log.match(new RegExp(`${k}: ([\\d.]+)`));
        return x ? +x[1] : null;
      };
      out.push({
        cycle: c,
        i,
        cold: i === 0,
        wall,
        init: m("Init Duration"),
        dur: m("Duration"),
        fnErr: r.FunctionError ?? null,
      });
    }
    const el = Date.now() - tStart;
    if (el < minGapMs) await sleep(minGapMs - el);
  }
  return out;
}

async function eventUrl({ fn, url, cycles, n = 6, minGapMs = 3000 }) {
  const out = [];
  for (let c = 0; c < cycles; c++) {
    const tStart = Date.now();
    await forceCold(fn, `${Date.now()}-${c}`);
    out.push({ cycle: c, reqs: await seq({ url, n }) });
    const el = Date.now() - tStart;
    if (el < minGapMs) await sleep(minGapMs - el);
  }
  return out;
}

const ACTIONS = {
  seq,
  burst,
  load,
  "event-invoke": eventInvoke,
  "event-url": eventUrl,
};

export const handler = async (ev) => {
  const run = ACTIONS[ev.action];
  if (!run) throw new Error(`unknown action ${ev.action}`);
  const rows = await run(ev);
  if (!ev.s3Key) return rows;
  await s3.send(
    new PutObjectCommand({
      Bucket: process.env.RESULT_BUCKET,
      Key: ev.s3Key,
      Body: JSON.stringify(rows),
    }),
  );
  return { s3Key: ev.s3Key, n: rows.length };
};
