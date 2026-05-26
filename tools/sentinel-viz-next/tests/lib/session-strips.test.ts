import { describe, it, expect } from "vitest";

import { buildSessionStrips, bucketsToSparkline } from "../../domain/session-strips";
import type { GraphResponse, RecentEvent } from "../../types/api";

/// Reference time well inside the past so events stamped relative
/// to it parse cleanly. All test fixtures anchor here.
const NOW = Date.parse("2026-05-26T18:00:00Z");

function tcEvent(seq: number, sid: string, tool: string, minutesAgo: number): RecentEvent {
  const t = new Date(NOW - minutesAgo * 60_000).toISOString().slice(0, 19);
  return {
    seq,
    type: "sentinel.tool_call_observed",
    ts: `${t}Z`,
    payload: {
      session_id: sid,
      sentinel_event: "PreToolUse",
      tool,
      tool_call_id: `SentinelToolCall#${seq}`,
      ts_sec: t,
    },
  };
}

function userPromptEvent(seq: number, sid: string, minutesAgo: number): RecentEvent {
  const t = new Date(NOW - minutesAgo * 60_000).toISOString().slice(0, 19);
  return {
    seq,
    type: "sentinel.tool_call_observed",
    ts: `${t}Z`,
    payload: {
      session_id: sid,
      sentinel_event: "UserPromptSubmit",
      ts_sec: t,
    },
  };
}

function graphOf(events: RecentEvent[]): GraphResponse {
  return {
    nodes: [
      {
        id: "SentinelSession#sess-a",
        type: "SentinelSession",
        data: { session_id: "sess-a" },
        ts: new Date(NOW).toISOString(),
        seq: 1,
        session_status: "firing",
        last_activity_age_s: 30,
      },
      {
        id: "SentinelSession#sess-b",
        type: "SentinelSession",
        data: { session_id: "sess-b" },
        ts: new Date(NOW).toISOString(),
        seq: 2,
        session_status: "awaiting_user",
        last_activity_age_s: 900,
      },
    ],
    edges: [],
    events,
    max_seq: events.length,
    window_limit: 100,
    stats: {
      nodes_total: 2,
      edges_total: 0,
      by_type: {},
      by_outcome: {},
      events_total: events.length,
      corpus_nodes: 2,
      corpus_edges: 0,
      corpus_by_type: {},
      corpus_by_outcome: {},
    },
  };
}

const COLOR_MAP = new Map([
  ["sess-a", "#f85149"],
  ["sess-b", "#39c5cf"],
]);

