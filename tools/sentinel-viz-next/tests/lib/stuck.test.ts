import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  STUCK_ALERT_COUNT,
  STUCK_ALERT_SUPPRESS_MS,
  STUCK_THRESHOLD_SECS,
  _resetStuckAlertState,
  isStuck,
  maybeFireStuckAlert,
  stuckSessions,
} from "../../lib/stuck";
import type { GraphResponse, Node } from "../../types/api";

function sessionNode(id: string, status: string, age: number, q = "still here?"): Node {
  return {
    id,
    type: "SentinelSession",
    data: { session_id: id.replace("SentinelSession#", "") },
    ts: "2026-05-25T00:00:00Z",
    seq: 1,
    session_status: status as Node["session_status"],
    last_activity_age_s: age,
    awaiting_question: q,
  };
}

function graph(nodes: Node[]): GraphResponse {
  return {
    nodes,
    edges: [],
    events: [],
    max_seq: 1,
    window_limit: 100,
    stats: {
      nodes_total: nodes.length,
      edges_total: 0,
      by_type: {},
      by_outcome: {},
      events_total: 0,
      corpus_nodes: nodes.length,
      corpus_edges: 0,
      corpus_by_type: {},
      corpus_by_outcome: {},
    },
  };
}

describe("isStuck", () => {
  it("returns true for awaiting_user older than threshold", () => {
    expect(isStuck(sessionNode("a", "awaiting_user", STUCK_THRESHOLD_SECS + 1))).toBe(true);
  });

  it("returns false for awaiting_user under threshold", () => {
    expect(isStuck(sessionNode("a", "awaiting_user", STUCK_THRESHOLD_SECS - 1))).toBe(false);
  });

  it("returns false for non-awaiting statuses regardless of age", () => {
    for (const s of ["firing", "busy", "idle", "dormant", "dead"]) {
      expect(isStuck(sessionNode("a", s, 99999))).toBe(false);
    }
  });

  it("returns false for non-session nodes", () => {
    const n = sessionNode("a", "awaiting_user", 99999);
    n.type = "SentinelToolCall";
    expect(isStuck(n)).toBe(false);
  });
});

describe("stuckSessions", () => {
  it("returns only the stuck nodes from the graph", () => {
    const g = graph([
      sessionNode("a", "awaiting_user", STUCK_THRESHOLD_SECS + 5),
      sessionNode("b", "awaiting_user", STUCK_THRESHOLD_SECS - 5),
      sessionNode("c", "firing", 99999),
    ]);
    const s = stuckSessions(g);
    expect(s).toHaveLength(1);
    expect(s[0].id).toBe("a");
  });

  it("returns empty array on null graph", () => {
    expect(stuckSessions(null)).toEqual([]);
  });
});

describe("maybeFireStuckAlert", () => {
  const granted = { permission: "granted" } as const;

  beforeEach(() => {
    _resetStuckAlertState();
    const NotificationMock = vi.fn() as unknown as typeof Notification;
    (NotificationMock as unknown as Record<string, unknown>).permission = "granted";
    (NotificationMock as unknown as Record<string, unknown>).requestPermission = vi.fn(() => Promise.resolve(granted.permission));
    vi.stubGlobal("Notification", NotificationMock);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("does not fire when stuck count is below threshold", () => {
    const stuck = [sessionNode("a", "awaiting_user", STUCK_THRESHOLD_SECS + 1)];
    maybeFireStuckAlert(1, stuck);
    expect(globalThis.Notification).not.toHaveBeenCalled();
  });

  it("fires when crossing threshold from below", () => {
    maybeFireStuckAlert(STUCK_ALERT_COUNT - 1, []);
    maybeFireStuckAlert(STUCK_ALERT_COUNT, [
      sessionNode("a", "awaiting_user", STUCK_THRESHOLD_SECS + 1),
      sessionNode("b", "awaiting_user", STUCK_THRESHOLD_SECS + 1),
      sessionNode("c", "awaiting_user", STUCK_THRESHOLD_SECS + 1),
    ]);
    expect(globalThis.Notification).toHaveBeenCalledOnce();
  });

  it("does not fire twice when count stays above threshold", () => {
    maybeFireStuckAlert(STUCK_ALERT_COUNT - 1, []);
    maybeFireStuckAlert(STUCK_ALERT_COUNT, [sessionNode("a", "awaiting_user", STUCK_THRESHOLD_SECS + 1)]);
    maybeFireStuckAlert(STUCK_ALERT_COUNT + 1, [sessionNode("a", "awaiting_user", STUCK_THRESHOLD_SECS + 1)]);
    expect(globalThis.Notification).toHaveBeenCalledOnce();
  });

  it("suppresses re-fire within STUCK_ALERT_SUPPRESS_MS even after dipping", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-05-25T00:00:00Z"));
    maybeFireStuckAlert(STUCK_ALERT_COUNT - 1, []);
    maybeFireStuckAlert(STUCK_ALERT_COUNT, [sessionNode("a", "awaiting_user", 9999)]);
    expect(globalThis.Notification).toHaveBeenCalledOnce();
    // Drop below then cross again WITHIN the suppression window.
    maybeFireStuckAlert(0, []);
    vi.advanceTimersByTime(STUCK_ALERT_SUPPRESS_MS - 1000);
    maybeFireStuckAlert(STUCK_ALERT_COUNT, [sessionNode("a", "awaiting_user", 9999)]);
    expect(globalThis.Notification).toHaveBeenCalledOnce();
    // Past the suppression window, fires again.
    vi.advanceTimersByTime(2000);
    maybeFireStuckAlert(0, []);
    maybeFireStuckAlert(STUCK_ALERT_COUNT, [sessionNode("a", "awaiting_user", 9999)]);
    expect(globalThis.Notification).toHaveBeenCalledTimes(2);
  });
});
