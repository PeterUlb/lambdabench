// Observable Framework configuration for the Lambda benchmark site.
//
// The data loader (src/data/stats.json.js) aggregates the raw run at BUILD time
// into a compact JSON; every page renders that JSON client-side, so the heavy
// 150 MB / 1.4M-row gz is touched exactly once per build and never shipped.

// Single source of truth for the site's domain. Used to build the canonical
// origin, the title bar, and the social/site-name tags. Defaults to the live
// domain; the LAMBDABENCH_SITE_DOMAIN env var overrides it at build time (e.g.
// to a staging host).
export const DOMAIN = process.env.LAMBDABENCH_SITE_DOMAIN ?? "lambdabench.dev";

// Canonical origin for absolute URLs in canonical/OG/Twitter/JSON-LD tags.
// No trailing slash; page paths (already normalized, e.g. "/" or "/comparison")
// are appended directly.
export const SITE_URL = `https://${DOMAIN}`;

// Public source-repo URL, linked from the footer's "Source on GitHub" link.
// Required: set via LAMBDABENCH_REPO_URL at build time.
export const REPO_URL = process.env.LAMBDABENCH_REPO_URL;
if (!REPO_URL) {
  throw new Error(
    "LAMBDABENCH_REPO_URL is required (e.g. https://github.com/you/lambdabench).",
  );
}

// Contact email, driven by the LAMBDABENCH_CONTACT_EMAIL env var at build time. When
// set, the footer shows a "Contact" mailto link, the only channel for
// corrections/questions until a public repo (with Issues/Discussions) exists.
// Left unset by default so no address is published until one is chosen; note a
// plain mailto in public HTML is exposed to spam scrapers, so use an address
// that can be retired.
export const CONTACT_EMAIL = process.env.LAMBDABENCH_CONTACT_EMAIL ?? null;

// Default description used when a page has no entry in DESCRIPTIONS.
// Keyword-bearing for the head query.
const DEFAULT_DESCRIPTION =
  "An independent AWS Lambda runtime benchmark of Rust, Node.js, Python, Java (plus SnapStart), and Go: cold start, warm latency, cost per million, P99/P99.9 tail, and arm64 vs x86_64.";

// Per-page meta descriptions, keyed by normalized page path. Framework's front
// matter parser drops unknown keys (only an allowlist like `title`/`keywords`
// survives), so a per-page `description:` in the .md would never reach the head
// function; keeping descriptions here is the reliable place for them.
const DESCRIPTIONS = {
  "/": DEFAULT_DESCRIPTION,
  "/comparison":
    "Compare AWS Lambda runtimes head to head: cold and warm latency, cost per million, P99/P99.9 tail, GC behavior, package size, and arm64 vs x86_64 for Rust, Node.js, Python, Java, and Go.",
  "/rust":
    "Two Rust build flags on AWS Lambda, measured per memory tier and CPU architecture: the aws-lc-rs jitter-entropy cold-start tax (AWS_LC_SYS_NO_JITTER_ENTROPY) and opt-level=3 vs opt-level=z.",
  "/lifecycle":
    "Anatomy of an AWS Lambda cold start: the download and environment-start steps no REPORT line shows, zip vs container image size cost, and the Init-phase CPU boost that rewards eager SDK init.",
  "/java-snapstart":
    "How much does AWS Lambda SnapStart cut Java cold starts? Plain JVM vs SnapStart restore latency measured across memory sizes and CPU architectures.",
  "/appendix":
    "AWS Lambda benchmark data appendix: cold vs warm latency distributions and full percentiles (P50/P90/P99/P99.9) per language, scenario, and memory tier, with per-cell sample counts.",
};

// Normalize the path Framework passes to the head function. The index page
// arrives as "/index" (the path normalizer strips the extension then rejoins
// "index"); collapse it to "/" so canonical URLs and lookups are clean.
function normalizePagePath(path) {
  return path === "/index" ? "/" : path;
}

// Map a configured page path to the path actually served. With
// `preserveExtension: true` (see default export), Framework formats every
// internal link as "/foo.html", which maps 1:1 to the built S3 object - so the
// static host needs no clean-URL edge rewrite. The root stays "/" (served via
// CloudFront's defaultRootObject = index.html). Canonical/OG/sitemap URLs are
// built by hand here and in gen-seo-files.js, bypassing Framework's link
// formatter, so they must apply the same ".html" suffix to stay crawlable.
export function servedPath(path) {
  if (path === "/") return "/";
  return path.endsWith(".html") ? path : `${path}.html`;
}

// Build the absolute canonical URL for a page path.
function canonicalUrl(path) {
  return `${SITE_URL}${servedPath(path)}`;
}

// JSON-LD describing the benchmark as a Dataset, emitted only on the homepage.
// Helps search engines classify the site and can surface dataset-rich results.
function datasetJsonLd() {
  const data = {
    "@context": "https://schema.org",
    "@type": "Dataset",
    name: "AWS Lambda Runtime Benchmark: Rust vs Node.js vs Python vs Java vs Go",
    description: DEFAULT_DESCRIPTION,
    url: `${SITE_URL}/`,
    keywords: [
      "AWS Lambda benchmark",
      "Lambda cold start",
      "Lambda warm latency",
      "Lambda cost per million",
      "Lambda P99 tail latency",
      "Lambda arm64 vs x86_64",
      "AWS Graviton Lambda",
      "Lambda package size",
      "Rust Lambda",
      "Node.js Lambda",
      "Python Lambda",
      "Java Lambda SnapStart",
      "Go Lambda",
      "serverless latency",
    ],
    creator: { "@type": "Organization", name: DOMAIN },
    isAccessibleForFree: true,
    measurementTechnique:
      "Repeated cold and warm Lambda invocations across memory tiers and CPU architectures (arm64 and x86_64)",
  };
  return `<script type="application/ld+json">${JSON.stringify(data)}</script>`;
}

