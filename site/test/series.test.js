import { describe, it, expect } from "vitest";
import {
  langKey,
  seriesOf,
  isSnapLang,
  baseLang,
  snapLangsToShow,
  findCell,
  archPair,
  colorModel,
} from "../src/lib/series.js";

describe("langKey", () => {
  it("returns the raw lang for non-SnapStart rows", () => {
    expect(langKey({ lang: "rust", snapstart: false })).toBe("rust");
    expect(langKey({ lang: "node" })).toBe("node");
  });

  it("treats SnapStart as its own pseudo-language", () => {
    expect(langKey({ lang: "java", snapstart: true })).toBe("java-snapstart");
  });

  it("derives the pseudo-language per runtime, not hardcoded to Java", () => {
    // A SnapStart-capable Python run must NOT collapse into java-snapstart.
    expect(langKey({ lang: "python", snapstart: true })).toBe(
      "python-snapstart",
    );
  });
});

describe("isSnapLang / baseLang", () => {
  it("recognizes a SnapStart pseudo-language key", () => {
    expect(isSnapLang("java-snapstart")).toBe(true);
    expect(isSnapLang("python-snapstart")).toBe(true);
    expect(isSnapLang("rust")).toBe(false);
  });

  it("decodes the base runtime, and is identity for a plain key", () => {
    expect(baseLang("python-snapstart")).toBe("python");
    expect(baseLang("rust")).toBe("rust");
  });

  it("round-trips with langKey", () => {
    const key = langKey({ lang: "python", snapstart: true });
    expect(isSnapLang(key)).toBe(true);
    expect(baseLang(key)).toBe("python");
  });
});

describe("snapLangsToShow", () => {
  const cells = [
    { lang: "java", snapstart: false },
    { lang: "java", snapstart: true },
    { lang: "python", snapstart: false },
    { lang: "python", snapstart: true },
  ];
  const active = ["java", "python", "rust"];

  it("returns every present runtime, sorted, when no scope is given", () => {
    expect(snapLangsToShow(cells, active)).toEqual(["java", "python"]);
  });

  it("scopes to the explicit `only` list (a per-runtime page stays single-runtime)", () => {
    expect(snapLangsToShow(cells, active, ["java"])).toEqual(["java"]);
  });

  it("never shows a runtime absent from snapCells even if `only` lists it", () => {
    // rust ran no SnapStart variant, so it is not in snapCells and must not appear.
    expect(snapLangsToShow(cells, active, ["rust"])).toEqual([]);
  });

  it("intersects with the active languages (a deselected runtime drops out)", () => {
    expect(snapLangsToShow(cells, ["python"])).toEqual(["python"]);
  });

  it("dedupes runtimes that appear across many cells", () => {
    const many = [{ lang: "java" }, { lang: "java" }, { lang: "java" }];
    expect(snapLangsToShow(many, ["java"])).toEqual(["java"]);
  });
});

describe("seriesOf", () => {
  it("combines langKey and arch", () => {
    expect(seriesOf({ lang: "rust", arch: "arm64" })).toBe("rust arm64");
  });

  it("uses the SnapStart pseudo-language in the series id", () => {
    expect(seriesOf({ lang: "java", snapstart: true, arch: "x86_64" })).toBe(
      "java-snapstart x86_64",
    );
  });
});

describe("findCell", () => {
  const cells = [
    {
      lang: "rust",
      arch: "arm64",
      scenario: "hello",
      memory_mb: 512,
      warm: { p50: 1 },
    },
    {
      lang: "rust",
      arch: "arm64",
      scenario: "hello",
      memory_mb: 1024,
      warm: { p50: 2 },
    },
    {
      lang: "node",
      arch: "x86_64",
      scenario: "hello",
      memory_mb: 512,
      warm: { p50: 3 },
    },
  ];

  it("finds the cell matching all four identity fields", () => {
    const c = findCell(cells, {
      lang: "rust",
      arch: "arm64",
      scenario: "hello",
      memory_mb: 1024,
    });
    expect(c.warm.p50).toBe(2);
  });

  it("returns undefined when no cell matches", () => {
    expect(
      findCell(cells, {
        lang: "go",
        arch: "arm64",
        scenario: "hello",
        memory_mb: 512,
      }),
    ).toBeUndefined();
  });

  it("distinguishes cells that differ only by memory", () => {
    const c = findCell(cells, {
      lang: "rust",
      arch: "arm64",
      scenario: "hello",
      memory_mb: 512,
    });
    expect(c.warm.p50).toBe(1);
  });
});

