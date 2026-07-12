import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { lifecycleRegex, newestResultsFile } from "../src/lib/results-input.js";

// The discovery helper is the shared newest-input picker for the probe loaders.
// These tests exercise it against a temp dir (via the `dir` override), so they are
// hermetic and touch neither the repo results/ nor any pipeline-produced file.
describe("lifecycleRegex", () => {
  it("matches a run-scoped file for its kind and captures the unix_ms", () => {
    const m = "lifecycle-download-start-1783000000000-038c4a87.json".match(
      lifecycleRegex("download-start"),
    );
    expect(m).not.toBeNull();
    expect(m[1]).toBe("1783000000000");
  });

  it("does not confuse download-scaling with download-scaling-image", () => {
    const scaling = lifecycleRegex("download-scaling");
    const image = lifecycleRegex("download-scaling-image");
    const imageName =
      "lifecycle-download-scaling-image-1783000000000-038c4a87.json";
    const zipName = "lifecycle-download-scaling-1783000000000-038c4a87.json";
    // The zip regex must reject the image file (the char after `scaling-` is "i",
    // not a digit) and vice-versa, so a prefix collision can never mis-route.
    expect(scaling.test(imageName)).toBe(false);
    expect(scaling.test(zipName)).toBe(true);
    expect(image.test(imageName)).toBe(true);
    expect(image.test(zipName)).toBe(false);
  });

  it("rejects a name with no run id", () => {
    expect(
      lifecycleRegex("download-start").test("lifecycle-download-start.json"),
    ).toBe(false);
  });
});

describe("newestResultsFile", () => {
  let dir;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "lbresults-"));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  function seed(names) {
    for (const n of names) writeFileSync(join(dir, n), "{}");
  }

  it("returns envPath verbatim without scanning", () => {
    const chosen = newestResultsFile({
      re: lifecycleRegex("download-start"),
      label: "x",
      envPath: "/explicit/path.json",
      dir,
    });
    expect(chosen).toBe("/explicit/path.json");
  });

  it("throws with a helpful message when the dir has no match", () => {
    seed(["run-123-abc.jsonl.gz", "unrelated.json"]);
    expect(() =>
      newestResultsFile({
        re: lifecycleRegex("download-start"),
        label: "download-start probe",
        dir,
      }),
    ).toThrow(/no download-start probe input/);
  });

  it("picks the newest by unix_ms, not lexically", () => {
    // 9-digit vs 13-digit ms: lexical sort would misrank; numeric must win.
    seed([
      "lifecycle-download-start-999999999-aaaaaaaa.json",
      "lifecycle-download-start-1783000000000-bbbbbbbb.json",
      "lifecycle-download-start-1782000000000-cccccccc.json",
    ]);
    const chosen = newestResultsFile({
      re: lifecycleRegex("download-start"),
      label: "download-start probe",
      dir,
    });
    expect(
      chosen.endsWith("lifecycle-download-start-1783000000000-bbbbbbbb.json"),
    ).toBe(true);
  });

  it("ignores other kinds in the same dir", () => {
    seed([
      "lifecycle-download-scaling-1783000000000-aaaaaaaa.json",
      "lifecycle-download-scaling-image-1783000000000-bbbbbbbb.json",
      "lifecycle-download-start-1700000000000-cccccccc.json",
    ]);
    const chosen = newestResultsFile({
      re: lifecycleRegex("download-scaling"),
      label: "download-scaling probe",
      dir,
    });
    expect(
      chosen.endsWith("lifecycle-download-scaling-1783000000000-aaaaaaaa.json"),
    ).toBe(true);
  });
});
