import { describe, it, expect } from "vitest";

import {
  HAIKU_RATES,
  OPUS_RATES,
  RATES_BY_MODEL,
  SONNET_RATES,
  cost,
  makeDollars,
  makeTokens,
  shortModelLabel,
  tierForModel,
  type TokenUsage,
} from "@/domain/dollars";

const ZERO_USAGE: TokenUsage = {
  input: 0,
  output: 0,
  cache_read: 0,
  cache_creation_5m: 0,
  cache_creation_1h: 0,
};

describe("rates parity with Rust pricing.rs", () => {
  it("opus rates match", () => {
    expect(OPUS_RATES.input).toBe(15);
    expect(OPUS_RATES.output).toBe(75);
    expect(OPUS_RATES.cache_read).toBe(1.5);
    expect(OPUS_RATES.cache_creation_5m).toBe(3.75);
    expect(OPUS_RATES.cache_creation_1h).toBe(18.75);
  });

  it("sonnet rates match", () => {
    expect(SONNET_RATES.input).toBe(3);
    expect(SONNET_RATES.output).toBe(15);
    expect(SONNET_RATES.cache_read).toBe(0.3);
    expect(SONNET_RATES.cache_creation_5m).toBe(0.75);
    expect(SONNET_RATES.cache_creation_1h).toBe(3.75);
  });

  it("haiku rates match", () => {
    expect(HAIKU_RATES.input).toBe(0.8);
    expect(HAIKU_RATES.output).toBe(4);
    expect(HAIKU_RATES.cache_read).toBe(0.08);
    expect(HAIKU_RATES.cache_creation_5m).toBe(0.2);
    expect(HAIKU_RATES.cache_creation_1h).toBe(1);
  });

  it("RATES_BY_MODEL covers all five model labels", () => {
    expect(Object.keys(RATES_BY_MODEL).sort()).toEqual([
      "haiku-4-5",
      "opus-4-6",
      "opus-4-7",
      "sonnet-4-5",
      "sonnet-4-6",
    ]);
  });
});

describe("tierForModel", () => {
  it("classifies known models", () => {
    expect(tierForModel("claude-opus-4-6")).toBe("opus");
    expect(tierForModel("claude-opus-4-7")).toBe("opus");
    expect(tierForModel("claude-sonnet-4-5")).toBe("sonnet");
    expect(tierForModel("claude-sonnet-4-6")).toBe("sonnet");
    expect(tierForModel("claude-haiku-4-5")).toBe("haiku");
  });

  it("falls back to opus for unknown models", () => {
    expect(tierForModel("totally-made-up-model")).toBe("opus");
    expect(tierForModel("")).toBe("opus");
    expect(tierForModel("gpt-5")).toBe("opus");
  });

  it("is case-insensitive", () => {
    expect(tierForModel("CLAUDE-SONNET-4-5")).toBe("sonnet");
    expect(tierForModel("Claude-Haiku-4-5")).toBe("haiku");
  });
});

describe("cost — published opus rates", () => {
  it("1M input on opus = $15", () => {
    const usage: TokenUsage = { ...ZERO_USAGE, input: 1_000_000 };
    expect(cost(usage, "claude-opus-4-7")).toBeCloseTo(15, 3);
  });

  it("1M output on opus = $75", () => {
    const usage: TokenUsage = { ...ZERO_USAGE, output: 1_000_000 };
    expect(cost(usage, "claude-opus-4-7")).toBeCloseTo(75, 3);
  });

  it("1M cache_read on opus = $1.50", () => {
    const usage: TokenUsage = { ...ZERO_USAGE, cache_read: 1_000_000 };
    expect(cost(usage, "claude-opus-4-7")).toBeCloseTo(1.5, 3);
  });

  it("1M cache_creation_5m on opus = $3.75", () => {
    const usage: TokenUsage = { ...ZERO_USAGE, cache_creation_5m: 1_000_000 };
    expect(cost(usage, "claude-opus-4-7")).toBeCloseTo(3.75, 3);
  });

  it("1M cache_creation_1h on opus = $18.75", () => {
    const usage: TokenUsage = { ...ZERO_USAGE, cache_creation_1h: 1_000_000 };
    expect(cost(usage, "claude-opus-4-7")).toBeCloseTo(18.75, 3);
  });

  it("matches the mixed-block fixture from Rust pricing.rs", () => {
    // 124800 input + 18420 output + 3920000 cache_read on opus
    // = 124800/1e6*15 + 18420/1e6*75 + 3920000/1e6*1.5
    // = 1.872 + 1.3815 + 5.88 = 9.1335
    const usage: TokenUsage = {
      input: 124_800,
      output: 18_420,
      cache_read: 3_920_000,
      cache_creation_5m: 0,
      cache_creation_1h: 0,
    };
    expect(cost(usage, "claude-opus-4-7")).toBeCloseTo(9.1335, 3);
  });
});

describe("cost — published sonnet rates", () => {
  it("1M input on sonnet = $3", () => {
    const usage: TokenUsage = { ...ZERO_USAGE, input: 1_000_000 };
    expect(cost(usage, "claude-sonnet-4-5")).toBeCloseTo(3, 3);
  });

  it("1M output on sonnet = $15", () => {
    const usage: TokenUsage = { ...ZERO_USAGE, output: 1_000_000 };
    expect(cost(usage, "claude-sonnet-4-5")).toBeCloseTo(15, 3);
  });
});

describe("cost — published haiku rates", () => {
  it("1M input on haiku = $0.80", () => {
    const usage: TokenUsage = { ...ZERO_USAGE, input: 1_000_000 };
    expect(cost(usage, "claude-haiku-4-5")).toBeCloseTo(0.8, 3);
  });

  it("1M output on haiku = $4", () => {
    const usage: TokenUsage = { ...ZERO_USAGE, output: 1_000_000 };
    expect(cost(usage, "claude-haiku-4-5")).toBeCloseTo(4, 3);
  });
});

describe("cost — unknown models fall back to opus", () => {
  it("uses opus rates for unrecognised model id", () => {
    const usage: TokenUsage = { ...ZERO_USAGE, input: 1_000_000 };
    expect(cost(usage, "totally-fake-model")).toBeCloseTo(15, 3);
  });
});

describe("makeDollars / makeTokens", () => {
  it("makeDollars accepts negative (credits)", () => {
    expect(makeDollars(-1.23)).toBe(-1.23);
  });

  it("makeDollars rejects non-finite", () => {
    expect(() => makeDollars(Number.NaN)).toThrow(TypeError);
  });

  it("makeTokens rejects negative", () => {
    expect(() => makeTokens(-1)).toThrow(RangeError);
  });

  it("makeTokens rejects non-finite", () => {
    expect(() => makeTokens(Number.POSITIVE_INFINITY)).toThrow(TypeError);
  });
});

describe("shortModelLabel", () => {
  it("strips claude- prefix", () => {
    expect(shortModelLabel("claude-opus-4-7")).toBe("opus-4-7");
    expect(shortModelLabel("claude-sonnet-4-5")).toBe("sonnet-4-5");
  });

  it("strips bracketed suffix like [1m]", () => {
    expect(shortModelLabel("claude-opus-4-7[1m]")).toBe("opus-4-7");
  });

  it("falls back when no claude- prefix", () => {
    expect(shortModelLabel("custom-model")).toBe("custom-model");
  });
});