describe("buildSessionStrips", () => {
  it("buckets events per category per session over the requested window", () => {
    // 5 Bash + 2 Edit + 1 user prompt for sess-a, within the last 60m.
    const events: RecentEvent[] = [
      tcEvent(1, "sess-a", "Bash", 5),
      tcEvent(2, "sess-a", "Bash", 5),
      tcEvent(3, "sess-a", "Bash", 6),
      tcEvent(4, "sess-a", "Bash", 7),
      tcEvent(5, "sess-a", "Bash", 7),
      tcEvent(6, "sess-a", "Edit", 8),
      tcEvent(7, "sess-a", "Edit", 8),
      userPromptEvent(8, "sess-a", 10),
    ];
    const strips = buildSessionStrips(graphOf(events), {
      windowMinutes: 60,
      colors: COLOR_MAP,
      now: NOW,
    });
    expect(strips).toHaveLength(1);
    const s = strips[0];
    expect(s.sessionId).toBe("sess-a");
    expect(s.totalEvents).toBe(8);
    // Three categories should be present: tc (Bash), tc again
    // (Edit also tc) — actually Bash + Edit are both tc. So
    // tc.total === 7, prompt.total === 1.
    const tc = s.rows.find((r) => r.category === "tc");
    const prompt = s.rows.find((r) => r.category === "prompt");
    expect(tc?.total).toBe(7);
    expect(prompt?.total).toBe(1);
    // Each row's counts array length is windowMinutes.
    expect(tc?.counts.length).toBe(60);
  });

  it("drops events older than the window", () => {
    const events: RecentEvent[] = [
      tcEvent(1, "sess-a", "Bash", 5),       // in window
      tcEvent(2, "sess-a", "Bash", 70),      // outside 60m window
      tcEvent(3, "sess-a", "Bash", 200),     // also outside
    ];
    const strips = buildSessionStrips(graphOf(events), {
      windowMinutes: 60,
      colors: COLOR_MAP,
      now: NOW,
    });
    expect(strips[0].totalEvents).toBe(1);
  });

  it("skips sessions with zero events in the window", () => {
    // sess-b has no events; only sess-a has activity.
    const events: RecentEvent[] = [tcEvent(1, "sess-a", "Bash", 1)];
    const strips = buildSessionStrips(graphOf(events), {
      windowMinutes: 60,
      colors: COLOR_MAP,
      now: NOW,
    });
    expect(strips.map((s) => s.sessionId)).toEqual(["sess-a"]);
  });

  it("stuck sessions render even when they have ZERO events in the window (P3-34)", () => {
    // Stuck session = operator hasn't responded for >15min, so its
    // newest event is necessarily older than recent windows. Strip
    // MUST still appear so operator can act.
    const events: RecentEvent[] = []; // empty — no events in window
    const stuck = new Map([
      [
        "sess-stuck",
        { ageSecs: 3600, kind: "reply" as string | null, question: "are we good?" as string | null },
      ],
    ]);
    const strips = buildSessionStrips(graphOf(events), {
      windowMinutes: 60,
      colors: COLOR_MAP,
      stuck,
      now: NOW,
    });
    expect(strips).toHaveLength(1);
    expect(strips[0].sessionId).toBe("sess-stuck");
    expect(strips[0].stuck?.question).toBe("are we good?");
    expect(strips[0].rows).toEqual([]);
    expect(strips[0].totalEvents).toBe(0);
  });

  it("sorts strips with stuck sessions first, then by last_activity_age ascending", () => {
    const events: RecentEvent[] = [
      tcEvent(1, "sess-a", "Bash", 1), // 30s old, firing
      tcEvent(2, "sess-b", "Bash", 5), // older session but stuck
    ];
    const stuck = new Map([
      ["sess-b", { ageSecs: 1100, kind: "AskUserQuestion", question: "do x?" }],
    ]);
    const strips = buildSessionStrips(graphOf(events), {
      windowMinutes: 60,
      colors: COLOR_MAP,
      stuck,
      now: NOW,
    });
    // Stuck session bubbles to the top despite older last activity.
    expect(strips[0].sessionId).toBe("sess-b");
    expect(strips[0].stuck?.question).toBe("do x?");
  });

  it("orders category rows: tc, planning, communication, prompt, other", () => {
    const events: RecentEvent[] = [
      userPromptEvent(1, "sess-a", 1),
      tcEvent(2, "sess-a", "AskUserQuestion", 1), // communication
      tcEvent(3, "sess-a", "TaskUpdate", 1),       // planning
      tcEvent(4, "sess-a", "Bash", 1),             // tc
    ];
    const strips = buildSessionStrips(graphOf(events), {
      windowMinutes: 60,
      colors: COLOR_MAP,
      now: NOW,
    });
    const categories = strips[0].rows.map((r) => r.category);
    expect(categories).toEqual(["tc", "planning", "communication", "prompt"]);
  });

  it("uses the LLM name when provided, else falls back to short sid", () => {
    const events: RecentEvent[] = [tcEvent(1, "sess-a", "Bash", 1)];
    const strips = buildSessionStrips(graphOf(events), {
      windowMinutes: 60,
      colors: COLOR_MAP,
      names: new Map([["sess-a", "warm-otter"]]),
      now: NOW,
    });
    expect(strips[0].displayName).toBe("warm-otter · s:sess-a");
  });
});

describe("bucketsToSparkline", () => {
  it("returns block characters that scale with counts", () => {
    expect(bucketsToSparkline([0, 1, 2, 4, 8], 8)).toBe("·▁▂▄█");
  });

  it("treats peak=0 as all empty buckets", () => {
    expect(bucketsToSparkline([0, 0, 0], 0)).toBe("···");
  });

  it("non-zero counts NEVER render as the empty dot — they always have at least ▁", () => {
    // A bucket of 1 against a peak of 100 would round to 0 if we
    // weren't careful. Make sure any non-zero count is visible.
    const out = bucketsToSparkline([1, 1, 1, 100], 100);
    expect(out[0]).toBe("▁");
    expect(out[3]).toBe("█");
  });
});
