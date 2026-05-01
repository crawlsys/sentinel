import { describe, it, expect } from "vitest";

import {
  STAGES,
  TIME_BUCKETS,
  bucketize,
  makeHours,
  type Stage,
  type StageTransition,
  type TimeBucket,
} from "@/domain/cycle-time";

describe("makeHours", () => {
  it("accepts non-negative finite values", () => {
    expect(makeHours(0)).toBe(0);
    expect(makeHours(1.5)).toBe(1.5);
    expect(makeHours(168)).toBe(168);
  });

  it("rejects negative values", () => {
    expect(() => makeHours(-1)).toThrow(RangeError);
  });

  it("rejects non-finite values", () => {
    expect(() => makeHours(Number.NaN)).toThrow(TypeError);
    expect(() => makeHours(Number.POSITIVE_INFINITY)).toThrow(TypeError);
    expect(() => makeHours(Number.NEGATIVE_INFINITY)).toThrow(TypeError);
  });
});

describe("bucketize", () => {
  it("covers all 5 buckets", () => {
    const seen = new Set<TimeBucket>();
    seen.add(bucketize(0.5));
    seen.add(bucketize(2));
    seen.add(bucketize(12));
    seen.add(bucketize(30));
    seen.add(bucketize(168));
    expect(seen.size).toBe(5);
    expect([...seen].sort()).toEqual([...TIME_BUCKETS].sort());
  });

  it("respects right-exclusive boundaries", () => {
    expect(bucketize(0)).toBe("< 1h");
    expect(bucketize(0.999)).toBe("< 1h");
    expect(bucketize(1)).toBe("1-4h");
    expect(bucketize(3.999)).toBe("1-4h");
    expect(bucketize(4)).toBe("4-24h");
    expect(bucketize(23.999)).toBe("4-24h");
    expect(bucketize(24)).toBe("1-3d");
    expect(bucketize(71.999)).toBe("1-3d");
    expect(bucketize(72)).toBe("> 3d");
    expect(bucketize(1000)).toBe("> 3d");
  });

  it("rejects negative or non-finite input", () => {
    expect(() => bucketize(-1)).toThrow(RangeError);
    expect(() => bucketize(Number.NaN)).toThrow(RangeError);
  });
});

describe("StageTransition", () => {
  it("type-checks expected shape", () => {
    // Compile-time test — instantiate one to be sure the type is exported.
    const t: StageTransition = {
      from: "Code Review",
      to: "QA Testing",
      ts: new Date(),
      hours: makeHours(2.5),
    };
    expect(t.from).toBe("Code Review");
  });
});

describe("STAGES", () => {
  it("contains every Stage in pipeline order", () => {
    const expected: Stage[] = [
      "Backlog",
      "Todo",
      "In Progress",
      "Code Review",
      "QA Testing",
      "QA Failed",
      "Completed",
    ];
    expect([...STAGES]).toEqual(expected);
  });
});
