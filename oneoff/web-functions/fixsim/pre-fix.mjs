// Simulates a wrapper that loads `http` without the ES module facade. Runs before the wrapper
// (NODE_OPTIONS=--import): replaces http's undici-backed lazy getters with ones that return
// undefined, so the wrapper's `import http from "http"` builds its facade without loading undici,
// then restores the originals once the wrapper resolves the entry module. Side effect, for this
// probe only: an ES module importing WebSocket/CloseEvent/MessageEvent from "http" gets undefined.
import { registerHooks } from "node:module";

const http = process.getBuiltinModule("http");
const NAMES = ["WebSocket", "CloseEvent", "MessageEvent"];
const saved = {};
for (const n of NAMES) {
  const d = Object.getOwnPropertyDescriptor(http, n);
  if (!d?.configurable) continue;
  saved[n] = d;
  Object.defineProperty(http, n, { configurable: true, enumerable: d.enumerable, get: () => undefined });
}
const state = (globalThis.__fixsim = { variant: "fix", preloadMs: +performance.now().toFixed(1), stubbed: Object.keys(saved), restoredMs: null });
const hooks = registerHooks({
  resolve(specifier, context, next) {
    const r = next(specifier, context);
    if (state.restoredMs === null && r.url.startsWith("file:///var/task/")) {
      for (const [n, d] of Object.entries(saved)) Object.defineProperty(http, n, d);
      state.restoredMs = +performance.now().toFixed(1);
      queueMicrotask(() => hooks.deregister());
    }
    return r;
  },
});
