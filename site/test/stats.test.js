import { describe, it, expect } from "vitest";
import {
  quantile,
  cleanSort,
  median,
  midOf,
  summarizeValues,
} from "../src/lib/stats.js";

describe("quantile", () => {
  it("returns NaN for an empty array", () => {
    expect(quantile([], 0.5)).toBeNaN();
  });

  it("returns the endpoints at q=0 and q=1", () => {
    const s = [1, 2, 3, 4, 5];
    expect(quantile(s, 0)).toBe(1);
    expect(quantile(s, 1)).toBe(5);
  });

  it("hits an exact element when the position is integral", () => {
    // (5-1)*0.5 = 2 -> index 2 exactly, no interpolation.
    expect(quantile([1, 2, 3, 4, 5], 0.5)).toBe(3);
  });

  it("linearly interpolates between neighbours", () => {
    // (4-1)*0.5 = 1.5 -> halfway between index 1 (20) and index 2 (30).
    expect(quantile([10, 20, 30, 40], 0.5)).toBe(25);
    // (4-1)*0.1 = 0.3 -> 10 + 0.3*(20-10) = 13.
    expect(quantile([10, 20, 30, 40], 0.1)).toBeCloseTo(13, 10);
  });

  it("returns the single value for a one-element array at any q", () => {
    expect(quantile([42], 0)).toBe(42);
    expect(quantile([42], 0.9)).toBe(42);
    expect(quantile([42], 1)).toBe(42);
  });

  it("throws on an out-of-range q rather than returning undefined", () => {
    expect(() => quantile([1, 2, 3], 1.5)).toThrow(/q must be in/);
    expect(() => quantile([1, 2, 3], -0.1)).toThrow(/q must be in/);
  });
});

describe("cleanSort", () => {
  it("drops null, undefined and NaN, then sorts ascending", () => {
    const out = cleanSort([3, null, 1, NaN, 2, undefined]);
    expect(out).toEqual([1, 2, 3]);
  });

  it("sorts numerically, not lexicographically", () => {
    expect(cleanSort([10, 2, 1, 20])).toEqual([1, 2, 10, 20]);
  });

  it("keeps zero (zero is a valid value, not nullish)", () => {
    expect(cleanSort([0, -1, 1])).toEqual([-1, 0, 1]);
  });

  it("does not mutate its input", () => {
    const input = [3, 1, 2];
    cleanSort(input);
    expect(input).toEqual([3, 1, 2]);
  });

  it("returns an empty array when everything is dropped", () => {
    expect(cleanSort([null, NaN, undefined])).toEqual([]);
  });

  it("drops non-numbers and ±Infinity (only finite numbers survive)", () => {
    expect(cleanSort([3, "2", 1, Infinity, -Infinity, true])).toEqual([1, 3]);
  });
});

describe("median", () => {
  it("returns null for an empty array", () => {
    expect(median([])).toBeNull();
  });

  it("returns null when every entry is nullish/NaN", () => {
    expect(median([null, NaN, undefined])).toBeNull();
  });

  it("computes the median of an unsorted array", () => {
    expect(median([5, 1, 3, 2, 4])).toBe(3);
  });

  it("averages the two middle values for even length", () => {
    expect(median([1, 2, 3, 4])).toBe(2.5);
  });

  it("ignores null/NaN entries when computing", () => {
    expect(median([3, null, 1, NaN, 2])).toBe(2);
  });

  it("does not mutate its input", () => {
    const input = [5, 1, 3];
    median(input);
    expect(input).toEqual([5, 1, 3]);
  });
});

describe("midOf", () => {
  it("returns undefined for an empty array", () => {
    expect(midOf([])).toBeUndefined();
  });

  it("returns the only element for length 1", () => {
    expect(midOf([7])).toBe(7);
  });

  it("returns the lower-middle element for even length", () => {
    // length 4 -> floor((4-1)/2) = index 1.
    expect(midOf([128, 256, 512, 1024])).toBe(256);
  });

  it("returns the middle element for odd length", () => {
    expect(midOf([128, 256, 512])).toBe(256);
  });
});

describe("summarizeValues", () => {
  it("returns null for an empty array", () => {
    expect(summarizeValues([])).toBeNull();
  });

  it("returns null when every entry is nullish/NaN", () => {
    expect(summarizeValues([null, NaN])).toBeNull();
  });

  it("reports basic stats and counts only clean samples", () => {
    const s = summarizeValues([1, 2, 3, 4, 5, null, NaN]);
    expect(s.n).toBe(5);
    expect(s.min).toBe(1);
    expect(s.max).toBe(5);
    expect(s.p50).toBe(3);
    expect(s.mean).toBe(3);
  });

  it("gates p99 below 200 samples and reports it at/above 200", () => {
    const below = summarizeValues(Array.from({ length: 199 }, (_, i) => i));
    expect(below.p99).toBeNull();
    expect(below.p999).toBeNull();

    const at = summarizeValues(Array.from({ length: 200 }, (_, i) => i));
    expect(at.p99).not.toBeNull();
    expect(typeof at.p99).toBe("number");
    // Still below the p999 threshold of 1000.
    expect(at.p999).toBeNull();
  });

  it("gates p999 below 1000 samples and reports it at/above 1000", () => {
    const below = summarizeValues(Array.from({ length: 999 }, (_, i) => i));
    expect(below.p999).toBeNull();

    const at = summarizeValues(Array.from({ length: 1000 }, (_, i) => i));
    expect(at.p999).not.toBeNull();
    expect(typeof at.p999).toBe("number");
  });

  it("computes mean over the clean samples only", () => {
    // Mean of 0..99 is 49.5; the null/NaN must not skew it.
    const s = summarizeValues([
      ...Array.from({ length: 100 }, (_, i) => i),
      null,
      NaN,
    ]);
    expect(s.mean).toBeCloseTo(49.5, 10);
    expect(s.n).toBe(100);
  });
});
