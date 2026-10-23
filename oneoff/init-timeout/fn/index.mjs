// Init sleeps INIT_MS before the runtime asks for the first event. A payload of {"hang": true}
// sleeps past any timeout, so the invoke times out and the next one pays a suppressed init;
// {"exit": true} ends the process instead, and {"oom": true} fills memory until Lambda stops it.
// Each leaves a marker in /tmp (which survives the reset), so that re-init sleeps REINIT_MS
// instead: a short first cold start, then a re-init of any length.
// INIT_WORK / REINIT_WORK (millions of loop iterations) add a fixed amount of CPU work to the
// init, so a slower CPU shows up as a longer init; {"work": N} runs the same loop in the handler.
// The loop also reports its speed per second of wall time (millions of iterations), which shows
// when the CPU share changes. {"sleep": ms} makes the handler wait that long, and every response
// reports context.getRemainingTimeInMillis() at the handler's start.
import { existsSync, writeFileSync } from "node:fs";

const MARKER = "/tmp/hung";
const initId = Math.random().toString(36).slice(2, 10);
const reinit = existsSync(MARKER);
const pick = (name) => (reinit && process.env[`RE${name}`] ? process.env[`RE${name}`] : process.env[name]);

const work = (millions) => {
  const t = performance.now();
  const c = process.cpuUsage();
  const perSecond = [];
  let x = 0;
  let second = t + 1000;
  let done = 0;
  // Blocks of a million iterations, so reading the clock costs nothing measurable.
  for (let m = 0; m < millions; m++) {
    for (let i = m * 1e6; i < (m + 1) * 1e6; i++) x += Math.sqrt(i);
    if (performance.now() >= second) {
      perSecond.push(m + 1 - done);
      done = m + 1;
      second += 1000;
    }
  }
  const cpu = process.cpuUsage(c);
  return {
    wallMs: Math.round(performance.now() - t),
    cpuMs: Math.round((cpu.user + cpu.system) / 1000),
    perSecond: perSecond.join(","),
    x: x > 0,
  };
};

const initStart = Date.now();
await new Promise((r) => setTimeout(r, Number(pick("INIT_MS") || 0)));
const initWork = work(Number(pick("INIT_WORK") || 0));
const initMs = Date.now() - initStart;

export const handler = async (event, context) => {
  const remainingAtStart = context.getRemainingTimeInMillis();
  if (event?.hang || event?.exit || event?.oom) writeFileSync(MARKER, "");
  if (event?.exit) process.exit(1);
  if (event?.oom) for (const held = []; ; ) held.push(Buffer.alloc(16 << 20, 1));
  if (event?.hang) await new Promise((r) => setTimeout(r, 900_000));
  const handlerWork = event?.work ? work(event.work) : undefined;
  if (event?.sleep) await new Promise((r) => setTimeout(r, event.sleep));
  return { initId, initMs, reinit, remainingAtStart, initWork, handlerWork };
};
