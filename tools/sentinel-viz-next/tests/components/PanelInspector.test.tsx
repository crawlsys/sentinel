import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { PanelInspector } from "../../components/PanelInspector";
import type { Node } from "../../types/api";

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
    expect(screen.getByText("SentinelSession")).toBeInTheDocument();
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
});
