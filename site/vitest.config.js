import { defineConfig } from "vitest/config";

// Tests live under test/ so vitest never scans src/, which holds Observable
// Framework pages (.md) and double-extension data loaders (e.g. stats.json.js)
// that are not meant to be collected as test files. jsdom is opt-in per file
// via the `// @vitest-environment jsdom` pragma; node is the default.
export default defineConfig({
  test: {
    include: ["test/**/*.test.js"],
    environment: "node",
  },
});
