// Build-time data loader for the Cold Start Anatomy "How far does download
// scale?" chart (zip synthetic sweep). Discovers the newest
// results/lifecycle-download-scaling-<run_id>.json (gitignored, already
// aggregated) and emits it verbatim; fails loud when none exists. Served as
// data/lifecycle-download-scaling.json.
//
// The regex is anchored so it does not also match `download-scaling-image-<id>`
// (see lifecycleRegex): the char after `scaling-` is a digit vs the letter "i".

import { readFileSync } from "node:fs";
import { lifecycleRegex, newestResultsFile } from "../lib/results-input.js";

const input = newestResultsFile({
  re: lifecycleRegex("download-scaling"),
  label: "download-scaling (zip) probe",
  envPath: process.env.LAMBDABENCH_LIFECYCLE_DOWNLOAD_SCALING,
});

process.stdout.write(readFileSync(input, "utf8"));
