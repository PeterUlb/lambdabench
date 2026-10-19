// Wrapper probe: where the time goes before a Web Function's own code runs.
// Each response reports, in ms since this node process started (performance.timeOrigin):
// Node's own startup marks, the runtime API calls the wrapper made (recorded by pre.mjs), this
// module's first line, listen() and the first request. GET /?children=1 also times bare `node`
// processes loading the files in children/, to separate Node's costs from the wrapper's.
const firstLineMs = performance.now();
globalThis.__probeMarks?.push(["entry first line", +firstLineMs.toFixed(1), +((process.cpuUsage().user + process.cpuUsage().system) / 1000).toFixed(1), process.resourceUsage().majorPageFault, process.resourceUsage().minorPageFault]);
import http from "node:http";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";

const round = (v) => (typeof v === "number" ? +v.toFixed(1) : v);
const nodeTiming = Object.fromEntries(
  Object.entries(performance.nodeTiming.toJSON())
    .filter(([, v]) => typeof v === "number")
    .map(([k, v]) => [k, round(v)]),
);
// Host CPU, as lambdabench reads it: the Arm "CPU part" from /proc/cpuinfo (after the timings above).
const CPU_PARTS = { "0xd0c": "Graviton2", "0xd40": "Graviton3", "0xd4f": "Graviton4" };
const cpuPart = readFileSync("/proc/cpuinfo", "utf8").match(/^CPU part\s*:\s*(\S+)/m)?.[1];
const cpu = CPU_PARTS[cpuPart] ?? cpuPart ?? "unknown";
const bootId = Math.random().toString(36).slice(2, 10);
const patched = {
  console: console.log.toString().includes("createLogEntry"),
  httpServerEmit: http.Server.prototype.emit.toString().includes("invokeStore"),
  loadResponseKeys: Object.keys(globalThis.awsLambdaInternal?.loadResponse ?? {}),
};
let listenMs, firstReqMs, reqN = 0;

const CHILDREN = ["empty.cjs", "http.cjs", "empty.mjs", "util-net-os.mjs", "http.mjs", "wrapper-imports.mjs"];
function timeChildren(runs = 5) {
  // Same node binary and flags as the runtime, without the function's NODE_OPTIONS preload.
  return Object.fromEntries(
    CHILDREN.map((f) => [
      f,
      Array.from({ length: runs }, () => {
        const r = spawnSync(process.execPath, [...process.execArgv, new URL(`./children/${f}`, import.meta.url).pathname], {
          encoding: "utf8",
          env: { PATH: process.env.PATH },
        });
        return +r.stdout;
      }),
    ]),
  );
}

const server = http.createServer((req, res) => {
  reqN++;
  firstReqMs ??= performance.now();
  const url = new URL(req.url, "http://x");
  const body = {
    bootId,
    cpu,
    reqN,
    nodeTiming,
    marks: globalThis.__probeMarks ?? null,
    firstLineMs: round(firstLineMs),
    listenMs: round(listenMs),
    firstReqMs: round(firstReqMs),
    patched,
  };
  if (url.searchParams.has("children")) body.children = timeChildren();
  const ms = Math.min(+(url.searchParams.get("sleep") ?? 0), 5000);
  setTimeout(() => {
    const s = JSON.stringify(body);
    res.writeHead(200, { "content-type": "application/json", "content-length": Buffer.byteLength(s) });
    res.end(s);
  }, ms);
});
server.listen(3000, "0.0.0.0", () => {
  listenMs = performance.now();
});
