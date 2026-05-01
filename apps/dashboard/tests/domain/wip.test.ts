import { describe, it, expect } from "vitest";

import {
  bottleneckStage,
  emptyWipByStage,
  type StageThroughput,
  type WipSnapshot,
} from "@/domain/wip";

describe("emptyWipByStage", () => {
  it("returns zero for every stage", () => {
    const w = emptyWipByStage();
    expect(w.Backlog).toBe(0);
    expect(w["In Progress"]).toBe(0);
    expect(w.Completed).toBe(0);
  });
});

describe("bottleneckStage", () => {
  const snapshot = (
    by_team: Record<string, Partial<Record<string, number>>>,
  ): WipSnapshot => {
    const norm: WipSnapshot["by_team"] = {};
    for (const [team, stages] of Object.entries(by_team)) {
      const w = emptyWipByStage();
      for (const [s, v] of Object.entries(stages)) {
        (w as unknown as Record<string, number>)[s] = v as number;
      }
      norm[team] = w;
    }
    return { ts: new Date(), by_team: norm };
  };

  it("identifies stage with highest WIP/throughput ratio", () => {
    // QA Testing has 10 tickets at 1/day throughput = 10 WIP-days.
    // Code Review has 4 at 2/day = 2 WIP-days. QA wins.
    const s = snapshot({
      "team-a": { "Code Review": 4, "QA Testing": 10 },
    });
    const t: StageThroughput = { "Code Review": 2, "QA Testing": 1 };
    expect(bottleneckStage(s, t)).toBe("QA Testing");
  });

  it("aggregates across teams", () => {
    const s = snapshot({
      "team-a": { "Code Review": 3 },
      "team-b": { "Code Review": 5, "QA Testing": 1 },
    });
    const t: StageThroughput = { "Code Review": 1, "QA Testing": 1 };
    expect(bottleneckStage(s, t)).toBe("Code Review");
  });

  it("returns null when snapshot has no WIP", () => {
    const s = snapshot({ "team-a": {} });
    const t: StageThroughput = { "Code Review": 1 };
    expect(bottleneckStage(s, t)).toBeNull();
  });

  it("returns null when no stage has throughput data", () => {
    const s = snapshot({ "team-a": { "Code Review": 5 } });
    expect(bottleneckStage(s, {})).toBeNull();
  });

  it("breaks ties by pipeline order (earlier stage wins)", () => {
    // Both stages have 4 WIP and 1/day throughput. Earlier stage (Code Review)
    // wins because score must be strictly greater to take over.
    const s = snapshot({
      "team-a": { "Code Review": 4, "QA Testing": 4 },
    });
    const t: StageThroughput = { "Code Review": 1, "QA Testing": 1 };
    expect(bottleneckStage(s, t)).toBe("Code Review");
  });
});
