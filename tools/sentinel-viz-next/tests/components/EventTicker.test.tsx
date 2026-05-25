import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { EventTicker } from "../../components/EventTicker";
import type { RecentEvent } from "../../types/api";

const sampleEvents: RecentEvent[] = [
  {
    seq: 1,
    type: "sentinel.tool_call_observed",
    ts: "2026-05-25T00:00:00Z",
    payload: { session_id: "sess-a", tool: "Bash", tool_call_id: "SentinelToolCall#tc1" },
  },
  {
    seq: 2,
    type: "sentinel.tool_call_observed",
    ts: "2026-05-25T00:00:01Z",
    payload: { session_id: "sess-a", tool: "Bash", tool_call_id: "SentinelToolCall#tc1" },
  },
  {
    seq: 3,
    type: "sentinel.hook_ingested",
    ts: "2026-05-25T00:00:02Z",
    payload: { session_id: "sess-a", hook_event: "PreToolUse", outcome: "denied" },
  },
];

describe("EventTicker", () => {
  it("renders empty state without crashing", () => {
    render(<EventTicker events={[]} onSelectNode={() => {}} />);
    expect(screen.getByTestId("event-ticker")).toBeInTheDocument();
    expect(screen.getByTestId("ticker-rows").children).toHaveLength(0);
  });

  it("groups consecutive events sharing the (session, type, tool_call_id, outcome) signature", () => {
    render(<EventTicker events={sampleEvents} onSelectNode={() => {}} />);
    // Two distinct rows: one denied hook, one (tc1×2) tool-call group.
    const rows = screen.getByTestId("ticker-rows").children;
    expect(rows).toHaveLength(2);
    expect(screen.getByText(/×2/)).toBeInTheDocument();
  });

  it("invokes onSelectNode with tool_call_id when a TC row is clicked", () => {
    const spy = vi.fn();
    render(<EventTicker events={sampleEvents} onSelectNode={spy} />);
    const rows = screen.getByTestId("ticker-rows").children;
    // The freshest row is the denied hook (no tool_call_id), so click the second one.
    fireEvent.click(rows[1]);
    expect(spy).toHaveBeenCalledWith("SentinelToolCall#tc1");
  });
});
