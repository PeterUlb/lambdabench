// Visual theme tokens for the CHARTS. Plot renders to SVG with concrete color
// attributes, so it cannot read the CSS `--theme-*` variables the rest of the
// page uses. These hexes mirror the palette by hand. They MUST stay in sync
// with the `:root` overrides in styles.css, which set the same values on
// Framework's `--theme-*` variables so the page chrome matches the charts.
export const THEME = {
  bg: "#0d0e11", // page background (deep blue-black)
  panel: "#16181d", // card / block background
  panel2: "#1b1e24", // elevated surface (KPI cards, hover)
  grid: "#2a2d35", // chart gridlines
  frame: "#33363f", // chart frame / rules
  text: "#e8e9ee", // primary text
  dim: "#9aa0ac", // secondary text
  faint: "#6b7280", // faint text / borders
  accent: "#7fb0ff", // links / focus / emphasis
};

// Two-color palettes for the A/B comparison charts (dumbbells, win-rate,
// distribution, cold breakdown). Each pair encodes one binary convention; kept
// here as the single source of truth so the same convention cannot drift
// between the charts that share it (e.g. the arch pair is used by both the
// arch dumbbell and the arch win-rate table). Each is `[first, second]` in the
// order the consumer documents.
export const PAIRS = {
  arch: ["#54d6bd", "#5b8def"], // first arch = teal, second arch = blue
  coldWarm: ["#ff5d5d", "#54d6bd"], // cold = red, warm = teal
  plainSnap: ["#ff6b6b", "#4ecdc4"], // plain JVM = red, SnapStart = teal
  opt: ["#ff9f45", "#9b8cff"], // opt-level=3 = amber, opt-level=z = violet
  initFirst: ["#5b8def", "#9cc0ff"], // cold init = blue, first request = light blue
};

// Thresholds below which two architectures are "too close to call" in the
// win-rate tally: a relative gap under `tiePct` percent OR an absolute gap
// under `tieMs` milliseconds counts as a tie. Single source of truth so the
// tally logic and its legend text cannot disagree.
export const TIE = { pct: 2, ms: 0.5 };

// Base Plot options shared by every chart (transparent so the page bg shows).
export const plotBase = {
  style: {
    background: "transparent",
    color: THEME.text,
    fontSize: "13px",
    fontFamily: "ui-sans-serif, -apple-system, 'Segoe UI', Roboto, sans-serif",
  },
};
