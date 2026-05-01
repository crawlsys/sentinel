import { describe, it, expect } from "vitest";

import {
  DORA_METRICS,
  DORA_TIERS,
  makeChangeFailureRate,
  makeDeployFrequency,
  tierFor,
  type DoraMetric,
  type DoraTier,
} from "@/domain/dora";

interface Case {
  metric: DoraMetric;
  value: number;
  tier: DoraTier;
}

const CASES: Case[] = [
  // lead_time (hours)
  { metric: "lead_time", value: 12, tier: "elite" },
  { metric: "lead_time", value: 100, tier: "high" },
  { metric: "lead_time", value: 500, tier: "medium" },
  { metric: "lead_time", value: 1000, tier: "low" },

  // deploy_freq (per day)
  { metric: "deploy_freq", value: 5, tier: "elite" }, // > 1
  { metric: "deploy_freq", value: 0.5, tier: "high" }, // 1/day to 1/wk
  { metric: "deploy_freq", value: 0.1, tier: "medium" }, // 1/wk to 1/mo
  { metric: "deploy_freq", value: 0.01, tier: "low" }, // < 1/mo

  // change_failure_rate (0-1)
  { metric: "change_failure_rate", value: 0.05, tier: "elite" },
  { metric: "change_failure_rate", value: 0.25, tier: "high" },
  { metric: "change_failure_rate", value: 0.4, tier: "medium" },
  { metric: "change_failure_rate", value: 0.6, tier: "low" },

  // mttr (hours)
  { metric: "mttr", value: 0.5, tier: "elite" },
  { metric: "mttr", value: 12, tier: "high" },
  { metric: "mttr", value: 100, tier: "medium" },
  { metric: "mttr", value: 200, tier: "low" },
];

describe("tierFor — full 4 metrics x 4 tiers matrix", () => {
  for (const c of CASES) {
    it(`${c.metric} = ${c.value} -> ${c.tier}`, () => {
      expect(tierFor(c.metric, c.value)).toBe(c.tier);
    });
  }
});

describe("tierFor — boundary behaviour", () => {
  it("lead_time at 24h boundary -> high (not elite)", () => {
    expect(tierFor("lead_time", 24)).toBe("high");
    expect(tierFor("lead_time", 23.999)).toBe("elite");
  });

  it("deploy_freq at 1/day boundary -> high (not elite)", () => {
    expect(tierFor("deploy_freq", 1)).toBe("high");
    expect(tierFor("deploy_freq", 1.0001)).toBe("elite");
  });

  it("change_failure_rate at 0.15 boundary -> elite (inclusive)", () => {
    expect(tierFor("change_failure_rate", 0.15)).toBe("elite");
    expect(tierFor("change_failure_rate", 0.151)).toBe("high");
  });

  it("mttr at 1h boundary -> high (not elite)", () => {
    expect(tierFor("mttr", 1)).toBe("high");
    expect(tierFor("mttr", 0.999)).toBe("elite");
  });
});

describe("tierFor — input validation", () => {
  it("rejects non-finite values", () => {
    expect(() => tierFor("lead_time", Number.NaN)).toThrow(TypeError);
    expect(() => tierFor("mttr", Number.POSITIVE_INFINITY)).toThrow(TypeError);
  });

  it("rejects out-of-range CFR", () => {
    expect(() => tierFor("change_failure_rate", -0.1)).toThrow(RangeError);
    expect(() => tierFor("change_failure_rate", 1.5)).toThrow(RangeError);
  });
});

describe("DORA constants", () => {
  it("DORA_TIERS lists all tiers in best-to-worst order", () => {
    expect([...DORA_TIERS]).toEqual(["elite", "high", "medium", "low"]);
  });

  it("DORA_METRICS lists all 4 metric ids", () => {
    expect([...DORA_METRICS].sort()).toEqual([
      "change_failure_rate",
      "deploy_freq",
      "lead_time",
      "mttr",
    ]);
  });
});

describe("DORA factories", () => {
  it("makeDeployFrequency rejects negative", () => {
    expect(() => makeDeployFrequency(-1)).toThrow(RangeError);
  });

  it("makeDeployFrequency accepts zero and positive", () => {
    expect(makeDeployFrequency(0)).toBe(0);
    expect(makeDeployFrequency(3.5)).toBe(3.5);
  });

  it("makeChangeFailureRate enforces [0, 1]", () => {
    expect(makeChangeFailureRate(0)).toBe(0);
    expect(makeChangeFailureRate(1)).toBe(1);
    expect(() => makeChangeFailureRate(-0.1)).toThrow(RangeError);
    expect(() => makeChangeFailureRate(1.1)).toThrow(RangeError);
  });

  it("rejects non-finite", () => {
    expect(() => makeDeployFrequency(Number.NaN)).toThrow(TypeError);
    expect(() => makeChangeFailureRate(Number.POSITIVE_INFINITY)).toThrow(TypeError);
  });
});
