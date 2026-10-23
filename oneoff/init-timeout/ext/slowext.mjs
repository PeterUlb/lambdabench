// External extension that takes EXT_INIT_MS to become ready, or EXT_REINIT_MS after the
// function's {"hang"|"exit"|"oom"} payload left /tmp/hung, then waits for events forever.
import { existsSync } from "node:fs";

const API = `http://${process.env.AWS_LAMBDA_RUNTIME_API}/2020-01-01/extension`;
const reg = await fetch(`${API}/register`, {
  method: "POST",
  headers: { "Lambda-Extension-Name": "slowext" },
  body: JSON.stringify({ events: ["INVOKE", "SHUTDOWN"] }),
});
const id = reg.headers.get("lambda-extension-identifier");
const delay = existsSync("/tmp/hung") && process.env.EXT_REINIT_MS ? process.env.EXT_REINIT_MS : process.env.EXT_INIT_MS;
await new Promise((r) => setTimeout(r, Number(delay || 0)));
console.log(JSON.stringify({ ext: "ready", delay: Number(delay || 0) }));
for (;;) {
  const ev = await (await fetch(`${API}/event/next`, { headers: { "Lambda-Extension-Identifier": id } })).json();
  if (ev.eventType === "SHUTDOWN") process.exit(0);
}
