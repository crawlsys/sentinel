import { describe, it, expect } from "vitest";

import { makeDollars } from "@/domain/dollars";
import {
  HUMAN_USD_PER_DAY,
  HUMAN_USD_PER_POINT,
  compute,
  roiForDays,
  roiForPoints,
} from "@/domain/roi";

describe("compute", () => {
  it("returns the simple ratio", () => {
    expect(compute(makeDollars(100), makeDollars(20))).toBeCloseTo(5, 6);
  });

  it("returns 1 when costs match", () => {
    expect(compute(makeDollars(50), makeDollars(50))).toBeCloseTo(1, 6);
  });

  it("returns < 1 when claude is more expensive", () => {
    expect(compute(makeDollars(10), makeDollars(40))).toBeCloseTo(0.25, 6);
  });

  it("returns Infinity when claudeCost is 0", () => {
    expect(compute(makeDollars(100), makeDollars(0))).toBe(Infinity);
  });

  it("rejects negative costs", () => {
    expect(() => compute(makeDollars(-1), makeDollars(1))).toThrow(RangeError);
    expect(() => compute(makeDollars(1), makeDollars(-1))).toThrow(RangeError);
  });
});

describe("constants", () => {
  it("HUMAN_USD_PER_POINT is 327", () => {
    expect(HUMAN_USD_PER_POINT).toBe(327);
  });

  it("HUMAN_USD_PER_DAY is 654", () => {
    expect(HUMAN_USD_PER_DAY).toBe(654);
  });
});

describe("roiForPoints", () => {
  it("scales by HUMAN_USD_PER_POINT", () => {
    // 5 points * $327 = $1635 human cost; vs $327 claude cost = 5x ROI.
    expect(roiForPoints(5, makeDollars(327))).toBeCloseTo(5, 6);
  });

  it("rejects negative points", () => {
    expect(() => roiForPoints(-1, makeDollars(10))).toThrow(RangeError);
  });
});

describe("roiForDays", () => {
  it("scales by HUMAN_USD_PER_DAY", () => {
    // 2 days * $654 = $1308 human cost; vs $654 claude cost = 2x ROI.
    expect(roiForDays(2, makeDollars(654))).toBeCloseTo(2, 6);
  });

  it("rejects negative days", () => {
    expect(() => roiForDays(-1, makeDollars(10))).toThrow(RangeError);
  });
});
