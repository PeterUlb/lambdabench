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

process.stdout.write(readFileSync(input, "utf8"));
