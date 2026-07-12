// Reactive filter UI. Returns a form element whose `.value` is the current
// selection { languages, architectures, scenarios, memories }. Framework's
// `view()` makes it reactive, so a chart cell reading it re-renders on toggle.

import { html } from "npm:htl";
import { langLabel, scenarioLabel } from "../lib/format.js";

// Build a labelled checkbox group. `colorFor` (optional) tints each label by its
// value so language toggles match the chart palette.
function checkGroup(name, values, labelFn, { colorFor } = {}) {
  const boxes = values.map((v) => {
    const input = html`<input
      type="checkbox"
      name="${name}"
      value="${v}"
      checked
    />`;
    const color = colorFor ? colorFor(v) : null;
    return html`<label class="flt-chk"
      >${input}<span style=${color ? `color:${color}` : ""}
        >${labelFn(v)}</span
      ></label
    >`;
  });
  return { boxes, inputs: boxes.map((b) => b.querySelector("input")) };
}

// Build the reactive filter form. `groups` selects which controls to show
// (default: all four). A page whose charts only vary on a subset (e.g. the
// Rust-only opt-level page) passes a subset like
// `["architectures", "scenarios", "memories"]`. Hidden groups are not rendered
// but still report every value as selected, so a chart reading `sel.languages`
// works without special-casing.
export function filterForm(stats, { colorModel, groups } = {}) {
  const dim = stats.dimensions;
  const langColor = colorModel ? colorModel.langColor : null;
  const show = groups ?? [
    "languages",
    "architectures",
    "scenarios",
    "memories",
  ];

  const langs = checkGroup("languages", dim.languages, langLabel, {
    colorFor: langColor ? (l) => langColor[l] : null,
  });
  const archs = checkGroup("architectures", dim.architectures, (a) => a);
  const scens = checkGroup("scenarios", dim.scenarios, scenarioLabel);
  const mems = checkGroup("memories", dim.memories, (m) => `${m} MB`);

  // A shown group reads its checkboxes; a hidden group reports its full domain,
  // so downstream filtering treats it as "all selected".
  const read = () => ({
    languages: show.includes("languages")
      ? langs.inputs.filter((i) => i.checked).map((i) => i.value)
      : [...dim.languages],
    architectures: show.includes("architectures")
      ? archs.inputs.filter((i) => i.checked).map((i) => i.value)
      : [...dim.architectures],
    scenarios: show.includes("scenarios")
      ? scens.inputs.filter((i) => i.checked).map((i) => i.value)
      : [...dim.scenarios],
    memories: show.includes("memories")
      ? mems.inputs.filter((i) => i.checked).map((i) => Number(i.value))
      : [...dim.memories],
  });

  const grp = (key, label, g) =>
    show.includes(key)
      ? html`<div class="flt-grp">
          <div class="flt-lbl">${label}</div>
          <div class="flt-row">${g.boxes}</div>
        </div>`
      : null;

  const form = html`<form class="filters">
    ${grp("languages", "Languages", langs)}
    ${grp("architectures", "Architectures", archs)}
    ${grp("scenarios", "Scenarios", scens)}
    ${grp("memories", "Memory tiers", mems)}
  </form>`;

  const update = () => {
    form.value = read();
    form.dispatchEvent(new CustomEvent("input"));
  };
  // Only wire listeners on shown groups (hidden ones are not in the DOM).
  const shownInputs = [
    ...(show.includes("languages") ? langs.inputs : []),
    ...(show.includes("architectures") ? archs.inputs : []),
    ...(show.includes("scenarios") ? scens.inputs : []),
    ...(show.includes("memories") ? mems.inputs : []),
  ];
  for (const i of shownInputs) i.addEventListener("change", update);
  form.value = read();
  return form;
}
