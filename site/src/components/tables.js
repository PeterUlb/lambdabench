// HTML table builders (head-to-head summary, architecture win-rate, percentile
// appendix): plain HTML honoring the active filter selection.

import { html } from "npm:htl";

import { THEME, PAIRS, TIE } from "./theme.js";
import { findCell, archPair } from "../lib/series.js";
import { median } from "../lib/stats.js";
import { fmtMs, langLabel, seriesLabel, scenarioLabel } from "../lib/format.js";

// All three tables take the filtered view from `makeView` (see charts.js), so
// they share the same filtered dimensions/cells/colors as the charts.

// ---- Head-to-head summary --------------------------------------------------
// Per (memory, scenario, arch), one column per language, row winner highlighted.
// Each phase shows P50 plus the strongest tail its sample count supports: cold
// P90, warm P99. Cold percentiles rest on one sample per cold cycle (n ≤ 50), so
// a cold P99 would sit on under one tail sample (MIN_N_FOR_P99 = 200 suppresses
// it to null); P90 is the same band the cold-vs-memory chart shades. Warm n
// (thousands per cell) carries a stable P99.
export function headToHead(v) {
  const langs = v.languages;
  const scenarios = v.scenarios;
  const archs = v.architectures;
  const memories = v.memories;

  const cellOf = (lang, scenario, arch, m) => {
    const c = findCell(v.cells, { lang, arch, scenario, memory_mb: m });
    return {
      cold: c?.cold?.p50,
      coldTail: c?.cold?.p90,
      warm: c?.warm?.p50,
      warmTail: c?.warm?.p99,
    };
  };
  const spread = (vals) =>
    vals.length >= 2 ? Math.max(...vals) / Math.min(...vals) : null;

  const rows = [];
  for (const m of memories) {
    for (const scenario of scenarios) {
      for (const arch of archs) {
        const perLang = Object.fromEntries(
          langs.map((l) => [l, cellOf(l, scenario, arch, m)]),
        );
        const coldVals = langs
          .map((l) => perLang[l].cold)
          .filter((v) => v != null);
        const warmVals = langs
          .map((l) => perLang[l].warm)
          .filter((v) => v != null);
        const coldTailVals = langs
          .map((l) => perLang[l].coldTail)
          .filter((v) => v != null);
        const warmTailVals = langs
          .map((l) => perLang[l].warmTail)
          .filter((v) => v != null);
        if (!coldVals.length && !warmVals.length) continue;
        rows.push({
          memory_mb: m,
          scenarioLabel: scenarioLabel(scenario),
          arch,
          perLang,
          coldSpread: spread(coldVals),
          warmSpread: spread(warmVals),
          bestCold: coldVals.length ? Math.min(...coldVals) : null,
          bestWarm: warmVals.length ? Math.min(...warmVals) : null,
          bestColdTail: coldTailVals.length ? Math.min(...coldTailVals) : null,
          bestWarmTail: warmTailVals.length ? Math.min(...warmTailVals) : null,
        });
      }
    }
  }

  // `val` is the P50, `t` the phase's tail (cold P90 / warm P99). Bold = fastest
  // P50 in the row; `.win` marks the fastest tail.
  const langCell = (lang, val, t, best, bestTail) => {
    const isBest = val != null && val === best ? "font-weight:700;" : "";
    const tWin = t != null && t === bestTail ? " win" : "";
    return html`<td
      style="color:${v.color.langColor[lang]};font-variant-numeric:tabular-nums;${isBest}"
    >
      ${fmtMs(val)}<span class="tail${tWin}"> / ${fmtMs(t)}</span>
    </td>`;
  };

  const body = rows.map((r, i) => {
    const firstOfBlock = i === 0 || rows[i - 1].memory_mb !== r.memory_mb;
    const blockClass =
      v.stats.dimensions.memories.indexOf(r.memory_mb) % 2 === 0
        ? "memA"
        : "memB";
    return html`<tr class="${blockClass}${firstOfBlock ? " block-top" : ""}">
      <td class="memcol">${firstOfBlock ? `${r.memory_mb} MB` : ""}</td>
      <td>${r.scenarioLabel}</td>
      <td class="dim">${r.arch}</td>
      ${langs.map((l) => langCell(l, r.perLang[l].cold, r.perLang[l].coldTail, r.bestCold, r.bestColdTail))}
      <td class="hot">${r.coldSpread ? r.coldSpread.toFixed(2) + "×" : "—"}</td>
      ${langs.map((l) => langCell(l, r.perLang[l].warm, r.perLang[l].warmTail, r.bestWarm, r.bestWarmTail))}
      <td class="hot">${r.warmSpread ? r.warmSpread.toFixed(2) + "×" : "—"}</td>
    </tr>`;
  });

  // Horizontal-scroll wrapper so a wide language set scrolls inside the content
  // column instead of overflowing under the page TOC.
  return html`<div class="tbl-scroll">
    <table class="summary">
      <thead>
        <tr>
          <th>Memory</th>
          <th>Scenario</th>
          <th>Arch</th>
          ${langs.map((l) => html`<th class="grp">${langLabel(l)} cold<br /><span class="th-sub">P50 / P90</span></th>`)}
          <th class="grp hot">cold spread</th>
          ${langs.map((l) => html`<th>${langLabel(l)} warm<br /><span class="th-sub">P50 / P99</span></th>`)}
          <th class="hot">warm spread</th>
        </tr>
      </thead>
      <tbody>
        ${body}
      </tbody>
    </table>
  </div>`;
}

