import { describe, it, expect } from "vitest";
import {
  aggregate,
  assertRunComplete,
  assertRowsMatchMeta,
} from "../src/lib/aggregate.js";

// Build a projected row with sane defaults; override per test. Matches the
// shape the loader projects each raw line down to.
function row(overrides = {}) {
  return {
    lang: "rust",
    scenario: "hello",
    arch: "arm64",
    memory_mb: 512,
    opt: null,
    snapstart: false,
    jitter: null,
    cycle: 0,
    is_cold: false,
    init_ms: null,
    restore_ms: null,
    duration_ms: 1.0,
    billed_ms: 1,
    max_memory_used_mb: 64,
    artifact_unzipped_bytes: null,
    artifact_zip_bytes: null,
    ...overrides,
  };
}

const warm = (o = {}) => row({ is_cold: false, ...o });
const cold = (o = {}) =>
  row({ is_cold: true, init_ms: 100, duration_ms: 5, ...o });

describe("assertRunComplete", () => {
  it("passes for an empty meta (older runs without completeness fields)", () => {
    expect(() => assertRunComplete({})).not.toThrow();
  });

  it("passes when recorded equals planned", () => {
    expect(() =>
      assertRunComplete({
        total_invocations_recorded: 100,
        total_invocations_planned: 100,
      }),
    ).not.toThrow();
  });

  it("throws when the run is marked failed", () => {
    expect(() =>
      assertRunComplete({ status: "failed", run_id: "run-1-abc" }),
    ).toThrow(/status="failed"/);
  });

  it("throws when the run is still marked running (crashed mid-run)", () => {
    // A "running" meta never reached a terminal state; reject on the status alone
    // rather than relying on the recorded<planned coincidence.
    expect(() =>
      assertRunComplete({ status: "running", run_id: "run-2-def" }),
    ).toThrow(/status="running"/);
  });

  it("throws when fewer invocations were recorded than planned", () => {
    expect(() =>
      assertRunComplete({
        total_invocations_recorded: 90,
        total_invocations_planned: 100,
      }),
    ).toThrow(/incomplete dataset/);
  });

  it("does not throw on a partial signal where only one count is present", () => {
    expect(() =>
      assertRunComplete({ total_invocations_recorded: 90 }),
    ).not.toThrow();
    expect(() =>
      assertRunComplete({ total_invocations_planned: 100 }),
    ).not.toThrow();
  });
});

describe("assertRowsMatchMeta", () => {
  it("passes when the streamed row count matches the recorded count", () => {
    expect(() =>
      assertRowsMatchMeta(
        100,
        { total_invocations_recorded: 100 },
        "run.jsonl.gz",
      ),
    ).not.toThrow();
  });

  it("throws when the data file is truncated below the recorded count", () => {
    // The meta says the run recorded 100 invocations but the file streamed only
    // 90: the .jsonl.gz was truncated after the meta was stamped complete.
    expect(() =>
      assertRowsMatchMeta(
        90,
        { total_invocations_recorded: 100, run_id: "run-3-ghi" },
        "run.jsonl.gz",
      ),
    ).toThrow(/holds 90 rows but its meta records 100/);
  });

  it("throws when the file holds more rows than recorded (mismatched pairing)", () => {
    expect(() =>
      assertRowsMatchMeta(
        110,
        { total_invocations_recorded: 100 },
        "run.jsonl.gz",
      ),
    ).toThrow(/truncated or mismatched/);
  });

  it("passes for an older meta without the recorded field", () => {
    expect(() => assertRowsMatchMeta(90, {}, "run.jsonl.gz")).not.toThrow();
  });
});

describe("aggregate: input guards", () => {
  it("throws on empty rows", () => {
    expect(() => aggregate([])).toThrow(/no rows/);
  });
});

