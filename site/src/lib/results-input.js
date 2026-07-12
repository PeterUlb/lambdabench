// Shared build-time input discovery for the documentation-probe loaders.
//
// The three probe loaders (data/lifecycle-*.json.js) each read the newest
// run-scoped file from the repo-root `results/` dir, "newest" by the `<unix_ms>`
// segment in the filename (run-id format `<unix_ms>-<8hex>`, see bencher
// `config::run_id`). Centralized here so the three loaders sort identically and
// stay unit-testable. The matrix loader (data/stats.json.js) does the same for
// its `results/run-*` input but keeps its own copy: it also gates on an adjacent
// `.meta.json` and matches a different filename pattern.
//
// `results/*` is gitignored, so at build time the directory holds whatever the
// current pipeline run produced. No match means the caller fails loud (see
// `newestResultsFile`): there is deliberately no committed fallback that could
// ship stale (e.g. laptop-vantage) data.

import { readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// results/ lives at the repo root, three levels up from site/src/lib/.
const here = dirname(fileURLToPath(import.meta.url));
export const RESULTS_DIR = join(here, "..", "..", "..", "results");

// Parse the `<unix_ms>` prefix of a run id so files sort by wall-clock time, not
// lexically (lexical order misranks ids once the ms clock changes digit count).
// Returns 0 for an unparseable name, so a malformed name sorts oldest.
function runMs(name, re) {
  const m = name.match(re);
  return m ? Number(m[1]) : 0;
}

// Strict filename matcher for a probe `kind`. Anchoring on the run-id shape
// `-<digits>-<hex>.json` distinguishes `lifecycle-download-scaling-<id>.json`
// from `lifecycle-download-scaling-image-<id>.json` even though one kind is a
// prefix of the other (the char after `scaling-` is a digit vs the letter "i").
// A helper so loaders and tests share one regex.
export function lifecycleRegex(kind) {
  return new RegExp(`^lifecycle-${kind}-(\\d+)-[0-9a-f]+\\.json$`);
}

// Absolute path of the newest results-dir file matching `re` (capturing
// `<unix_ms>` in group 1). Throws when the dir is missing or holds no match,
// with a message on how to produce one; `label` names the input class in that
// error (e.g. "download-start probe"). An explicit `envPath` short-circuits
// discovery and is returned verbatim. `dir` defaults to the repo-root
// `results/`; tests pass a temp dir to exercise the real scan/sort.
export function newestResultsFile({ re, label, envPath, dir = RESULTS_DIR }) {
  if (envPath) return envPath;
  let entries;
  try {
    entries = readdirSync(dir);
  } catch (err) {
    throw new Error(
      `cannot read results dir ${dir} for the ${label} input: ${err.message}`,
      { cause: err },
    );
  }
  const matches = entries.filter((f) => re.test(f));
  if (matches.length === 0) {
    throw new Error(
      `no ${label} input in ${dir} (expected a file matching ${re}). ` +
        `Run the probe to produce one, or the publish pipeline that runs it. ` +
        `Refusing to build with no data (there is no committed fallback).`,
    );
  }
  matches.sort((a, b) => runMs(b, re) - runMs(a, re));
  return join(dir, matches[0]);
}
