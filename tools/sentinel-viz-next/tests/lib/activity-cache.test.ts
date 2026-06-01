import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  _cacheSize,
  _resetActivityCache,
  indexActivity,
  lookup,
  subscribe,
} from "../../lib/activity-cache";
import type { ActivityResponse } from "../../types/api";

function activityFor(sid: string, segments: Array<{ts: string; tools: string[]; summary: string}>): ActivityResponse {
  return {
    session_id: sid,
    transcript: `/tmp/${sid}.jsonl`,
    events: [],
    segments: segments.map((s, i) => ({
      ts: s.ts,
      kind: "assistant_turn" as const,
      label: s.tools.join(", "),
      preview: s.summary,
      tools: s.tools,
      tool_calls: s.tools.map((t, j) => ({ id: `tu_${i}_${j}`, tool: t, summary: s.summary })),
      tool_count: s.tools.length,
    })),
    total: segments.length,
    total_segments: segments.length,
  };
}

beforeEach(() => {
  _resetActivityCache();
});

describe("activity-cache", () => {
  it("indexes tool_calls by (session, tool, minute-bucket)", () => {
    const a = activityFor("sess-a", [
      { ts: "2026-05-25T13:25:43.100Z", tools: ["Bash"], summary: "ls -la /tmp" },
    ]);
    indexActivity("sess-a", a);
    expect(_cacheSize()).toBe(1);
    expect(lookup("sess-a", "Bash", "2026-05-25T13:25:43")?.summary).toBe("ls -la /tmp");
    // Minute-bucket match: 30 seconds later in the same minute still hits.
    expect(lookup("sess-a", "Bash", "2026-05-25T13:25:00")?.summary).toBe("ls -la /tmp");
    // Different minute → miss.
    expect(lookup("sess-a", "Bash", "2026-05-25T13:26:00")).toBeNull();
  });

  it("supports many tool_calls per segment", () => {
    const a = activityFor("sess-b", [
      { ts: "2026-05-25T13:25:00Z", tools: ["Bash", "Read"], summary: "x" },
    ]);
    indexActivity("sess-b", a);
    expect(lookup("sess-b", "Bash", "2026-05-25T13:25")).not.toBeNull();
    expect(lookup("sess-b", "Read", "2026-05-25T13:25")).not.toBeNull();
  });

  it("ignores empty input", () => {
    indexActivity("sess-c", undefined);
    indexActivity("sess-c", { segments: [] } as unknown as ActivityResponse);
    expect(_cacheSize()).toBe(0);
  });

  it("notifies subscribers on new entries, not on no-op indexes", () => {
    const fn = vi.fn();
    const off = subscribe(fn);
    indexActivity("sess-d", activityFor("sess-d", [
      { ts: "2026-05-25T13:25:00Z", tools: ["Bash"], summary: "x" },
    ]));
    expect(fn).toHaveBeenCalledOnce();
    // Re-index same segment → no notification.
    indexActivity("sess-d", activityFor("sess-d", [
      { ts: "2026-05-25T13:25:00Z", tools: ["Bash"], summary: "x" },
    ]));
    expect(fn).toHaveBeenCalledOnce();
    off();
  });

  it("lookup returns null on missing fields", () => {
    expect(lookup(null, "Bash", "2026-05-25T13:25")).toBeNull();
    expect(lookup("sess-a", null, "2026-05-25T13:25")).toBeNull();
    expect(lookup("sess-a", "Bash", null)).toBeNull();
  });
});