// ---- Architecture win-rate -------------------------------------------------
// Per (lang, metric): how many scenario × memory cells each arch wins on P50,
// with near-ties counted separately, plus the median win margin.
export function archWinRate(v) {
  // Generic over any two-architecture run: derive the pair from the data, not
  // hardcoded arm64/x86_64. A run with a single arch (or more than two) has no
  // head-to-head to show.
  const pair = archPair(v.stats);
  if (!pair) return html`<div></div>`;
  const { a0, a1, p50 } = pair;
  const langs = v.languages;
  const scenarios = v.scenarios;
  const memories = v.memories;
  // First arch teal, second arch blue, regardless of the run's arch names.
  // Shared with the arch dumbbell so the convention cannot drift.
  const archColor = { [a0]: PAIRS.arch[0], [a1]: PAIRS.arch[1] };

  // `useAbsFloor` adds the absolute TIE.ms floor to the relative TIE.pct one.
  // Meaningful only for cold-init values (tens-to-hundreds of ms), where a
  // sub-0.5ms gap is noise. For warm P50s on cheap scenarios (often sub-1ms) a
  // 0.5ms floor would swamp a real 2x relative gap, so warm uses relative only.
  const tally = (field, useAbsFloor) => {
    const counts = {};
    for (const lang of langs)
      counts[lang] = { [a0]: 0, [a1]: 0, tie: 0, margins0: [], margins1: [] };
    for (const lang of langs) {
      for (const scenario of scenarios) {
        for (const m of memories) {
          // Read both arches from the full dataset so the arch filter can't hide
          // one side. `archPair.p50` enforces that (full stats.cells, not v.cells).
          const x0 = p50(lang, scenario, m, a0, field);
          const x1 = p50(lang, scenario, m, a1, field);
          if (x0 == null || x1 == null) continue;
          const marginPct = (Math.abs(x0 - x1) / Math.min(x0, x1)) * 100;
          const isTie =
            marginPct < TIE.pct || (useAbsFloor && Math.abs(x0 - x1) < TIE.ms);
          if (isTie) counts[lang].tie++;
          else if (x0 < x1) {
            counts[lang][a0]++;
            counts[lang].margins0.push(marginPct);
          } else {
            counts[lang][a1]++;
            counts[lang].margins1.push(marginPct);
          }
        }
      }
    }
    return counts;
  };
  // "Cold init" uses the coldInit marker (init/restore time only), not the
  // init+first-request total the cold charts plot; labelled to match.
  const coldWins = tally("coldInit", true);
  const warmWins = tally("warm", false);
  // Shared NaN-safe median (lib/stats.js, R-7 like d3.quantile), so the value
  // stays consistent with the percentile table and cold charts.
  const fmtMargin = (a) => (a.length ? `${median(a).toFixed(0)}%` : "—");

  const winRow = (lang, metric, c) => {
    const total = c[a0] + c[a1] + c.tie || 1;
    const seg = (n, color, label) =>
      n
        ? html`<span
            class="winseg"
            style="width:${(100 * n) / total}%;background:${color}"
            title="${label}: ${n}"
            >${n}</span
          >`
        : "";
    return html`<tr>
      <td class="winlang" style="color:${v.color.langColor[lang]}">
        ${langLabel(lang)}
      </td>
      <td class="dim">${metric}</td>
      <td class="winbar">
        ${seg(c[a0], archColor[a0], a0)}${seg(c[a1], archColor[a1], a1)}${seg(c.tie, THEME.frame, "too close to call")}
      </td>
      <td class="winnum">
        ${c[a0]}&nbsp;/&nbsp;${c[a1]}<span class="dim"
          >&nbsp;/&nbsp;${c.tie}</span
        >
      </td>
      <td class="winnum dim">
        ${fmtMargin(c.margins0)}&nbsp;/&nbsp;${fmtMargin(c.margins1)}
      </td>
    </tr>`;
  };

  return html`<div class="winrate-wrap">
    <div class="winlegend">
      <span
        ><span class="lg-swatch" style="background:${archColor[a0]}"></span
        >${a0} wins</span
      >
      <span
        ><span class="lg-swatch" style="background:${archColor[a1]}"></span
        >${a1} wins</span
      >
      <span
        ><span class="lg-swatch" style="background:${THEME.frame}"></span>too
        close to call (&lt;${TIE.pct}%, or for cold init &lt;${TIE.ms}ms)</span
      >
    </div>
    <table class="winrate">
      <thead>
        <tr>
          <th>Lang</th>
          <th>Metric</th>
          <th>Wins by cell (scenario × memory)</th>
          <th>${a0} / ${a1} / tie</th>
          <th>
            median win margin<br /><span class="th-sub">${a0} / ${a1}</span>
          </th>
        </tr>
      </thead>
      <tbody>
        ${langs.map((l) => winRow(l, "cold init", coldWins[l]))}
        ${langs.map((l) => winRow(l, "warm", warmWins[l]))}
      </tbody>
    </table>
  </div>`;
}

