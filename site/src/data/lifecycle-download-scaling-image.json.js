// Build-time data loader for the Cold Start Anatomy "Zip vs container image"
// chart. Discovers the newest
// results/lifecycle-download-scaling-image-<run_id>.json (gitignored, written by
// `probe download-scaling --with-image`, already aggregated and carrying its own
// co-measured zip_baseline) and emits it verbatim; fails loud when none exists.
// Served as data/lifecycle-download-scaling-image.json.

import { readFileSync } from "node:fs";
import { lifecycleRegex, newestResultsFile } from "../lib/results-input.js";

const input = newestResultsFile({
  re: lifecycleRegex("download-scaling-image"),
  label: "download-scaling container-image probe",
  envPath: process.env.LAMBDABENCH_LIFECYCLE_DOWNLOAD_SCALING_IMAGE,
});

process.stdout.write(readFileSync(input, "utf8"));
