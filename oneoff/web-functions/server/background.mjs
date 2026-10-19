// Background-work probe: what keeps running in a Web Function environment between requests.
// Loaded by server.mjs on the first /bg/start, so it adds nothing to the other probes' cold starts.
// Starts, once per environment:
//   - a main-thread setInterval (1 s): ticks vs elapsed, largest gap on the monotonic and wall clock
//   - a worker thread with its own 1 s timer and, with spin=1, a busy loop counting iterations
//   - a child process that writes a tick counter to /tmp every second
//   - a fire-and-forget task 5 s after the /bg/start response
//   - with net=1, an outbound HTTPS request every 10 s
//   - a log line every 30 s, and a final summary on SIGTERM (server.mjs calls summary())
import { Worker } from "node:worker_threads";
import { spawn } from "node:child_process";
import https from "node:https";
import fs from "node:fs";

const CHILD_FILE = "/tmp/bg-child-ticks";
const s = {
  startedMono: 0,
  startedWall: 0,
  options: {},
  timer: { ticks: 0, maxGapMono: 0, maxGapWall: 0, lastMono: 0, lastWall: 0 },
  worker: { ticks: 0, maxGapMono: 0, spinPerSec: [] },
  afterResponse: { scheduledMono: 0, firedAfterMs: null },
  net: { ok: 0, fail: 0, lastMs: null, lastError: null },
  child: { pid: null },
};

export function start({ spin = false, net = false }) {
  if (s.startedMono) return status();
  s.startedMono = performance.now();
  s.startedWall = Date.now();
  s.options = { spin, net };

  s.timer.lastMono = s.startedMono;
  s.timer.lastWall = s.startedWall;
  setInterval(() => {
    const m = performance.now();
    const w = Date.now();
    s.timer.ticks++;
    s.timer.maxGapMono = Math.max(s.timer.maxGapMono, m - s.timer.lastMono);
    s.timer.maxGapWall = Math.max(s.timer.maxGapWall, w - s.timer.lastWall);
    s.timer.lastMono = m;
    s.timer.lastWall = w;
  }, 1000);

  const worker = new Worker(
    `
    const { parentPort, workerData } = require("node:worker_threads");
    let last = performance.now();
    setInterval(() => {
      const m = performance.now();
      parentPort.postMessage({ tick: true, gap: m - last });
      last = m;
    }, 1000);
    if (workerData.spin) {
      // Busy loop in 100 ms slices, yielding to the worker's event loop so its timer still fires.
      let iters = 0, windowStart = performance.now();
      const slice = () => {
        const end = performance.now() + 100;
        let x = 0;
        while (performance.now() < end) { x += Math.sqrt(iters++); }
        const m = performance.now();
        if (m - windowStart >= 1000) {
          parentPort.postMessage({ spinPerSec: Math.round(iters * 1000 / (m - windowStart)) });
          iters = 0; windowStart = m;
        }
        setImmediate(slice);
      };
      slice();
    }`,
    { eval: true, workerData: { spin } },
  );
  worker.on("message", (msg) => {
    if (msg.tick) {
      s.worker.ticks++;
      s.worker.maxGapMono = Math.max(s.worker.maxGapMono, msg.gap);
    }
    if (msg.spinPerSec !== undefined) {
      s.worker.spinPerSec.push(msg.spinPerSec);
      if (s.worker.spinPerSec.length > 600) s.worker.spinPerSec.shift();
    }
  });
  worker.unref();

  fs.writeFileSync(CHILD_FILE, "0");
  const child = spawn(
    process.execPath,
    ["-e", `let n=0; setInterval(() => require("fs").writeFileSync(${JSON.stringify(CHILD_FILE)}, String(++n)), 1000);`],
    { stdio: "ignore" },
  );
  s.child.pid = child.pid;
  child.unref();

  if (net) {
    setInterval(() => {
      const t0 = performance.now();
      https
        .get("https://checkip.amazonaws.com/", { timeout: 3000 }, (res) => {
          res.resume();
          res.on("end", () => {
            s.net.ok++;
            s.net.lastMs = +(performance.now() - t0).toFixed(1);
          });
        })
        .on("error", (e) => {
          s.net.fail++;
          s.net.lastError = e.message;
        });
    }, 10000);
  }

  setInterval(() => console.log({ evt: "bg", ...summary() }), 30000);
  return status();
}

// Called right after the /bg/start response has been sent.
export function scheduleAfterResponse() {
  if (s.afterResponse.scheduledMono) return;
  s.afterResponse.scheduledMono = performance.now();
  setTimeout(() => {
    s.afterResponse.firedAfterMs = +(performance.now() - s.afterResponse.scheduledMono).toFixed(1);
  }, 5000);
}

export function summary() {
  if (!s.startedMono) return { started: false };
  const elapsedMono = performance.now() - s.startedMono;
  const elapsedWall = Date.now() - s.startedWall;
  let childTicks = null;
  try {
    childTicks = +fs.readFileSync(CHILD_FILE, "utf8");
  } catch {
    // child never wrote
  }
  let childAlive = false;
  try {
    process.kill(s.child.pid, 0);
    childAlive = true;
  } catch {
    // gone
  }
  const spin = s.worker.spinPerSec;
  return {
    started: true,
    options: s.options,
    elapsedSec: +(elapsedMono / 1000).toFixed(1),
    wallMinusMonoMs: Math.round(elapsedWall - elapsedMono),
    timer: { ticks: s.timer.ticks, maxGapMonoMs: Math.round(s.timer.maxGapMono), maxGapWallMs: Math.round(s.timer.maxGapWall) },
    worker: {
      ticks: s.worker.ticks,
      maxGapMonoMs: Math.round(s.worker.maxGapMono),
      spinPerSecLast: spin.at(-1) ?? null,
      spinPerSecMin: spin.length ? Math.min(...spin) : null,
      spinPerSecMax: spin.length ? Math.max(...spin) : null,
    },
    child: { pid: s.child.pid, alive: childAlive, ticks: childTicks },
    afterResponseFiredAfterMs: s.afterResponse.firedAfterMs,
    net: s.net,
  };
}

export const status = summary;