describe("aggregate: loud-fail data-quality gates", () => {
  it("throws when a cold-flagged row carries no init/restore marker", () => {
    const rows = [warm(), cold({ init_ms: null, restore_ms: null })];
    expect(() => aggregate(rows)).toThrow(/no .*init_ms\/restore_ms marker/);
  });

  it("throws when a cold sample has a marker but a non-finite duration", () => {
    const rows = [warm(), cold({ duration_ms: null })];
    expect(() => aggregate(rows)).toThrow(/null\/non-finite/);
  });

  it("throws when a cold row's marker is NaN (not merely absent)", () => {
    // A NaN marker passes a bare `!= null` check but would sum to a NaN cold
    // total that cleanSort silently drops; it must trip the marker gate instead.
    const rows = [warm(), cold({ init_ms: NaN })];
    expect(() => aggregate(rows)).toThrow(/no .*init_ms\/restore_ms marker/);
  });

  it("throws when a warm row carries a non-finite duration", () => {
    const rows = [warm({ duration_ms: null }), cold()];
    expect(() => aggregate(rows)).toThrow(/warm sample.*null\/non-finite/);
  });

  it("does not throw for a warm row with a null marker (markers are cold-only)", () => {
    expect(() =>
      aggregate([warm({ init_ms: null, restore_ms: null })]),
    ).not.toThrow();
  });

  it("throws when a warm row's footprint is present but non-finite", () => {
    // NaN passes a bare `!= null` check but is silently dropped by cleanSort, so
    // footprint.n would diverge from warm.n without a trace.
    const rows = [warm({ max_memory_used_mb: NaN }), cold()];
    expect(() => aggregate(rows)).toThrow(/non-finite max_memory_used_mb/);
  });

  it("throws when a warm row's cost input is present but non-finite", () => {
    const rows = [warm({ billed_ms: NaN }), cold()];
    expect(() => aggregate(rows)).toThrow(/non-finite cost/);
  });

  it("does not throw for a warm row with a null footprint/cost field (legitimately absent)", () => {
    expect(() =>
      aggregate([warm({ max_memory_used_mb: null, billed_ms: null }), cold()]),
    ).not.toThrow();
  });

  it("throws when a runtime name collides with the reserved SnapStart suffix", () => {
    // A literal `foo-snapstart` runtime would be indistinguishable from a
    // SnapStart variant once collapsed into a series key; reject it loudly.
    expect(() => aggregate([warm({ lang: "foo-snapstart" })])).toThrow(
      /reserved SnapStart/,
    );
  });
});

describe("aggregate: cold marker selection", () => {
  it("uses init_ms for a plain cold start", () => {
    const out = aggregate([
      cold({ init_ms: 100, restore_ms: null, duration_ms: 5 }),
    ]);
    const c = out.cells[0];
    expect(c.coldInit.p50).toBe(100);
    // cold total = marker + first-request duration
    expect(c.cold.p50).toBe(105);
    expect(c.coldFirstReq.p50).toBe(5);
  });

  it("uses restore_ms for a SnapStart cold start when init_ms is absent", () => {
    const out = aggregate([
      cold({
        lang: "java",
        snapstart: true,
        init_ms: null,
        restore_ms: 40,
        duration_ms: 3,
      }),
    ]);
    const c = out.cells[0];
    expect(c.coldInit.p50).toBe(40);
    expect(c.cold.p50).toBe(43);
  });
});

describe("aggregate: representative collapse", () => {
  it("keeps only Rust o3 in the headline cells, dropping oz", () => {
    const rows = [
      warm({ opt: "o3", duration_ms: 10 }),
      warm({ opt: "oz", duration_ms: 99 }),
    ];
    const out = aggregate(rows);
    // Headline cells collapse to the o3 build only.
    expect(out.cells).toHaveLength(1);
    expect(out.cells[0].warm.p50).toBe(10);
  });

  it("drops Rust jitter=on rows from the headline cells", () => {
    const rows = [
      warm({ opt: "o3", jitter: "off", duration_ms: 10 }),
      warm({ opt: "o3", jitter: "on", duration_ms: 99 }),
    ];
    const out = aggregate(rows);
    expect(out.cells).toHaveLength(1);
    expect(out.cells[0].warm.p50).toBe(10);
  });

  it("keeps SnapStart as its own peer series, not collapsed into java", () => {
    const rows = [
      warm({ lang: "java", snapstart: false, duration_ms: 10 }),
      warm({ lang: "java", snapstart: true, duration_ms: 20 }),
    ];
    const out = aggregate(rows);
    const series = out.cells.map((c) => c.series).sort();
    expect(series).toEqual(["java arm64", "java-snapstart arm64"]);
    expect(out.dimensions.languages).toContain("java-snapstart");
  });
});