// ---- Percentile appendix (one table per scenario) --------------------------
export function percentileTable(v, scenario) {
  const cols = [
    ["min", "min"],
    ["p10", "P10"],
    ["p50", "P50"],
    ["p90", "P90"],
    ["p99", "P99"],
    ["p999", "P99.9"],
    ["max", "max"],
    ["n", "n", (x) => x.toLocaleString("en-US")],
  ];
  // v.color.domain is the selected series in display order; keep only those with
  // data for this scenario.
  const seriesList = v.color.domain.filter((s) =>
    v.cells.some((c) => c.series === s && c.scenario === scenario),
  );
  const rows = [];
  for (const s of seriesList) {
    for (const m of v.memories) {
      const cell = v.cells.find(
        (c) => c.series === s && c.scenario === scenario && c.memory_mb === m,
      );
      if (!cell || (!cell.warm && !cell.cold)) continue;
      rows.push({
        series: s,
        memory_mb: m,
        warm: cell.warm,
        cold: cell.cold,
        warmCycles: cell.warmCycles,
      });
    }
  }
  if (!rows.length) return null;

  const cellVal = (stat, field, fmt) =>
    stat && stat[field] != null ? (fmt ?? fmtMs)(stat[field]) : "—";
  // The warm tail percentiles are correlation-limited: within one cold cycle the
  // warm samples share a sandbox, so the effective independent count is the
  // distinct-cycle count (`warmCycles`), not the raw warm n. Annotate those two
  // cells with that count so a hover shows what backs the number.
  const WARM_TAIL_FIELDS = new Set(["p99", "p999"]);
  const body = rows.map((r, i) => {
    const firstOfSeries = i === 0 || rows[i - 1].series !== r.series;
    const swatchColor = v.color.range[v.color.domain.indexOf(r.series)];
    const seriesCell = firstOfSeries
      ? html`<td
          class="pctl-series"
          rowspan="${rows.filter((x) => x.series === r.series).length}"
        >
          <span class="lg-swatch" style="background:${swatchColor}"></span
          >${seriesLabel(r.series)}
        </td>`
      : "";
    const warmCells = cols.map(([f, , fmt]) => {
      const val = cellVal(r.warm, f, fmt);
      if (
        WARM_TAIL_FIELDS.has(f) &&
        r.warm &&
        r.warm[f] != null &&
        r.warmCycles != null
      ) {
        return html`<td
          class="warm-tail"
          title="≈${r.warmCycles} distinct cold cycles back this tail (warm samples within a cycle are correlated); compare across cells rather than reading the absolute value as i.i.d.-robust"
        >
          ${val}
        </td>`;
      }
      return html`<td>${val}</td>`;
    });
    const coldCells = cols.map(
      ([f, , fmt], j) =>
        html`<td class="${j === 0 ? "pctl-sep" : ""}">
          ${cellVal(r.cold, f, fmt)}
        </td>`,
    );
    return html`<tr class="${firstOfSeries ? "pctl-block-top" : ""}">
      ${seriesCell}
      <td class="memcol">${r.memory_mb}</td>
      ${warmCells}${coldCells}
    </tr>`;
  });

  // The widest view (~18 columns); wrap it so it scrolls horizontally within the
  // content column rather than sliding under the TOC.
  return html`<div class="tbl-scroll">
    <table class="summary pctl">
      <thead>
        <tr>
          <th rowspan="2">Series</th>
          <th rowspan="2">Mem<br /><span class="th-sub">MB</span></th>
          <th colspan="${cols.length}">Warm (ms, + n)</th>
          <th colspan="${cols.length}" class="pctl-sep-h">
            Cold = init + 1st req (ms, + n)
          </th>
        </tr>
        <tr>
          ${cols.map(([, l]) => html`<th>${l}</th>`)}${cols.map(([, l], j) => html`<th class="${j === 0 ? "pctl-sep-h" : ""}">${l}</th>`)}
        </tr>
      </thead>
      <tbody>
        ${body}
      </tbody>
    </table>
  </div>`;
}
