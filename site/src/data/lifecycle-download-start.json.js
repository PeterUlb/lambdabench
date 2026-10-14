// Build-time data loader for the Cold Start Anatomy "download + start" table.
//
// The download-start probe writes its already-aggregated JSON into the repo-root
// results/ dir under a run-scoped name (gitignored). This loader discovers the
// newest such file and emits it verbatim; the probe already reduced to p50s, so
// there is no raw->aggregate transform. Fails loud when none exists. Framework
// serves stdout as data/lifecycle-download-start.json.

import { readFileSync } from "node:fs";
import { lifecycleRegex, newestResultsFile } from "../lib/results-input.js";

const input = newestResultsFile({
  re: lifecycleRegex("download-start"),
  label: "download-start probe",
  envPath: process.env.LAMBDABENCH_LIFECYCLE_DOWNLOAD_START,
});

const raw = readFileSync(input, "utf8");

// The "What a caller waits for end to end" section renders the caller-wait
// fields (w_warm_p50, the W_cold/W_warm min-max spreads, caller_warm_*); an output lacking
// them comes from an outdated probe binary. Stale data is handled like missing
// data (see lib/results-input.js): refuse to build rather than
// publish a placeholder. check-lifecycle-prose.py flags the same condition
// with the same re-run pointer.
// The production-caller fields (caller_warm_*: warm invokes WITHOUT the log
// tail) feed the same section's no-tail lump and the warm-floor callout's p50/p99.
const waitFields = [
  "w_warm_p50",
  "w_cold_min",
  "w_cold_max",
  "w_warm_min",
  "w_warm_max",
  "caller_warm_p50",
  "caller_warm_p90",
  "caller_warm_p99",
  "caller_warm_min",
  "caller_warm_max",
];
const stale = (JSON.parse(raw).cells ?? []).some((c) =>
  waitFields.some((f) => !Number.isFinite(c[f])),
);
if (stale) {
  throw new Error(
    `${input} predates the caller-wait fields (${waitFields.join(", ")}). ` +
      `Re-run the probe (cargo run -p bencher -- probe download-start) against a ` +
      `deployed matrix. Refusing to build with stale-schema data.`,
  );
}

process.stdout.write(raw);
