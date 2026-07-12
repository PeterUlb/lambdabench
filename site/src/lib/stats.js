// Pure statistics helpers shared by the build-time data loader and the client,
// so percentiles compute identically at build time and in the browser.

// Linear-interpolated quantile (type-7, matching numpy's default and Excel's
// PERCENTILE.INC) of an ascending-sorted array. `q` must be in [0, 1]; reject
// out-of-range loudly rather than index past the array and return undefined.
export function quantile(sorted, q) {
  if (sorted.length === 0) return NaN;
  if (!(q >= 0 && q <= 1))
    throw new Error(`quantile: q must be in [0, 1], got ${q}`);
  const pos = (sorted.length - 1) * q;
  const base = Math.floor(pos);
  const rest = pos - base;
  return sorted[base + 1] !== undefined
    ? sorted[base] + rest * (sorted[base + 1] - sorted[base])
    : sorted[base];
}

// Returns a new ascending-sorted array of only the finite numbers. A single
// non-numeric entry makes the `a - b` comparator return NaN and sorts
// unpredictably, corrupting every percentile; ±Infinity would distort mean/max.
// The one place that enforces finiteness, so all quantile inputs pass through it.
export function cleanSort(values) {
  return values
    .filter((v) => typeof v === "number" && Number.isFinite(v))
    .sort((a, b) => a - b);
}

// Median of an unsorted array, or null when empty after dropping non-finite
// entries. Sorts a copy, leaving the caller's array untouched. Convenience
// wrapper over `quantile` for call sites that only need a P50 from raw values.
export function median(values) {
  const s = cleanSort(values);
  if (s.length === 0) return null;
  return quantile(s, 0.5);
}

// The middle element (lower-middle for an even length), undefined when empty.
// Picks a representative "middle" memory tier (the loader's default pivot, and
// the middle axis tick in a narrow chart panel); shared so the two agree.
export function midOf(arr) {
  if (arr.length === 0) return undefined;
  return arr[Math.floor((arr.length - 1) / 2)];
}

// Minimum sample sizes a percentile needs before reporting it as a number.
// A percentile q with n samples puts ~(1 - q) * n samples in the tail; below a
// handful of tail samples the value tracks individual outliers, not the
// distribution. Under-sampled cells return null so the site hides the value.
//
// The gate is deliberately a floor on the RAW count `s.length`, even though
// warm samples within a cycle are correlated and a warm tail's effective
// independent count is the cycle count, far below n (DESIGN.md's "effective n
// for the tail" reading rule): a P99.9 gap between two cells still compares
// meaningfully, so the warm tail stays published, and the aggregate carries
// `warmCycles` so the site can label it as correlation-limited.
export const MIN_N_FOR_P99 = 200;
export const MIN_N_FOR_P999 = 1000;

// Summarize a numeric array into the percentile object used everywhere downstream.
// Returns null for an empty input so callers can filter empty cells cleanly.
export function summarizeValues(values) {
  const s = cleanSort(values);
  if (s.length === 0) return null;
  return {
    n: s.length,
    min: s[0],
    p10: quantile(s, 0.1),
    p50: quantile(s, 0.5),
    p90: quantile(s, 0.9),
    p99: s.length >= MIN_N_FOR_P99 ? quantile(s, 0.99) : null,
    p999: s.length >= MIN_N_FOR_P999 ? quantile(s, 0.999) : null,
    max: s[s.length - 1],
    mean: s.reduce((a, b) => a + b, 0) / s.length,
  };
}