describe("archPair", () => {
  const makeStats = (architectures, cells = []) => ({
    dimensions: { architectures },
    cells,
  });

  it("returns null for a single-arch run", () => {
    expect(archPair(makeStats(["arm64"]))).toBeNull();
  });

  it("returns null for more than two arches", () => {
    expect(archPair(makeStats(["arm64", "x86_64", "riscv"]))).toBeNull();
  });

  it("exposes both arch names for a clean two-arch run", () => {
    const p = archPair(makeStats(["arm64", "x86_64"]));
    expect(p.a0).toBe("arm64");
    expect(p.a1).toBe("x86_64");
    expect(p.archs).toEqual(["arm64", "x86_64"]);
  });

  it("p50 reader pulls a field's p50 from the full cells", () => {
    const cells = [
      {
        lang: "rust",
        arch: "x86_64",
        scenario: "hello",
        memory_mb: 512,
        warm: { p50: 9 },
      },
    ];
    const p = archPair(makeStats(["arm64", "x86_64"], cells));
    expect(p.p50("rust", "hello", 512, "x86_64", "warm")).toBe(9);
  });

  it("p50 reader returns undefined when the cell or field is missing", () => {
    const p = archPair(makeStats(["arm64", "x86_64"], []));
    expect(p.p50("rust", "hello", 512, "arm64", "warm")).toBeUndefined();
  });
});

describe("colorModel", () => {
  it("produces one langColor per language and one color per series", () => {
    const m = colorModel(["rust", "node"], ["arm64", "x86_64"]);
    expect(Object.keys(m.langColor).sort()).toEqual(["node", "rust"]);
    expect(Object.keys(m.color).sort()).toEqual([
      "node arm64",
      "node x86_64",
      "rust arm64",
      "rust x86_64",
    ]);
  });

  it("emits hex colors", () => {
    const m = colorModel(["rust"], ["arm64"]);
    expect(m.color["rust arm64"]).toMatch(/^#[0-9a-f]{6}$/);
    expect(m.langColor.rust).toMatch(/^#[0-9a-f]{6}$/);
  });

  it("keeps domain and range as parallel arrays over the series colors", () => {
    const m = colorModel(["rust", "node"], ["arm64"]);
    expect(m.domain).toEqual(Object.keys(m.color));
    expect(m.range).toEqual(Object.values(m.color));
    expect(m.domain.length).toBe(m.range.length);
  });

  it("shades the two arches of a language differently", () => {
    const m = colorModel(["rust"], ["arm64", "x86_64"]);
    expect(m.color["rust arm64"]).not.toBe(m.color["rust x86_64"]);
  });

  it("assigns distinct hues to known languages", () => {
    const m = colorModel(["rust", "node", "java", "python"], ["arm64"]);
    const colors = Object.values(m.langColor);
    expect(new Set(colors).size).toBe(colors.length);
  });

  it("assigns a color to an unknown language without throwing", () => {
    const m = colorModel(["brainfuck"], ["arm64"]);
    expect(m.langColor.brainfuck).toMatch(/^#[0-9a-f]{6}$/);
  });

  it("gives a runtime's SnapStart pseudo-language its own derived hue", () => {
    // python-snapstart has no explicit hue entry; it must still get a color,
    // distinct from base python, rather than colliding or throwing.
    const m = colorModel(["python", "python-snapstart"], ["arm64"]);
    expect(m.langColor["python-snapstart"]).toMatch(/^#[0-9a-f]{6}$/);
    expect(m.langColor["python-snapstart"]).not.toBe(m.langColor.python);
  });
});