describe("aggregate: warmCycles (effective independent count for the warm tail)", () => {
  it("counts distinct cold cycles the warm samples span, not the raw warm n", () => {
    // 6 warm samples across only 2 distinct cycles: warm.n is 6, but the tail
    // rests on 2 independent sandboxes.
    const rows = [
      warm({ cycle: 0, duration_ms: 1 }),
      warm({ cycle: 0, duration_ms: 2 }),
      warm({ cycle: 0, duration_ms: 3 }),
      warm({ cycle: 1, duration_ms: 4 }),
      warm({ cycle: 1, duration_ms: 5 }),
      warm({ cycle: 1, duration_ms: 6 }),
    ];
    const out = aggregate(rows);
    expect(out.cells).toHaveLength(1);
    expect(out.cells[0].warm.n).toBe(6);
    expect(out.cells[0].warmCycles).toBe(2);
  });

  it("is null when rows carry no cycle (older runs)", () => {
    const rows = [warm({ cycle: undefined, duration_ms: 1 })];
    const out = aggregate(rows);
    expect(out.cells[0].warmCycles).toBeNull();
  });

  it("does not count cold-only cycles toward the warm effective count", () => {
    // Cold samples do not contribute to warmCycles; only cycles that produced a
    // warm sample count.
    const rows = [
      cold({ cycle: 0, init_ms: 100, duration_ms: 5 }),
      warm({ cycle: 0, duration_ms: 1 }),
      cold({ cycle: 1, init_ms: 100, duration_ms: 5 }),
    ];
    const out = aggregate(rows);
    expect(out.cells[0].warmCycles).toBe(1);
  });
});

describe("aggregate: dimensions", () => {
  it("orders scenarios canonically, then unknowns sorted after", () => {
    const rows = [
      warm({ scenario: "zzz-custom" }),
      warm({ scenario: "oneclient" }),
      warm({ scenario: "hello" }),
    ];
    const out = aggregate(rows);
    expect(out.dimensions.scenarios).toEqual([
      "hello",
      "oneclient",
      "zzz-custom",
    ]);
  });

  it("picks 1024 as the pivot memory when present", () => {
    const rows = [
      warm({ memory_mb: 256 }),
      warm({ memory_mb: 1024 }),
      warm({ memory_mb: 2048 }),
    ];
    expect(aggregate(rows).dimensions.pivotMem).toBe(1024);
  });

  it("falls back to the middle tier when 1024 is absent", () => {
    const rows = [
      warm({ memory_mb: 256 }),
      warm({ memory_mb: 512 }),
      warm({ memory_mb: 2048 }),
    ];
    // sorted [256, 512, 2048], lower-middle index 1 -> 512
    expect(aggregate(rows).dimensions.pivotMem).toBe(512);
  });

  it("sets feature flags from the data", () => {
    const base = [warm()];
    expect(aggregate(base).dimensions.hasOpt).toBe(false);
    expect(aggregate(base).dimensions.hasSnapStart).toBe(false);
    expect(aggregate(base).dimensions.hasJitter).toBe(false);

    expect(aggregate([...base, warm({ opt: "oz" })]).dimensions.hasOpt).toBe(
      true,
    );
    expect(
      aggregate([...base, cold({ lang: "java", snapstart: true })]).dimensions
        .hasSnapStart,
    ).toBe(true);
    expect(
      aggregate([...base, cold({ jitter: "on" })]).dimensions.hasJitter,
    ).toBe(true);
  });
});

