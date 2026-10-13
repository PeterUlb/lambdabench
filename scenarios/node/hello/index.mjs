// Scenario "hello": return a constant.
// Baseline isolating Node runtime startup + handler dispatch, no I/O, no SDK.
// The returned constant matches the other languages' hello output, so only
// runtime overhead differs.
// Node 24 requires async handlers (callback style was removed).

import { reportIfRequested } from "../lib/cpuinfo.mjs";

export const handler = async (event, context) => {
  reportIfRequested(context);
  return { message: "hello", scenario: "hello" };
};
