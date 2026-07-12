import { describe, it, expect } from "vitest";
import {
  titleCase,
  fmtMs,
  fmtBytes,
  fmtUsd,
  fmtDateUtc,
  langLabel,
  seriesLabel,
  scenarioLabel,
  shortScenario,
} from "../src/lib/format.js";

describe("titleCase", () => {
  it("capitalizes the first character", () => {
    expect(titleCase("rust")).toBe("Rust");
  });
});

describe("fmtMs", () => {
  it("renders an em-dash for null", () => {
    expect(fmtMs(null)).toBe("—");
  });

  it("uses two decimals below 10 ms", () => {
    expect(fmtMs(1.234)).toBe("1.23ms");
    expect(fmtMs(9.999)).toBe("10.00ms");
  });

  it("uses whole milliseconds at/above 10 ms", () => {
    expect(fmtMs(10)).toBe("10ms");
    expect(fmtMs(123.6)).toBe("124ms");
  });
});

describe("fmtBytes", () => {
  it("renders an em-dash for null", () => {
    expect(fmtBytes(null)).toBe("—");
  });

  it("renders bytes below 1 KB", () => {
    expect(fmtBytes(512)).toBe("512 B");
  });

  it("renders KB between 1e3 and 1e6", () => {
    expect(fmtBytes(2048)).toBe("2 KB");
  });

  it("renders MB at/above 1e6", () => {
    expect(fmtBytes(3_500_000)).toBe("3.5 MB");
  });
});

describe("fmtUsd", () => {
  it("renders an em-dash for null", () => {
    expect(fmtUsd(null)).toBe("—");
  });

  it("uses three decimals below $1", () => {
    expect(fmtUsd(0.1234)).toBe("$0.123");
  });

  it("uses two decimals at/above $1", () => {
    expect(fmtUsd(12.345)).toBe("$12.35");
  });
});

describe("fmtDateUtc", () => {
  it("renders the UTC calendar date of a unix-ms timestamp", () => {
    // 1782427954697 ms = 2026-06-25T22:52:34.697Z
    expect(fmtDateUtc(1782427954697)).toBe("2026-06-25");
  });

  it("uses the UTC day even when the local day would differ", () => {
    // 2026-06-26T05:07:34.222Z stays the 26th regardless of timezone offset.
    expect(fmtDateUtc(1782450454222)).toBe("2026-06-26");
  });

  it("returns null for missing or non-positive input", () => {
    expect(fmtDateUtc(null)).toBe(null);
    expect(fmtDateUtc(undefined)).toBe(null);
    expect(fmtDateUtc(0)).toBe(null);
    expect(fmtDateUtc(NaN)).toBe(null);
  });
});

describe("langLabel", () => {
  it("special-cases the SnapStart pseudo-language", () => {
    expect(langLabel("java-snapstart")).toBe("Java SnapStart");
  });

  it("labels a SnapStart pseudo-language for any runtime", () => {
    expect(langLabel("python-snapstart")).toBe("Python SnapStart");
  });

  it("title-cases any other language", () => {
    expect(langLabel("rust")).toBe("Rust");
  });
});

describe("seriesLabel", () => {
  it("humanizes the language half of a lang-arch key and keeps the arch token", () => {
    expect(seriesLabel("rust arm64")).toBe("Rust arm64");
  });

  it("humanizes a SnapStart pseudo-language series key", () => {
    expect(seriesLabel("java-snapstart x86_64")).toBe("Java SnapStart x86_64");
  });

  it("falls back to langLabel for a bare language key with no arch", () => {
    expect(seriesLabel("node")).toBe("Node");
  });
});

describe("scenarioLabel", () => {
  it("returns the known display name", () => {
    expect(scenarioLabel("oneclient")).toBe("1 AWS client (DDB)");
  });

  it("falls back to the raw id for an unknown scenario", () => {
    expect(scenarioLabel("madeup")).toBe("madeup");
  });
});

describe("shortScenario", () => {
  it("returns the known short label", () => {
    expect(shortScenario("threeclient")).toBe("3-client");
  });

  it("falls back to the full scenario label for an unknown scenario", () => {
    // Unknown short id -> scenarioLabel, which itself falls back to the raw id.
    expect(shortScenario("madeup")).toBe("madeup");
  });
});
