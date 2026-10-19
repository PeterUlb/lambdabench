// Preloaded through NODE_OPTIONS=--import, so it runs after Node's bootstrap and before the
// wrapper /var/runtime/index.js. Records, as [label, ms since process start, CPU ms used so far,
// major page faults, minor page faults]:
//   - the wrapper's HTTP calls to the runtime API (/load, /ready), via diagnostics_channel
//   - every module resolve and load the ESM loader performs, via module.registerHooks
//   - the first call of `http`'s lazy WebSocket getter (which loads Node's bundled undici)
import dc from "node:diagnostics_channel";
import { registerHooks } from "node:module";

const marks = (globalThis.__probeMarks = []);
const mark = (label) => {
  const cpu = process.cpuUsage();
  const ru = process.resourceUsage();
  marks.push([label, +performance.now().toFixed(1), +((cpu.user + cpu.system) / 1000).toFixed(1), ru.majorPageFault, ru.minorPageFault]);
};
mark("preload");

dc.subscribe("http.client.request.start", ({ request }) => mark(`request ${request.path}`));
dc.subscribe("http.client.response.finish", ({ request }) => mark(`response ${request.path}`));

const short = (u) => u.replace("file:///var/runtime/", "runtime/").replace("file:///var/task/", "task/");
registerHooks({
  resolve(specifier, context, next) {
    const r = next(specifier, context);
    mark(`resolve ${short(r.url)}`);
    return r;
  },
  load(url, context, next) {
    mark(`load> ${short(url)}`);
    const r = next(url, context);
    mark(`load< ${short(url)}`);
    return r;
  },
});

// Wrap the getter without creating http's ES module facade (process.getBuiltinModule is the
// CommonJS-style module object).
const http = process.getBuiltinModule("http");
const d = Object.getOwnPropertyDescriptor(http, "WebSocket");
if (d?.get && d.configurable) {
  let first = true;
  Object.defineProperty(http, "WebSocket", {
    ...d,
    get() {
      if (!first) return d.get.call(this);
      first = false;
      mark("WebSocket getter>");
      const v = d.get.call(this);
      mark("WebSocket getter<");
      return v;
    },
  });
} else {
  mark("WebSocket getter not wrappable");
}
mark("preload done");