describe("aggregate: cost pricing", () => {
  it("prices arm64 cheaper than x86_64 for the same GB-seconds", () => {
    const armOut = aggregate(
      Array.from({ length: 5 }, () =>
        warm({ arch: "arm64", memory_mb: 1024, billed_ms: 1000 }),
      ),
    );
    const x86Out = aggregate(
      Array.from({ length: 5 }, () =>
        warm({ arch: "x86_64", memory_mb: 1024, billed_ms: 1000 }),
      ),
    );
    expect(armOut.cells[0].cost.p50).toBeLessThan(x86Out.cells[0].cost.p50);
  });

  it("throws on an unknown arch rather than silently pricing it as x86_64", () => {
    expect(() => aggregate([warm({ arch: "riscv64" })])).toThrow(
      /no GB-second price for arch/,
    );
  });
});

describe("aggregate: optCells (Rust opt-level A/B)", () => {
  it("emits one cell per (arch, scenario, opt, memory) with medians and artifact sizes", () => {
    const rows = [
      cold({
        opt: "o3",
        init_ms: 100,
        artifact_zip_bytes: 1000,
        artifact_unzipped_bytes: 3000,
      }),
      warm({ opt: "o3", duration_ms: 10 }),
      cold({ opt: "oz", init_ms: 120 }),
      warm({ opt: "oz", duration_ms: 12 }),
    ];
    const out = aggregate(rows);
    expect(out.dimensions.hasOpt).toBe(true);
    const o3 = out.optCells.find((c) => c.opt === "o3");
    const oz = out.optCells.find((c) => c.opt === "oz");
    expect(o3.coldInitP50).toBe(100);
    expect(o3.warmP50).toBe(10);
    expect(o3.zip).toBe(1000);
    expect(oz.coldInitP50).toBe(120);
  });

  it("drops Rust jitter=on rows so the o3-vs-oz cell is not jitter-contaminated", () => {
    // The jitter A/B (o3-only) shares the opt cell's key, so a jitter=on row
    // would otherwise inflate the o3 cold-init median above the jitter=off build
    // the oz cell is compared against.
    const rows = [
      cold({ opt: "o3", jitter: "off", init_ms: 100, duration_ms: 5 }),
      cold({ opt: "o3", jitter: "on", init_ms: 900, duration_ms: 5 }),
      warm({ opt: "o3", jitter: "off", duration_ms: 10 }),
      cold({ opt: "oz", jitter: "off", init_ms: 120, duration_ms: 5 }),
      warm({ opt: "oz", jitter: "off", duration_ms: 12 }),
    ];
    const out = aggregate(rows);
    const o3 = out.optCells.find((c) => c.opt === "o3");
    // Only the jitter=off cold sample counts, so the median is 100, not 500.
    expect(o3.coldInitP50).toBe(100);
  });
});

