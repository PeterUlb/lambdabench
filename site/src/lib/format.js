// Human-facing formatters and labels. Pure; shared by loader and client.

import { isSnapLang, baseLang } from "./series.js";

export const titleCase = (s) => s.charAt(0).toUpperCase() + s.slice(1);

// Latency in ms: two decimals below 10 ms (sub-10 values are where the cheap
// scenarios live), whole milliseconds above.
export const fmtMs = (v) =>
  v == null ? "—" : v < 10 ? `${v.toFixed(2)}ms` : `${v.toFixed(0)}ms`;

// Artifact size: bytes / KB / MB by magnitude.
export const fmtBytes = (b) =>
  b == null
    ? "—"
    : b >= 1e6
      ? `${(b / 1e6).toFixed(1)} MB`
      : b >= 1e3
        ? `${(b / 1e3).toFixed(0)} KB`
        : `${Math.round(b)} B`;

export const fmtUsd = (v) =>
  v == null ? "—" : v < 1 ? `$${v.toFixed(3)}` : `$${v.toFixed(2)}`;

// UTC calendar date (yyyy-mm-dd) of a unix-ms timestamp. Returns null for a
// missing or non-positive input so callers can omit the date rather than render
// a bogus epoch one.
export const fmtDateUtc = (unixMs) =>
  Number.isFinite(unixMs) && unixMs > 0
    ? new Date(unixMs).toISOString().slice(0, 10)
    : null;

// Human label for a language/series key. A SnapStart pseudo-language
// (`<runtime>-snapstart`) renders as "<Runtime> SnapStart" for any runtime;
// every other key is just capitalized.
export const langLabel = (l) =>
  isSnapLang(l) ? `${titleCase(baseLang(l))} SnapStart` : titleCase(l);

// Human label for a "lang arch" series key (e.g. "java-snapstart arm64" ->
// "Java SnapStart arm64"). Humanizes the language half via langLabel, leaves the
// arch as-is; a key with no space (a bare language) falls through to langLabel.
// Shared by charts and tables so the rendering of a key cannot drift.
export const seriesLabel = (s) => {
  const sep = s.lastIndexOf(" ");
  return sep === -1
    ? langLabel(s)
    : `${langLabel(s.slice(0, sep))} ${s.slice(sep + 1)}`;
};

// Known scenario display names + the short labels used on dense axes. Any
// scenario not listed falls back to its raw id so new scenarios still appear.
export const SCENARIO_LABELS = {
  hello: "Hello World",
  smithy: "Smithy server (framework)",
  oneclient: "1 AWS client (DDB)",
  threeclient: "3 AWS clients (DDB+KMS+S3)",
  smithyfull: "Smithy framework + write flow (realistic)",
  lettercount: "Letter count (CPU)",
  authz: "JWT authorizer (crypto)",
  batch: "Batch parse + group-by (JSON)",
  cache: "Retained cache churn (GC)",
};

export const SHORT_SCENARIO = {
  hello: "hello",
  smithy: "smithy",
  oneclient: "1-client",
  threeclient: "3-client",
  smithyfull: "smithy+IO",
  lettercount: "letters",
  authz: "authz",
  batch: "batch",
  cache: "cache",
};

// One-line "what it does + what it tests" per scenario, shown in the Scenarios
// reference block. Sourced from the handler doc-comments / DESIGN.md.
//
// `does` = the mechanics (verifiable from the handler). `why` = what the
// scenario surfaces, phrased as the question it puts to the data, not the
// answer. The blurbs avoid naming a winner (the chart shows that), singling out
// one runtime where several share a trait, and "isolates X" / "what X adds over
// Y" framing, since these scenarios read by direct comparison, not subtraction
// (their costs do not cleanly add up; see DESIGN.md / README).
//
// PLAIN TEXT ONLY: interpolated into an html`` template (see index.md), which
// escapes into a text node, so backticks and tags render literally, not as
// formatting. Reference other scenarios by name ("the cache scenario").
export const SCENARIO_BLURBS = {
  hello: {
    does: "Bare handler that returns a constant, with no framework and no AWS calls.",
    why: "The startup floor for a conventional handler doing no real work, the baseline the other scenarios build on.",
  },
  smithy: {
    does: "A server framework generated from a Smithy interface definition: routes one request with request/response (de)serialization, no AWS calls.",
    why: "The startup cost a server framework adds on top of a bare handler.",
  },
  oneclient: {
    does: "Constructs one AWS SDK client (DynamoDB) at init and does a GetItem per invoke.",
    why: "The startup cost of constructing and calling a single AWS SDK client.",
  },
  threeclient: {
    does: "Constructs three SDK clients at init, then per invoke does DDB GetItem, KMS Encrypt, S3 GetObject.",
    why: "How startup cost scales when a handler constructs three SDK clients.",
  },
  smithyfull: {
    does: "Smithy server framework hosting a CreateOrder write flow: KMS-encrypt, DDB PutItem, S3 PutObject.",
    why: "The realistic production shape: framework plus multiple clients plus real (de)serialization.",
  },
  lettercount: {
    does: "Re-parses a ~1 MB JSON string array (preloaded from S3 at init) and counts a-z per invoke.",
    why: "In-language CPU cost: a tight loop with no per-invoke I/O, so memory (= CPU on Lambda) is the determining factor.",
  },
  authz: {
    does: "RS256 JWT signature verify, base64url/JSON decode, and claim extraction + type-mapping.",
    why: "A realistic authorizer hot path mixing native crypto (RS256 verify) with in-language glue (decode + claim mapping).",
  },
  batch: {
    does: "Re-parses a ~16 MB JSON record batch (preloaded from S3 at init) and groups-by key per invoke.",
    why: "Designed to emphasize standard JSON parsing plus group-by work; parser dominance is inferred, not separately measured (the matrix records only total per-invoke duration). Its object graph is transient, so the dedicated GC probe is the cache scenario.",
  },
  cache: {
    does: "Holds a ~100 MB in-memory working set across warm invokes, replacing a slice of it (and scanning it) each invoke.",
    why: "The dedicated GC probe: a large retained heap a tracing GC re-traces each invoke. On a tracing collector the warm tail can pull away from the median, most at the low memory tiers; more vCPU can reduce it but does not always, while the non-GC and reference-counting runtimes stay closer to the floor. The absolute tail in ms is the figure to read, since a small median inflates the ratio. GC here is inferred from the latency shape, not measured.",
  },
};

// Canonical scenario ordering for the ones we ship; unknown scenarios sort after.
export const KNOWN_SCENARIO_ORDER = [
  "hello",
  "smithy",
  "oneclient",
  "threeclient",
  "smithyfull",
  "lettercount",
  "authz",
  "batch",
  "cache",
];

export const scenarioLabel = (s) => SCENARIO_LABELS[s] ?? s;
export const shortScenario = (s) => SHORT_SCENARIO[s] ?? scenarioLabel(s);
