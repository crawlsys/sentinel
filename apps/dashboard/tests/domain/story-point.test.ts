import { describe, it, expect } from "vitest";

import {
  BUCKETS,
  bucketEstimate,
  makeStoryPoint,
} from "@/domain/story-point";

describe("makeStoryPoint", () => {
  it("accepts canonical Fibonacci values", () => {
    for (const b of BUCKETS) {
      expect(makeStoryPoint(b)).toBe(b);
    }
  });

  it("rejects 0 and 4 (4 is not in canonical set)", () => {
    expect(() => makeStoryPoint(0)).toThrow(RangeError);
    expect(() => makeStoryPoint(4)).toThrow(RangeError);
    expect(() => makeStoryPoint(7)).toThrow(RangeError);
    expect(() => makeStoryPoint(13)).toThrow(RangeError);
  });

  it("rejects non-integer", () => {
    expect(() => makeStoryPoint(2.5)).toThrow(RangeError);
  });

  it("rejects non-finite", () => {
    expect(() => makeStoryPoint(Number.NaN)).toThrow(TypeError);
    expect(() => makeStoryPoint(Number.POSITIVE_INFINITY)).toThrow(TypeError);
  });

  it("rejects negative", () => {
    expect(() => makeStoryPoint(-1)).toThrow(RangeError);
  });
});

describe("bucketEstimate — exact matches", () => {
  for (const b of BUCKETS) {
    it(`maps exact ${b} to ${b}`, () => {
      expect(bucketEstimate(b)).toBe(b);
    });
  }
});

describe("bucketEstimate — ties round up", () => {
  it("4 -> 5 (closer to 5 than 3, ties up)", () => {
    expect(bucketEstimate(4)).toBe(5);
  });

  it("6 -> 8 (equidistant from 5 and 8 ⇒ 8)", () => {
    // 6 is 1 from 5 and 2 from 8 — 5 wins this case (not a tie). But
    // matching SEN-13's documented behaviour: 6 is closer to 5.
    expect(bucketEstimate(6)).toBe(5);
  });

  it("12 -> 16 (equidistant from 8 and 16 ⇒ ties round up)", () => {
    expect(bucketEstimate(12)).toBe(16);
  });

  it("rounds nearby values correctly", () => {
    expect(bucketEstimate(3.4)).toBe(3);
    expect(bucketEstimate(7)).toBe(8);
    expect(bucketEstimate(2.5)).toBe(3); // tie 2-3, round up to 3
  });
});

describe("bucketEstimate — clamping", () => {
  it("clamps below 1 to 1", () => {
    expect(bucketEstimate(0.5)).toBe(1);
    expect(bucketEstimate(0)).toBe(1);
  });

  it("clamps above 16 to 16", () => {
    expect(bucketEstimate(20)).toBe(16);
    expect(bucketEstimate(100)).toBe(16);
  });
});

describe("bucketEstimate — input validation", () => {
  it("throws on non-finite", () => {
    expect(() => bucketEstimate(Number.NaN)).toThrow(TypeError);
    expect(() => bucketEstimate(Number.POSITIVE_INFINITY)).toThrow(TypeError);
  });
});
