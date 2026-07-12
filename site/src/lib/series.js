// Series identity and color derivation, all derived from the data so the site
// adapts to whatever was run (adding Go/Kotlin needs no code change).
//
// A "series" is a (langKey, arch) pair. `langKey` treats SnapStart as its own
// language (`<runtime>-snapstart`, e.g. `java-snapstart`), not a sub-variant,
// since snapshot restore is a different execution model from cold init. The
// pseudo-language is derived per runtime, never hardcoded to Java, so a
// SnapStart-capable Python run becomes `python-snapstart` rather than merging
// into Java's series. The raw runtime (`d.lang`) is kept where the within-runtime
// distinction matters (the SnapStart A/B panel keys off it).

// The `<runtime>-snapstart` suffix encoding and its encode/decode live here, so
// consumers call isSnapLang/baseLang and changing it touches only this module.
const SNAP_SUFFIX = "-snapstart";
export const langKey = (d) =>
  d.snapstart ? `${d.lang}${SNAP_SUFFIX}` : d.lang;
// True when a language/series key is a SnapStart pseudo-language.
export const isSnapLang = (key) => key.endsWith(SNAP_SUFFIX);
// The base runtime a key restores (`python-snapstart` -> `python`); identity for
// a plain runtime key.
export const baseLang = (key) =>
  isSnapLang(key) ? key.slice(0, -SNAP_SUFFIX.length) : key;
export const seriesOf = (d) => `${langKey(d)} ${d.arch}`;

// Locate a single main-stats cell by its identity fields. Shared by charts and
// tables so the (lang, arch, scenario, memory) lookup is expressed once.
export const findCell = (cells, { lang, arch, scenario, memory_mb }) =>
  cells.find(
    (c) =>
      c.lang === lang &&
      c.arch === arch &&
      c.scenario === scenario &&
      c.memory_mb === memory_mb,
  );

// Runtimes to render in a SnapStart A/B view, in display order: those present in
// `snapCells`, narrowed to the active languages and an optional `only` scope. A
// per-runtime page passes `only: ["java"]` so it never sprouts rows for another
// runtime that later gains a SnapStart variant; omitting `only` shows all.
export const snapLangsToShow = (snapCells, activeLangs, only = null) =>
  [...new Set(snapCells.map((c) => c.lang))]
    .filter(
      (l) => activeLangs.includes(l) && (only == null || only.includes(l)),
    )
    .sort();

// Shared contract for the two architecture A/B views (arch dumbbell, arch
// win-rate table): both compare exactly two architectures and must read both
// sides from the full dataset, never the arch-filtered cells, or the toggle
// would hide one side. Returns the two arch names plus a
// `p50(lang, scenario, m, arch, field)` reader over the full `stats.cells`, or
// null when the run is not a clean two-arch pair (single arch, or more than two).
export function archPair(stats) {
  const archs = stats.dimensions.architectures;
  if (archs.length !== 2) return null;
  const [a0, a1] = archs;
  const p50 = (lang, scenario, memory_mb, arch, field) =>
    findCell(stats.cells, { lang, arch, scenario, memory_mb })?.[field]?.p50;
  return { a0, a1, archs, p50 };
}

// HSL→hex without a color library (keeps loader + client dependency-free).
function hslToHex(h, s, l) {
  const a = s * Math.min(l, 1 - l);
  const f = (n) => {
    const k = (n + h / 30) % 12;
    const c = l - a * Math.max(-1, Math.min(k - 3, 9 - k, 1));
    return Math.round(255 * c)
      .toString(16)
      .padStart(2, "0");
  };
  return `#${f(0)}${f(8)}${f(4)}`;
}

// Conventional hues so colors match expectations (Rust = orange, Node = teal).
// An unlisted language gets an auto-assigned free hue.
const KNOWN_HUE = {
  rust: 28,
  node: 160,
  java: 8,
  "java-snapstart": 45,
  python: 210,
  go: 188,
  kotlin: 270,
};
const RESERVED_HUES = Object.values(KNOWN_HUE);

// Hue shift for a SnapStart pseudo-language off its base runtime, so the two
// read as related-but-distinct (Java 8° -> Java SnapStart 45°; the explicit
// `java-snapstart` entry pins that pairing). Other runtimes inherit this offset.
const SNAP_HUE_OFFSET = 37;

function langHue(lang, languages) {
  if (lang in KNOWN_HUE) return KNOWN_HUE[lang];
  // A SnapStart pseudo-language with no explicit hue derives one from its base
  // runtime so it stays visually adjacent to the runtime it restores.
  if (isSnapLang(lang)) {
    return (langHue(baseLang(lang), languages) + SNAP_HUE_OFFSET) % 360;
  }
  const unknown = languages.filter((l) => !(l in KNOWN_HUE));
  const idx = unknown.indexOf(lang);
  let h = (300 + (idx * 360) / Math.max(1, unknown.length)) % 360;
  // Spin the candidate hue away from any reserved (known-language) hue within
  // MIN_SEPARATION degrees. `Math.abs(((h - r + 540) % 360) - 180)` is the
  // circular distance between two hues (0 = identical, 180 = opposite).
  const MIN_SEPARATION = 25;
  for (
    let guard = 0;
    guard < 12 &&
    RESERVED_HUES.some(
      (r) => Math.abs(((h - r + 540) % 360) - 180) < MIN_SEPARATION,
    );
    guard++
  ) {
    h = (h + 23) % 360;
  }
  return h;
}

// Build the full color model for a given set of languages + architectures.
// Returns:
//   langColor:  { lang -> hex }                 (one hue per language)
//   color:      { "lang arch" -> hex }           (arch-shaded series colors)
//   domain/range: parallel arrays for Plot's color scale
export function colorModel(languages, architectures) {
  const langColor = Object.fromEntries(
    languages.map((lang) => [
      lang,
      hslToHex(langHue(lang, languages), 0.78, 0.62),
    ]),
  );
  const color = {};
  for (const lang of languages) {
    architectures.forEach((arch, j) => {
      const l =
        architectures.length > 1
          ? 0.7 - (0.28 * j) / Math.max(1, architectures.length - 1)
          : 0.62;
      color[`${lang} ${arch}`] = hslToHex(langHue(lang, languages), 0.72, l);
    });
  }
  return {
    langColor,
    color,
    domain: Object.keys(color),
    range: Object.values(color),
  };
}
