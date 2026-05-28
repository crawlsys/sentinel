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
    expect(screen.getByText(/want me to merge this PR/)).toBeInTheDocument();
    vi.unstubAllGlobals();
  });

  it("translates the raw session_status enum in the status row, keeping the enum on hover", () => {
    // The status row used to print the backend enum verbatim
    // ("awaiting_user") — the same jargon leak the strip header and
    // codex timeline already translate. Hold this row to the same bar:
    // friendly word on screen, raw enum discoverable via title only.
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    render(withClient(<PanelInspector node={sessionNode} onClose={() => {}} />));
    const status = screen.getByText("waiting");
    expect(status).toHaveAttribute("title", "awaiting_user");
    // The underscored machine enum must not reach the screen as text.
    // (The "awaiting user · reply" callout below is separate copy and
    // intentionally uses the kind, not the status enum.)
    expect(screen.queryByText("awaiting_user")).toBeNull();
    vi.unstubAllGlobals();
  });

  it("translates the sentinel-internal 'firing' status to 'active' for a codex node", () => {
    // Second synthetic shape: a non-claude node whose status row used
    // to read "firing" (sentinel-internal jargon). Same translation
    // path; raw enum stays on hover.
    render(withClient(<PanelInspector node={codexNode} events={[]} onClose={() => {}} />));
    const status = screen.getByText("active");
    expect(status).toHaveAttribute("title", "firing");
    expect(screen.queryByText("firing")).toBeNull();
  });

  it("passes an unknown future status through unchanged (never swallows a novel enum)", () => {
    // Third shape: a status the label table doesn't know. The strip
    // header and statusLabel() both fall through unchanged here, so the
    // inspector must too — hiding a novel backend status behind a dash
    // would be worse than showing the raw word.
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    const futureNode: Node = { ...sessionNode, session_status: "rate_limited" };
    render(withClient(<PanelInspector node={futureNode} onClose={() => {}} />));
    const status = screen.getByText("rate_limited");
    expect(status).toHaveAttribute("title", "rate_limited");
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

  it("falls back to graph events when claude transcript segments are empty", async () => {
    vi.stubGlobal("fetch", vi.fn((input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/api/activity/")) {
        return Promise.resolve(new Response(JSON.stringify({
          session_id: "sess-a",
          transcript: null,
          events: [],
          segments: [],
        }), { status: 200 }));
      }
      if (url.includes("/api/summary/")) {
        return Promise.resolve(new Response(JSON.stringify({
          session_id: "sess-a",
          kind: "card",
          at_ts: null,
          text: null,
          source: "disabled",
          cached: false,
        }), { status: 200 }));
      }
      return Promise.resolve(new Response("{}", { status: 200 }));
    }));
    const events: RecentEvent[] = [
      {
        seq: 10,
        type: "sentinel.hook_ingested",
        ts: "2026-05-25T00:00:10Z",
        payload: {
          session_id: "sess-a",
          sentinel_event: "PreToolUse",
          hook: "mcp_health",
          tool: "Bash",
          outcome: "allow",
        },
      },
    ];
    render(withClient(<PanelInspector node={sessionNode} events={events} onClose={() => {}} />));

    expect(await screen.findByTestId("activity-segments")).toBeInTheDocument();
    expect(screen.getByText("Bash")).toBeInTheDocument();
    vi.unstubAllGlobals();
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

  it("renders the friendly event name when tool is absent (e.g. UserPromptSubmit)", () => {
    const events: RecentEvent[] = [
      { ...baseEvent, payload: { session_id: "cdx-1", event: "UserPromptSubmit" } },
    ];
    render(<CodexEventTimeline sessionId="cdx-1" harness="codex" events={events} />);
    expect(screen.getByText("user prompt")).toBeInTheDocument();
    // The raw lifecycle name must NOT leak as the detail sub-line —
    // "user prompt" already conveys it; "UserPromptSubmit" is jargon.
    expect(screen.queryByText("UserPromptSubmit")).toBeNull();
  });

  it("translates bare lifecycle events (no tool, no hook) to operator phrasing instead of raw names", () => {
    // Codex sessions surface lifecycle events that carry neither a
    // tool nor a hook (Notification, PreCompact, …). The Claude
    // ticker already routes these through sentinelEventPhrase(); the
    // codex timeline used to leak the raw `Notification` /
    // `PreCompact` name. Operator-facing copy should match the ticker.
    const events: RecentEvent[] = [
      { ...baseEvent, seq: 1, payload: { session_id: "cdx-1", sentinel_event: "Notification" } },
      { ...baseEvent, seq: 2, payload: { session_id: "cdx-1", sentinel_event: "PreCompact" } },
    ];
    render(<CodexEventTimeline sessionId="cdx-1" harness="codex" events={events} />);
    expect(screen.getByText("notified")).toBeInTheDocument();
    expect(screen.getByText("compacting")).toBeInTheDocument();
    // Raw lifecycle jargon must NOT appear as a row label.
    expect(screen.queryByText("Notification")).toBeNull();
    expect(screen.queryByText("PreCompact")).toBeNull();
  });

  it("collapses duplicate routine rows and hides low-signal codex result noise", () => {
    const events: RecentEvent[] = [
      { ...baseEvent, seq: 1, payload: { session_id: "cdx-1", sentinel_event: "PostToolUse", hook: "codex_shim_tool_result", outcome: "allow" } },
      { ...baseEvent, seq: 2, payload: { session_id: "cdx-1", sentinel_event: "PreToolUse", hook: "codex_shim_tool_exec_command", tool: "Bash", outcome: "allow" } },
      { ...baseEvent, seq: 3, payload: { session_id: "cdx-1", sentinel_event: "PreToolUse", hook: "codex_shim_tool_exec_command", tool: "Bash", outcome: "allow" } },
    ];
    render(<CodexEventTimeline sessionId="cdx-1" harness="codex" events={events} />);
    expect(screen.getByText("×2 Bash")).toBeInTheDocument();
    expect(screen.queryByText(/codex_shim_tool_result/)).toBeNull();
  });
});
