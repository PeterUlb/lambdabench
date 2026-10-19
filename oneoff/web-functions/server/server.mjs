// Web Function probe server. Wraps a benchmark scenario handler (./scenario.mjs, a copy of
// scenarios/node/dist/<scenario>/index.mjs) behind node:http, so the per-request work is the
// same as the Event Function scenario. Every response carries headers that identify the
// execution environment and its process-relative timestamps, so a client can tell a cold
// request (x-req-n: 1) from a warm one and count environments. x-peer is the address the
// connection to the server came from.
//
// Routes:
//   /run          call the scenario handler with ./event.json (or {}) as the event
//   /sleep?ms=N   wait N ms without using CPU (I/O-bound stand-in), N <= 5000
//   /cpu?ms=N     burn N ms of synchronous CPU (blocks the event loop), N <= 1000
//   /info         runtime introspection: Node flags, CPU, memory, processes, mounts
//   /bg/start?spin=1&net=1   start the background-work probe (background.mjs) in this environment
//   /bg/status    what the background probe has observed so far
const T_MODULE = performance.now(); // ms since this node process started (performance.timeOrigin)
import http from "node:http";
import { randomUUID } from "node:crypto";
import fs from "node:fs";
import os from "node:os";

const BOOT_ID = randomUUID().slice(0, 8);
let scenario = null;
try {
  scenario = (await import("./scenario.mjs")).handler;
} catch (e) {
  if (e.code !== "ERR_MODULE_NOT_FOUND") throw e;
}
const EVENT_URL = new URL("./event.json", import.meta.url);
const EVENT = fs.existsSync(EVENT_URL) ? JSON.parse(fs.readFileSync(EVENT_URL, "utf8")) : {};
const T_SCENARIO_LOADED = performance.now();

let reqN = 0;
let inflight = 0;
let maxInflight = 0;
let firstReqAt = null;
let listenAt = -1;
let bg = null; // background.mjs, imported on the first /bg/start

const read = (p) => {
  try {
    return fs.readFileSync(p, "utf8");
  } catch (e) {
    return `ERR ${e.code}`;
  }
};
const busy = (ms) => {
  const end = performance.now() + ms;
  let x = 0;
  while (performance.now() < end) x += Math.sqrt(x + 1);
  return x;
};
const clamp = (v, max) => Math.min(Math.max(+v || 0, 0), max);

function info() {
  const processes = fs
    .readdirSync("/proc")
    .filter((d) => /^\d+$/.test(d))
    .map((pid) => ({ pid: +pid, cmd: read(`/proc/${pid}/cmdline`).replace(/\0/g, " ").trim() }));
  return {
    node: process.versions.node,
    argv: process.argv,
    execArgv: process.execArgv,
    pid: process.pid,
    cwd: process.cwd(),
    // Only the runtime's own non-secret variables.
    env: Object.fromEntries(
      Object.entries(process.env).filter(([k]) => /^AWS_(LAMBDA_|EXECUTION_ENV|REGION)/.test(k)),
    ),
    cpus: os.cpus().length,
    cpuModel: os.cpus()[0]?.model,
    totalmem: os.totalmem(),
    kernel: os.release(),
    processes,
    mounts: read("/proc/mounts"),
    limits: read("/proc/self/limits"),
  };
}

const server = http.createServer(async (req, res) => {
  const tIn = performance.now();
  const n = ++reqN;
  if (firstReqAt === null) firstReqAt = tIn;
  inflight++;
  if (inflight > maxInflight) maxInflight = inflight;
  const seenInflight = inflight;
  const url = new URL(req.url, "http://x");
  let status = 200;
  let body;
  try {
    switch (url.pathname) {
      case "/run":
        body = await scenario(EVENT, {});
        break;
      case "/sleep":
        await new Promise((r) => setTimeout(r, clamp(url.searchParams.get("ms") ?? 1000, 5000)));
        body = { slept: true };
        break;
      case "/cpu":
        busy(clamp(url.searchParams.get("ms") ?? 100, 1000));
        body = { burned: true };
        break;
      case "/info":
        body = info();
        break;
      case "/bg/start":
        bg ??= await import("./background.mjs");
        body = bg.start({ spin: url.searchParams.get("spin") === "1", net: url.searchParams.get("net") === "1" });
        break;
      case "/bg/status":
        body = bg ? bg.status() : { started: false };
        break;
      default:
        body = { ok: true };
    }
  } catch (e) {
    status = 500;
    body = { error: String(e?.message ?? e) };
  }
  const tOut = performance.now();
  inflight--;
  const payload = JSON.stringify(body);
  // An explicit Content-Length keeps the response in one piece (no chunked encoding).
  res.writeHead(status, {
    "content-type": "application/json",
    "content-length": Buffer.byteLength(payload),
    "x-boot-id": BOOT_ID,
    "x-req-n": n,
    "x-inflight": seenInflight,
    "x-max-inflight": maxInflight,
    "x-uptime-ms": tOut.toFixed(1),
    "x-handler-ms": (tOut - tIn).toFixed(2),
    "x-module-start-ms": T_MODULE.toFixed(1),
    "x-scenario-loaded-ms": T_SCENARIO_LOADED.toFixed(1),
    "x-listen-ms": listenAt.toFixed(1),
    "x-first-req-ms": firstReqAt.toFixed(1),
    "x-peer": req.socket.remoteAddress, // where Lambda's connection to the server comes from
    "x-cpu": hostCpu,
  });
  res.end(payload);
  if (url.pathname === "/bg/start") bg?.scheduleAfterResponse();
});

process.on("SIGTERM", () => {
  console.log({ evt: "sigterm", bootId: BOOT_ID, reqN, maxInflight, uptimeMs: performance.now(), bg: bg?.summary() });
  process.exit(0);
});

const [host, port] = (process.env.AWS_LAMBDA_HTTP_ENDPOINT || "127.0.0.1:3000").split(":");
server.keepAliveTimeout = 65000;
// Host CPU generation from the Arm "CPU part" (as lambdabench reads it), after listen().
const CPU_PARTS = { "0xd0c": "Graviton2", "0xd40": "Graviton3", "0xd4f": "Graviton4" };
let hostCpu = "unknown";
server.listen(+port, host, () => {
  listenAt = performance.now();
  const part = fs.readFileSync("/proc/cpuinfo", "utf8").match(/^CPU part\s*:\s*(\S+)/m)?.[1];
  hostCpu = CPU_PARTS[part] ?? part ?? os.cpus()[0]?.model ?? "unknown";
  console.log({ evt: "listening", bootId: BOOT_ID, moduleStartMs: T_MODULE, listenMs: listenAt });
});
