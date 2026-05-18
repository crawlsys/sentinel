import { describe, it, expect } from "vitest";

import { GetTokenEconomics } from "@/application/get-token-economics";
import type { MetricsRepository, TimeRange, TokenSession } from "@/ports";

const window: TimeRange = {
  start: new Date("2026-05-01T00:00:00Z"),
  end: new Date("2026-05-31T00:00:00Z"),
};

function fakeRepo(sessions: TokenSession[]): MetricsRepository {
  return {
    readCycleTimeEvents: async () => [],
    readDeploys: async () => [],
    readTokenUsage: async () => sessions,
    readIncidents: async () => [],
  };
}

describe("GetTokenEconomics", () => {
  it("returns zero cost + 0 cache hit rate on an empty stream", async () => {
    const handler = new GetTokenEconomics(fakeRepo([]));
    const result = await handler.run(window);
    expect(result.totalCostUsd).toBe(0);
    expect(result.cacheHitRate).toBe(0);
    expect(result.byTicket).toEqual([]);
  });

  it("sums costs across sessions and computes the cache hit rate", async () => {
    const sessions: TokenSession[] = [
      {
        sessionId: "s1",
        totalInput: 100,
        cacheRead: 400,
        cacheCreation: 100,
        output: 50,
        costUsd: 2.5,
        model: "opus-4-7",
      },
      {
        sessionId: "s2",
        totalInput: 100,
        cacheRead: 200,
        cacheCreation: 100,
        output: 50,
        costUsd: 1.5,
        model: "sonnet-4-6",
      },
    ];
    const handler = new GetTokenEconomics(fakeRepo(sessions));
    const result = await handler.run(window);
    expect(result.totalCostUsd).toBe(4); // 2.5 + 1.5
    // (400 + 200) / (400 + 200 + 100 + 100 + 100 + 100) = 600 / 1000 = 0.6
    expect(result.cacheHitRate).toBeCloseTo(0.6, 6);
    expect(result.byTicket).toHaveLength(2);
  });

  it("rate is 0 when all denominators are zero", async () => {
    const sessions: TokenSession[] = [
      {
        sessionId: "s1",
        totalInput: 0,
        cacheRead: 0,
        cacheCreation: 0,
        output: 0,
        costUsd: 0,
        model: "opus-4-7",
      },
    ];
    const handler = new GetTokenEconomics(fakeRepo(sessions));
    const result = await handler.run(window);
    expect(result.cacheHitRate).toBe(0);
  });
});
