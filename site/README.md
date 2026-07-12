# LambdaBench interactive site

An [Observable Framework](https://observablehq.com/framework/) site that turns a
lambdabench results file into an interactive, hostable dashboard. Two-stage design:

1. **Data loader** (`src/data/stats.json.js`) runs at **build time**: it streams
   the raw `results/run-*.jsonl.gz` (of order 100 MB gz / ~10^6 rows) exactly once
   and emits a compact `stats.json` of aggregates (sub-MB gzipped): per-cell
   percentiles, cost, footprint, artifact sizes, and pre-sampled distribution
   points. This is the only code that touches raw rows.
2. **Pages** (`src/*.md`) load that JSON and render with Observable Plot **in the
   browser**. Languages, scenarios, architectures, and memory tiers are reactive
   filters (`src/components/filters.js`): toggling one re-renders every chart
   instantly, with no rebuild. This is what the static renderer could not do.

## Usage

```bash
cd site
npm install
npm run dev      # live-reload dev server at http://127.0.0.1:3000
npm run build    # static site -> out-site/  (deploy this anywhere)
```

The loader auto-selects the newest `*.jsonl(.gz)` in `../../results`. Point it at
a specific file with `LAMBDABENCH_RESULTS=/abs/path/run.jsonl.gz npm run build`.

Build-time env vars:

- `LAMBDABENCH_RESULTS`: optional, absolute path to a specific run file (default: newest in `../../results`).
- `LAMBDABENCH_SITE_DOMAIN`: optional, apex domain baked into canonical/OG/sitemap URLs (default: `lambdabench.dev`).
- `LAMBDABENCH_REPO_URL`: required, public source-repo URL the footer's "Source on GitHub" link points to (e.g. `https://github.com/you/lambdabench`).
- `LAMBDABENCH_CONTACT_EMAIL`: optional, contact email; when set, adds a "Contact" mailto link to the footer (default: unset, link omitted).

Deploy the contents of `out-site/` to any static host (S3 + CloudFront, GitHub
Pages, Netlify, …). Enable gzip/brotli on the host so `stats.json` transfers at
its compressed size.

## Layout

```
src/
  data/stats.json.js     build-time loader: raw rows -> compact aggregates
  lib/                   pure, dependency-free modules (loader + client share)
    stats.js             quantile / summarize / geomean
    format.js            formatters, scenario labels + blurbs
    series.js            langKey/seriesOf + data-derived color model
  components/
    theme.js             palette + base Plot options
    charts.js            makeView() + client-side chart builders (take a view)
    tables.js            HTML tables (head-to-head, win-rate, percentiles)
    filters.js           reactive filter form
  styles.css             component styles (filters, small multiples, tables)
  index.md               Overview: KPIs, scenario reference, cold-start charts,
                         head-to-head table
  comparison.md          Warm, Cost & Tail: warm, tail, CPU-sensitivity,
                         footprint, cost, artifact size, arch (cold start lives
                         on the Overview, not here)
  lifecycle.md           Cold Start Anatomy: the unreported download +
                         environment-start cost (probe table + scaling charts),
                         zip vs container image, the Init-phase CPU boost /
                         work placement, and the suppressed init; renders the
                         off-matrix results/lifecycle-*.json probe data, not
                         stats.json
  rust.md                Rust opt-level A/B (o3 vs oz); Rust-only dimension
  java-snapstart.md      plain JVM vs SnapStart A/B; Java-only dimension
  appendix.md            distribution scatter + full percentile tables
```

Pages split by role: the Overview holds the cold-start headline (the charts
where the spread is widest) plus the summary NUMBERS (head-to-head table); Warm,
Cost & Tail holds everything beyond cold start (warm, tail, cost, footprint,
arch); and each runtime-specific dimension (Rust opt-level, Java SnapStart) gets
its own standalone page so the cross-language views stay strictly like-for-like.
No chart is duplicated across pages. The Rust / Java SnapStart pages render a note instead of a chart when the
dataset lacks that dimension.

## The view pattern

Every page builds a filtered VIEW once per render and passes it to all charts
and tables:

```js
const sel = view(filterForm(stats, { colorModel: cm })); // reactive selection
const v = makeView(stats, sel); // apply it ONCE
display(coldVsMemory(v)); // charts read the view
display(headToHead(v));
```

`makeView(stats, sel)` (in `components/charts.js`) is the single place the filter
selection is applied. It returns the filtered dimensions (`languages`,
`scenarios`, `memories`, `architectures`), the filtered `cells`, the restricted
`color` scale, and a `memX()` axis builder, all mutually consistent. A chart
never re-derives "what is selected", so it is structurally impossible to filter
the data but not the axis (or the row order, or the legend). `v.stats` is the
escape hatch for the few charts that need the FULL dataset (a fixed
normalization baseline, or the per-language A/B cells that live outside `cells`).
When adding a chart, take `v` and read from it; do not re-filter `stats`.