// Function-form head: Framework calls this per page with the normalized path,
// the resolved title, and the page's frontmatter (`data`). It centralizes every
// SEO/social tag so individual pages only need a `title` and `description`.
function head({ title, path }) {
  path = normalizePagePath(path);
  const pageTitle = [title, DOMAIN].filter(Boolean).join(" | ");
  const description = DESCRIPTIONS[path] ?? DEFAULT_DESCRIPTION;
  const url = canonicalUrl(path);
  const isHome = path === "/";
  const tags = [
    `<meta name="description" content="${description}">`,
    `<link rel="canonical" href="${url}">`,
    `<meta name="robots" content="index, follow">`,
    // Open Graph (no share image: card renders text-only)
    `<meta property="og:type" content="website">`,
    `<meta property="og:site_name" content="${DOMAIN}">`,
    `<meta property="og:title" content="${pageTitle}">`,
    `<meta property="og:description" content="${description}">`,
    `<meta property="og:url" content="${url}">`,
    // Twitter / X card
    `<meta name="twitter:card" content="summary">`,
    `<meta name="twitter:title" content="${pageTitle}">`,
    `<meta name="twitter:description" content="${description}">`,
    // Favicons (Framework copies + content-hashes assets referenced by href,
    // same as the stylesheet below). A lambda glyph in a teal-cyan-violet
    // gradient drawn from the site's series palette, on the dark page background.
    // SVG for modern browsers; a committed 180x180 PNG as the Apple touch icon
    // (iOS does not render SVG here). The PNG is checked in rather than rendered
    // at build because the runner image has no SVG rasterizer; regenerate from
    // apple-touch-icon.svg with `rsvg-convert -w 180 -h 180`.
    `<link rel="icon" type="image/svg+xml" href="/favicon.svg">`,
    `<link rel="apple-touch-icon" href="/apple-touch-icon.png">`,
    `<link rel="stylesheet" href="/styles.css">`,
  ];
  if (isHome) tags.push(datasetJsonLd());
  return tags.join("\n");
}

export default {
  // Drives the second half of every <title> ("<page> | <domain>") and the
  // build manifest. "LambdaBench" was the POC repo name and is intentionally gone.
  title: DOMAIN,
  // Output to a site-specific dir. The repo root `dist/` holds Lambda zips, and
  // Framework's default output is also `dist`; isolating it here avoids any clash.
  output: "out-site",
  root: "src",
  // `wide` makes the main column span the full viewport (no centered 1440px cap),
  // so charts and tables use the whole width on large monitors.
  theme: ["dark", "near-midnight", "wide"],
  // All SEO/social meta plus the stylesheet link are emitted per page here.
  head,
  // Format internal links as "/foo.html" so each maps directly to its built S3
  // object. This lets a private-S3 + CloudFront (OAC) host serve the site with
  // no clean-URL rewrite function at the edge. The home link stays "/".
  preserveExtension: true,
  // The benchmark is a fixed dataset, not a live dashboard: a static toc + the
  // default sidebar is enough. Pages are listed explicitly for ordering.
  pages: [
    { name: "Overview", path: "/" },
    { name: "Comparison", path: "/comparison" },
    { name: "Cold Start Anatomy", path: "/lifecycle" },
    { name: "Rust", path: "/rust" },
    { name: "Java SnapStart", path: "/java-snapstart" },
    { name: "Appendix", path: "/appendix" },
  ],
  // Global footer, two rows: a quiet fine-print disclaimer line, then a brighter
  // links row (Contact / Source / Full disclaimer) so the actions don't get lost at
  // the tail of the prose. "Contact" renders only when CONTACT_EMAIL is set; the
  // "Source on GitHub" link always renders, pointing at REPO_URL.
  footer:
    '<div class="foot-note">Independent personal project. Not affiliated with, endorsed, sponsored, or reviewed by AWS or any company whose products it measures. Provided as-is with no warranty: the numbers are best-effort and may be inaccurate or out of date. Verify against your own testing.</div>' +
    '<div class="foot-links">' +
    (CONTACT_EMAIL
      ? `<a href="mailto:${CONTACT_EMAIL}">Contact</a><span class="sep">&middot;</span>`
      : "") +
    `<a href="${REPO_URL}">Source on GitHub</a>` +
    '<span class="sep">&middot;</span><a href="/appendix.html#disclaimer">Full disclaimer</a>' +
    "</div>",
  // NOTE: the appendix "## Disclaimer" heading auto-generates the #disclaimer
  // anchor this links to. Keep the heading text exactly "Disclaimer" (no custom
  // {#id} syntax, which this Framework version renders literally).
  // Keep the build self-contained and offline-friendly.
  search: true,
};
