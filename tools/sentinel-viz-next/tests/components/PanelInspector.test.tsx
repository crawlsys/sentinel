import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { CodexEventTimeline, PanelInspector } from "../../components/PanelInspector";
import type { Node, RecentEvent } from "../../types/api";

const sessionNode: Node = {
  id: "SentinelSession#sess-a",
  type: "SentinelSession",
  data: { session_id: "sess-a", cwd: "/tmp", started_at: "2026-05-25T00:00:00Z" },
  ts: "2026-05-25T00:00:00Z",
  seq: 1,
  session_status: "awaiting_user",
  last_activity_age_s: 42,
  awaiting_kind: "reply",
  awaiting_question: "want me to merge this PR?",
};

const codexNode: Node = {
  id: "SentinelSession#cdx-1",
  type: "SentinelSession",
  data: { session_id: "cdx-1", source_harness: "codex", cwd: "/tmp", started_at: "2026-05-25T00:00:00Z" },
  ts: "2026-05-25T00:00:00Z",
  seq: 1,
  session_status: "firing",
  last_activity_age_s: 5,
};

function withClient(ui: React.ReactElement) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return <QueryClientProvider client={client}>{ui}</QueryClientProvider>;
}

describe("PanelInspector", () => {
  it("renders empty state when no node selected", () => {
    render(withClient(<PanelInspector node={null} onClose={() => {}} />));
    expect(screen.getByText(/click a node/i)).toBeInTheDocument();
  });

  it("renders session details and awaiting question", () => {
    // Avoid the real fetch — short-circuit with a fetch mock that never resolves.
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    render(withClient(<PanelInspector node={sessionNode} onClose={() => {}} />));
    expect(screen.getAllByRole("heading")[0]).toHaveTextContent("session");
    expect(screen.getByText("awaiting_user")).toBeInTheDocument();
    expect(screen.getByText(/want me to merge this PR/)).toBeInTheDocument();
    vi.unstubAllGlobals();
  });

  it("calls onClose when the close button is clicked", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    const spy = vi.fn();
    render(withClient(<PanelInspector node={sessionNode} onClose={spy} />));
    fireEvent.click(screen.getByLabelText("close inspector"));
    expect(spy).toHaveBeenCalledOnce();
    vi.unstubAllGlobals();
  });

  it("renders the codex timeline (not the transcript activity panel) for codex sessions", () => {
    const events: RecentEvent[] = [
      {
        seq: 1,
        type: "hook_invocation",
        ts: "2026-05-25T00:00:01Z",
        payload: { session_id: "cdx-1", event: "PreToolUse", tool: "Bash", outcome: "allow" },
      },
    ];
    render(withClient(
      <PanelInspector node={codexNode} events={events} onClose={() => {}} />
    ));
    // codex timeline is rendered, not the claude activity panel
    expect(screen.getByTestId("codex-event-timeline")).toBeInTheDocument();
    expect(screen.getByText("Bash")).toBeInTheDocument();
    // claude-only "ai summary" + "recent activity" headings should be absent
    expect(screen.queryByText(/recent activity|activity ± 60s/i)).toBeNull();
    expect(screen.queryByTestId("ai-summary")).toBeNull();
  });
});

describe("CodexEventTimeline", () => {
  const baseEvent = {
    seq: 1,
    type: "hook_invocation",
    ts: "2026-05-25T00:00:00Z",
  };

  it("filters events to the given session_id", () => {
    const events: RecentEvent[] = [
      { ...baseEvent, seq: 1, payload: { session_id: "cdx-1", event: "PreToolUse", tool: "Bash", outcome: "allow" } },
      { ...baseEvent, seq: 2, payload: { session_id: "other-session", event: "PreToolUse", tool: "Edit", outcome: "allow" } },
      { ...baseEvent, seq: 3, payload: { session_id: "cdx-1", event: "PreToolUse", tool: "Read", outcome: "allow" } },
    ];
    render(<CodexEventTimeline sessionId="cdx-1" harness="codex" events={events} />);
    expect(screen.getByText("Bash")).toBeInTheDocument();
    expect(screen.getByText("Read")).toBeInTheDocument();
    // Other session's Edit should be filtered out
    expect(screen.queryByText("Edit")).toBeNull();
  });

  it("shows an empty-state message when no events match", () => {
    render(<CodexEventTimeline sessionId="cdx-1" harness="codex" events={[]} />);
    expect(screen.getByText(/no events for this session/i)).toBeInTheDocument();
  });

  it("renders the event name when tool is absent (e.g. UserPromptSubmit)", () => {
    const events: RecentEvent[] = [
      { ...baseEvent, payload: { session_id: "cdx-1", event: "UserPromptSubmit" } },
    ];
    render(<CodexEventTimeline sessionId="cdx-1" harness="codex" events={events} />);
    // Renders in both the title (no tool → fall back to event) and the
    // subtitle (always the event name). Both should resolve to the same string.
    expect(screen.getAllByText("UserPromptSubmit").length).toBeGreaterThan(0);
  });
});