describe("aggregate: snapCells (SnapStart A/B)", () => {
  it("emits cold total P50 per snapstart variant", () => {
    const rows = [
      cold({ lang: "java", snapstart: false, init_ms: 400, duration_ms: 10 }),
      cold({
        lang: "java",
        snapstart: true,
        init_ms: null,
        restore_ms: 80,
        duration_ms: 5,
      }),
    ];
    const out = aggregate(rows);
    expect(out.dimensions.hasSnapStart).toBe(true);
    const off = out.snapCells.find((c) => c.snapstart === false);
    const on = out.snapCells.find((c) => c.snapstart === true);
    expect(off.coldP50).toBe(410);
    expect(on.coldP50).toBe(85);
    // Each A/B cell carries its raw runtime so the chart can disambiguate.
    expect(off.lang).toBe("java");
    expect(on.lang).toBe("java");
  });

  it("keys A/B cells per runtime so a second SnapStart runtime does not merge into Java", () => {
    const rows = [
      cold({ lang: "java", snapstart: false, init_ms: 400, duration_ms: 10 }),
      cold({
        lang: "java",
        snapstart: true,
        init_ms: null,
        restore_ms: 80,
        duration_ms: 5,
      }),
      cold({ lang: "python", snapstart: false, init_ms: 200, duration_ms: 8 }),
      cold({
        lang: "python",
        snapstart: true,
        init_ms: null,
        restore_ms: 40,
        duration_ms: 4,
      }),
    ];
    const out = aggregate(rows);
    const py = out.snapCells.filter((c) => c.lang === "python");
    expect(py.find((c) => c.snapstart === false).coldP50).toBe(208);
    expect(py.find((c) => c.snapstart === true).coldP50).toBe(44);
    // Java cells are untouched by the presence of a second SnapStart runtime.
    expect(out.snapCells.filter((c) => c.lang === "java")).toHaveLength(2);
  });

  it("omits a runtime with no SnapStart variant from the A/B (no orphan plain half)", () => {
    const rows = [
      cold({ lang: "java", snapstart: false, init_ms: 400, duration_ms: 10 }),
      cold({
        lang: "java",
        snapstart: true,
        init_ms: null,
        restore_ms: 80,
        duration_ms: 5,
      }),
      cold({ lang: "rust", snapstart: false, init_ms: 50, duration_ms: 2 }),
    ];
    const out = aggregate(rows);
    expect(out.snapCells.some((c) => c.lang === "rust")).toBe(false);
  });

  it("keeps a non-Java SnapStart runtime as its own series rather than merging into java-snapstart", () => {
    const rows = [
      cold({
        lang: "java",
        snapstart: true,
        init_ms: null,
        restore_ms: 80,
        duration_ms: 5,
      }),
      cold({
        lang: "python",
        snapstart: true,
        init_ms: null,
        restore_ms: 40,
        duration_ms: 4,
      }),
    ];
    const out = aggregate(rows);
    expect(out.dimensions.languages).toContain("java-snapstart");
    expect(out.dimensions.languages).toContain("python-snapstart");
    // The two SnapStart runtimes never collapse into a single contaminated series.
    expect(out.cells.some((c) => c.lang === "java-snapstart")).toBe(true);
    expect(out.cells.some((c) => c.lang === "python-snapstart")).toBe(true);
  });
});

describe("aggregate: jitterCells (Rust jitter-entropy A/B)", () => {
  it("splits the tax across init / firstReq / total per jitter variant", () => {
    const rows = [
      cold({ opt: "o3", jitter: "off", init_ms: 50, duration_ms: 5 }),
      cold({ opt: "o3", jitter: "on", init_ms: 90, duration_ms: 20 }),
    ];
    const out = aggregate(rows);
    expect(out.dimensions.hasJitter).toBe(true);
    const on = out.jitterCells.find((c) => c.jitter === "on");
    expect(on.initP50).toBe(90);
    expect(on.firstReqP50).toBe(20);
    expect(on.totalP50).toBe(110);
  });
});

describe("aggregate: artifacts", () => {
  it("prefers arm64 over x86_64 for the representative size", () => {
    const rows = [
      warm({
        arch: "x86_64",
        scenario: "hello",
        artifact_zip_bytes: 2000,
        artifact_unzipped_bytes: 5000,
      }),
      warm({
        arch: "arm64",
        scenario: "hello",
        artifact_zip_bytes: 1800,
        artifact_unzipped_bytes: 4800,
      }),
    ];
    const out = aggregate(rows);
    const a = out.artifacts.find((x) => x.scenario === "hello");
    expect(a.arch).toBe("arm64");
    expect(a.zip).toBe(1800);
    expect(out.dimensions.hasUnzippedSize).toBe(true);
    // Internal grouping key must not leak into the payload.
    expect(a._key).toBeUndefined();
  });

  it("does not emit a SnapStart artifact (it shares Java's)", () => {
    const rows = [
      warm({
        lang: "java",
        snapstart: true,
        scenario: "hello",
        artifact_zip_bytes: 9000,
      }),
    ];
    const out = aggregate(rows);
    expect(
      out.artifacts.find((a) => a.lang === "java-snapstart"),
    ).toBeUndefined();
  });
});

