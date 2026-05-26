import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, screen, waitFor } from "@testing-library/react";

import { SessionConsole } from "../../components/SessionConsole";
import type { ActivityResponse, GraphResponse, Segment } from "../../types/api";

/// Mock fetchActivity so each test can stage what the API returns
/// per session and observe how the console scopes/merges results.
const fetchSpy = vi.fn();
vi.mock("../../lib/api", () => ({
  fetchActivity: (...args: unknown[]) => fetchSpy(...args),
}));

function seg(ts: string, label: string, preview: string): Segment {
  return {
    ts,
    ts_end: ts,
    kind: "assistant_turn",
    label,
    preview,
    tools: [],
    tool_count: 0,
  };
}

function activityFor(sessionId: string, segments: Segment[]): ActivityResponse {
  return {
    session_id: sessionId,
    transcript: "fake.jsonl",
    events: [],
    segments,
  };
}

function makeGraph(sids: string[]): GraphResponse {
  return {
    nodes: sids.map((sid, i) => ({
      id: `SentinelSession#${sid}`,
      type: "SentinelSession",
      data: { session_id: sid },
      ts: "2026-05-25T00:00:00Z",
      seq: i,
    })),
    edges: [],
    events: [],
    max_seq: sids.length,
    window_limit: 100,
    stats: {
      nodes_total: sids.length,
      edges_total: 0,
      by_type: {},
      by_outcome: {},
      events_total: 0,
      corpus_nodes: sids.length,
      corpus_edges: 0,
      corpus_by_type: {},
      corpus_by_outcome: {},
    },
  };
}

beforeEach(() => {
  fetchSpy.mockReset();
});

afterEach(() => {
  vi.clearAllTimers();
  vi.useRealTimers();
});

describe("SessionConsole — cross-session merged view", () => {
  it("fetches activity for every visible session and merges newest-first", async () => {
    fetchSpy.mockImplementation((sid: string) => {
      if (sid === "sess-a") {
        return Promise.resolve(
          activityFor("sess-a", [
            seg("2026-05-25T00:00:01", "turn-a-old", "first"),
            seg("2026-05-25T00:00:09", "turn-a-new", "fresh-a"),
          ]),
        );
      }
      return Promise.resolve(
        activityFor("sess-b", [seg("2026-05-25T00:00:05", "turn-b", "b mid")]),
      );
    });

    render(
      <SessionConsole
        graph={makeGraph(["sess-a", "sess-b"])}
        sessionColors={new Map()}
      />,
    );

    await waitFor(() => expect(fetchSpy).toHaveBeenCalledTimes(2));
    // Two fetches, one per visible session
    expect(fetchSpy.mock.calls.map((c) => c[0]).sort()).toEqual(["sess-a", "sess-b"]);

    await waitFor(() => expect(screen.getByText("turn-a-new")).toBeInTheDocument());
    expect(screen.getByText("turn-b")).toBeInTheDocument();

    // Header reads "all sessions" when nothing is selected.
    expect(screen.getByTestId("session-console-scope").textContent).toMatch(/all sessions/i);
  });

  it("renders empty state when no sessions are visible", async () => {
    render(<SessionConsole graph={makeGraph([])} sessionColors={new Map()} />);
    // Should NOT fetch anything when there are no sessions.
    await Promise.resolve();
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(screen.getByText(/no recent segments/i)).toBeInTheDocument();
  });
});

describe("SessionConsole — follow-selection", () => {
  it("scopes to the selected session only and ignores other sessions", async () => {
    fetchSpy.mockImplementation((sid: string) =>
      Promise.resolve(
        activityFor(sid, [seg("2026-05-25T00:00:01", `label-${sid}`, `preview-${sid}`)]),
      ),
    );

    render(
      <SessionConsole
        graph={makeGraph(["sess-a", "sess-b", "sess-c"])}
        sessionColors={new Map()}
        selectedSessionId="sess-b"
      />,
    );

    await waitFor(() => expect(fetchSpy).toHaveBeenCalledTimes(1));
    expect(fetchSpy.mock.calls[0][0]).toBe("sess-b");

    await waitFor(() => expect(screen.getByText("label-sess-b")).toBeInTheDocument());
    expect(screen.queryByText("label-sess-a")).toBeNull();
    expect(screen.queryByText("label-sess-c")).toBeNull();
  });

  it("shows a scope chip reflecting the selected session id", async () => {
    fetchSpy.mockResolvedValue(activityFor("sess-b", []));
    render(
      <SessionConsole
        graph={makeGraph(["sess-a", "sess-b"])}
        sessionColors={new Map([["sess-b", "#39c5cf"]])}
        selectedSessionId="sess-b"
      />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("session-console-scope").textContent).toMatch(/scoped/i),
    );
    expect(screen.getByTestId("session-console-scope").textContent).toMatch(/sess-b/);
  });

  it("re-fetches when selection changes from one session to another", async () => {
    fetchSpy.mockImplementation((sid: string) =>
      Promise.resolve(activityFor(sid, [seg("2026-05-25T00:00:01", `label-${sid}`, "x")])),
    );

    const { rerender } = render(
      <SessionConsole
        graph={makeGraph(["sess-a", "sess-b"])}
        sessionColors={new Map()}
        selectedSessionId="sess-a"
      />,
    );
    await waitFor(() => expect(fetchSpy).toHaveBeenCalledWith("sess-a", expect.any(Object)));
    await waitFor(() => expect(screen.getByText("label-sess-a")).toBeInTheDocument());

    rerender(
      <SessionConsole
        graph={makeGraph(["sess-a", "sess-b"])}
        sessionColors={new Map()}
        selectedSessionId="sess-b"
      />,
    );
    await waitFor(() => expect(fetchSpy).toHaveBeenCalledWith("sess-b", expect.any(Object)));
    await waitFor(() => expect(screen.getByText("label-sess-b")).toBeInTheDocument());
    // sess-a label should be gone now that we re-scoped to sess-b.
    expect(screen.queryByText("label-sess-a")).toBeNull();
  });

  it("re-fetches when selection clears back to null (merged view)", async () => {
    fetchSpy.mockImplementation((sid: string) =>
      Promise.resolve(activityFor(sid, [seg("2026-05-25T00:00:01", `label-${sid}`, "x")])),
    );

    const { rerender } = render(
      <SessionConsole
        graph={makeGraph(["sess-a", "sess-b"])}
        sessionColors={new Map()}
        selectedSessionId="sess-a"
      />,
    );
    await waitFor(() => expect(fetchSpy).toHaveBeenCalledWith("sess-a", expect.any(Object)));

    fetchSpy.mockClear();
    rerender(
      <SessionConsole
        graph={makeGraph(["sess-a", "sess-b"])}
        sessionColors={new Map()}
        selectedSessionId={null}
      />,
    );
    await waitFor(() => expect(fetchSpy.mock.calls.length).toBeGreaterThanOrEqual(2));
    const sids = fetchSpy.mock.calls.map((c) => c[0]).sort();
    expect(sids).toEqual(["sess-a", "sess-b"]);
  });

  it("requests a larger per-session segment limit when scoped to one session", async () => {
    fetchSpy.mockResolvedValue(activityFor("sess-b", []));
    render(
      <SessionConsole
        graph={makeGraph(["sess-a", "sess-b"])}
        sessionColors={new Map()}
        selectedSessionId="sess-b"
      />,
    );
    await waitFor(() => expect(fetchSpy).toHaveBeenCalledOnce());
    const opts = fetchSpy.mock.calls[0][1] as { limit?: number };
    expect(opts.limit).toBeGreaterThanOrEqual(10);
  });
});
