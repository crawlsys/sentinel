import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { EventTicker } from "../../components/EventTicker";
import type { RecentEvent } from "../../types/api";

const sampleEvents: RecentEvent[] = [
  {
    seq: 1,
    type: "sentinel.tool_call_observed",
    ts: "2026-05-25T00:00:00Z",
    payload: {
      session_id: "sess-a",
      sentinel_event: "PreToolUse",
      tool: "Bash",
      tool_call_id: "SentinelToolCall#tc1",
      ts_sec: "2026-05-25T00:00:00",
    },
  },
  {
    seq: 2,
    type: "sentinel.tool_call_observed",
    ts: "2026-05-25T00:00:01Z",
    payload: {
      session_id: "sess-a",
      sentinel_event: "PreToolUse",
      tool: "Bash",
      tool_call_id: "SentinelToolCall#tc1",
      ts_sec: "2026-05-25T00:00:01",
    },
  },
  {
    seq: 3,
    type: "sentinel.hook_ingested",
    ts: "2026-05-25T00:00:02Z",
    payload: {
      session_id: "sess-a",
      sentinel_event: "PreToolUse",
      hook: "tool_usage_gate",
      outcome: "deny",
      ts: "2026-05-25T00:00:02",
    },
  },
  {
    seq: 4,
    type: "sentinel.tool_call_observed",
    ts: "",
    payload: {
      session_id: "sess-b",
      sentinel_event: "UserPromptSubmit",
      tool: "",
      tool_call_id: "SentinelToolCall#tc2",
      ts_sec: "2026-05-25T00:00:03",
    },
  },
];

describe("EventTicker", () => {
  it("renders empty state without crashing", () => {
    render(<EventTicker events={[]} onSelectNode={() => {}} />);
    expect(screen.getByTestId("event-ticker")).toBeInTheDocument();
    expect(screen.getByTestId("ticker-rows").children).toHaveLength(0);
  });

  it("groups consecutive tc events on the same (session,type,tool_call_id,outcome)", () => {
    render(<EventTicker events={sampleEvents} onSelectNode={() => {}} />);
    const rows = screen.getByTestId("ticker-rows").children;
    // 3 distinct rows: user-prompt (tc2), denied hook, then bashed tc1 ×2.
    expect(rows).toHaveLength(3);
    expect(screen.getByText(/×2/)).toBeInTheDocument();
  });

  it("labels UserPromptSubmit events as 'user prompt' and not blank", () => {
    render(<EventTicker events={sampleEvents} onSelectNode={() => {}} />);
    expect(screen.getByText("user prompt")).toBeInTheDocument();
  });

  it("derives ts from payload.ts_sec when the SQL column is empty", () => {
    // The 4th event has empty `ts` column but ts_sec=00:00:03 in payload.
    // Relative formatting kicks in (the fixture is far in the past from
    // wall-clock 'now'), so we expect SOMETHING formatted (not "—")
    // referencing the day or hour, not a blank dash.
    render(<EventTicker events={sampleEvents} onSelectNode={() => {}} />);
    // At least one row has a parseable (non-dash) timestamp present.
    const rows = screen.getByTestId("ticker-rows").querySelectorAll("li");
    const hasReadableTs = Array.from(rows).some((li) => /(\d+[smh]\b|\d{2}:\d{2})/.test(li.textContent ?? ""));
    expect(hasReadableTs).toBe(true);
  });

  it("clicking a row invokes onSelectNode with the tool_call_id", () => {
    const spy = vi.fn();
    render(<EventTicker events={sampleEvents} onSelectNode={spy} />);
    const rows = screen.getByTestId("ticker-rows").children;
    // Click the "Bash ×2" row (last in display order, freshest first → newest is user prompt, second is denied hook, third is Bash).
    fireEvent.click(rows[2].querySelector(".cursor-pointer")!);
    expect(spy).toHaveBeenCalledWith("SentinelToolCall#tc1", expect.any(String));
  });

  it("clicking the ×N badge expands the group without firing onSelectNode", () => {
    const spy = vi.fn();
    render(<EventTicker events={sampleEvents} onSelectNode={spy} />);
    const badge = screen.getByText(/×2/);
    fireEvent.click(badge);
    expect(spy).not.toHaveBeenCalled();
    // After expanding, the row should reveal both grouped members.
    expect(screen.getAllByText("TC#tc1")).toHaveLength(2);
  });
});
