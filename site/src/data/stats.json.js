// Build-time data loader: reads the raw lambdabench run (of order 100 MB gz / ~10^6 rows)
// off disk line by line (gunzip + readline), projects each row to the columns
// the charts need, and hands the projection to aggregate() to compute a compact
// JSON (a few hundred KB). The read is streamed; the projected rows are
// materialized (walked several times to build the different aggregate families).
// The only place that touches raw rows, so the heavy file is never shipped.
//
// A thin I/O shell: input discovery, streaming, the meta completeness gate, and
// stdout. All transform logic lives in ../lib/aggregate.js for unit testing.
//
// Framework runs this once and caches the output, re-running only when this
// script or its inputs change. Output goes to stdout (Framework convention).

import {
  createReadStream,
  readdirSync,
  readFileSync,
  existsSync,
} from "node:fs";
import { createGunzip } from "node:zlib";
import { createInterface } from "node:readline";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  aggregate,
  assertRunComplete,
  assertRowsMatchMeta,
} from "../lib/aggregate.js";

const here = dirname(fileURLToPath(import.meta.url));
// results/ lives at the repo root, three levels up from src/data/.
const resultsDir = join(here, "..", "..", "..", "results");

// Pick the input: LAMBDABENCH_RESULTS env override, else the newest *.jsonl(.gz)
// in results/, so a new run renders with no edits.
function findInput() {
  if (process.env.LAMBDABENCH_RESULTS) return process.env.LAMBDABENCH_RESULTS;
  const files = readdirSync(resultsDir)
    .filter((f) => f.endsWith(".jsonl") || f.endsWith(".jsonl.gz"))
    .map((f) => join(resultsDir, f));
  if (files.length === 0) throw new Error(`no results in ${resultsDir}`);
  // Sort descending by the run-id timestamp in the filename (`run-<ms>-<hash>`).
  // Parse <ms> numerically; lexical comparison misorders runs once the ms clock
  // changes digit count.
  const runMs = (p) => {
    const m = p.match(/run-(\d+)-/);
    return m ? Number(m[1]) : 0;
  };
  files.sort((a, b) => runMs(b) - runMs(a));
  return files[0];
}

const inputPath = findInput();
const metaPath = inputPath.replace(/\.jsonl(\.gz)?$/, ".meta.json");
// A real run always writes the meta before the writer, so a results file with no
// adjacent .meta.json is anomalous: without it assertRunComplete sees no
// completeness signals and waves the run through. This is not "an older run with
// no signal" (that is a meta lacking specific fields); the sidecar was lost/moved
// or LAMBDABENCH_RESULTS points at a file with no meta. Fail loud.
if (!existsSync(metaPath)) {
  throw new Error(
    `results file ${inputPath} has no adjacent meta (${metaPath}); cannot verify ` +
      `run completeness. Refusing to publish ungated stats. Restore the .meta.json ` +
      `or point LAMBDABENCH_RESULTS at a run that has one.`,
  );
}
const meta = JSON.parse(readFileSync(metaPath, "utf8"));

// Refuse a known-incomplete run BEFORE streaming the (potentially truncated)
// results file off disk.
assertRunComplete(meta);

// ---- Stream + project ------------------------------------------------------
// Keep only the columns the charts use; retaining ids/nonces/timestamps on every
// row would exhaust memory on a full run.
const rows = [];
{
  const gzipped = inputPath.endsWith(".gz");
  const fileStream = createReadStream(inputPath);
  const input = gzipped
    ? fileStream.pipe(createGunzip())
    : fileStream.setEncoding("utf8");
  const rl = createInterface({ input, crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line.trim()) continue;
    const r = JSON.parse(line);
    rows.push({
      lang: r.lang,
      scenario: r.scenario,
      arch: r.arch,
      memory_mb: r.memory_mb,
      opt: r.opt,
      snapstart: r.snapstart ?? false,
      // Rust-only diagnostic A/B: "off" is the standing matrix, "on" is the
      // narrow jitter=On variant generated only for `oneclient`/`lettercount`.
      // Non-Rust rows are null.
      jitter: r.jitter,
      // `cycle` groups the samples that share a sandbox (one forced cold start +
      // its trailing warm invokes), so the site can report the effective
      // independent count for a warm tail (distinct cycles), far below the raw
      // warm count. See aggregate.js (`warmCycles`) and appendix.md.
      cycle: r.cycle,
      is_cold: r.is_cold,
      init_ms: r.init_ms,
      restore_ms: r.restore_ms,
      duration_ms: r.duration_ms,
      billed_ms: r.billed_ms,
      max_memory_used_mb: r.max_memory_used_mb,
      artifact_unzipped_bytes: r.artifact_unzipped_bytes,
      artifact_zip_bytes: r.artifact_zip_bytes,
    });
  }
}

// Cross-check the rows actually streamed off disk against the count the run
// recorded in its meta. assertRunComplete only compares fields within the meta;
// it cannot see a data file truncated after the run stamped itself complete.
assertRowsMatchMeta(rows.length, meta, inputPath);

const payload = aggregate(rows, meta, {
  inputBasename: inputPath.split("/").pop(),
});

process.stdout.write(JSON.stringify(payload));