describe("aggregate: distribution thinning", () => {
  it("keeps small groups intact (at/below the body-dot cap)", () => {
    const rows = Array.from({ length: 50 }, (_, i) =>
      warm({ duration_ms: i + 1 }),
    );
    const out = aggregate(rows);
    const group = out.dist[512].find((g) => g.kind === "warm");
    expect(group.values).toHaveLength(50);
  });

  it("thins a large group toward the cap while preserving min and full tail", () => {
    // 5000 warm samples 1..5000. Body cap is 220 dots; tail (>=P98) is kept whole.
    const rows = Array.from({ length: 5000 }, (_, i) =>
      warm({ duration_ms: i + 1 }),
    );
    const out = aggregate(rows);
    const group = out.dist[512].find((g) => g.kind === "warm");
    // 220 body dots + the full tail slice from index floor(0.98*4999)=4899 -> 101 points.
    expect(group.values.length).toBe(220 + 101);
    // Min preserved, max (top of tail) preserved.
    expect(Math.min(...group.values)).toBe(1);
    expect(Math.max(...group.values)).toBe(5000);
    // distMedians is computed from the FULL data, not the thinned sample.
    const med = out.distMedians[512].find((g) => g.kind === "warm");
    expect(med.n).toBe(5000);
    expect(med.median).toBeCloseTo(2500.5, 5);
  });

  it("never emits more dots than the input just above the cap (no oversampling/dup)", () => {
    // Just above MAX_BODY_DOTS (220): a body of <= 220 points must be kept whole,
    // not strided with a fractional step (which would re-emit and inflate it).
    for (const n of [221, 222, 225, 230]) {
      const rows = Array.from({ length: n }, (_, i) =>
        warm({ duration_ms: i + 1 }),
      );
      const out = aggregate(rows);
      const group = out.dist[512].find((g) => g.kind === "warm");
      expect(group.values.length).toBeLessThanOrEqual(n);
      // Values are a subset of the input, with no duplicates introduced.
      expect(new Set(group.values).size).toBe(group.values.length);
      expect(Math.min(...group.values)).toBe(1);
      expect(Math.max(...group.values)).toBe(n);
    }
  });
});

describe("aggregate: kpi and meta", () => {
  it("reports cold-start (total) range, invocation counts, and carries meta provenance", () => {
    // coldRange spans the cold TOTAL (marker + first-request duration_ms), the
    // laziness-neutral headline metric, not the init marker alone: 80+5=85 and
    // 300+5=305.
    const rows = [
      cold({ init_ms: 80, duration_ms: 5 }),
      cold({ init_ms: 300, duration_ms: 5, scenario: "oneclient" }),
      warm({ duration_ms: 2 }),
    ];
    const out = aggregate(
      rows,
      { run_id: "run-1-abc", region: "eu-central-1" },
      { inputBasename: "run-1-abc.jsonl.gz" },
    );
    expect(out.kpi.coldRange).toEqual({ min: 85, max: 305 });
    expect(out.kpi.totalInvocations).toBe(3);
    expect(out.kpi.coldCount).toBe(2);
    expect(out.kpi.warmCount).toBe(1);
    expect(out.meta.run_id).toBe("run-1-abc");
    expect(out.meta.region).toBe("eu-central-1");
    expect(out.meta.input_basename).toBe("run-1-abc.jsonl.gz");
  });

  it("never leaks an account id into the payload meta", () => {
    const out = aggregate([warm()], {
      run_id: "r",
      account_id: "123456789012",
    });
    expect(out.meta.account_id).toBeUndefined();
    expect(JSON.stringify(out)).not.toContain("123456789012");
  });
});
