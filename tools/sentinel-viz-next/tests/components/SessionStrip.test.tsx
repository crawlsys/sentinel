import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { SessionStrip } from "../../components/SessionStrip";
import type { SessionStripData } from "../../domain/session-strips";

/// SessionStrip fires a TanStack Query for the AI summary on mount.
/// Stub fetch with a never-resolving promise so the component renders
/// its initial state without hitting the network (mirrors the
/// PanelInspector component test).
function withClient(ui: React.ReactElement) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return <QueryClientProvider client={client}>{ui}</QueryClientProvider>;
}

function strip(overrides: Partial<SessionStripData> = {}): SessionStripData {
  return {
    sessionId: "sess-a",
    displayName: "warm-otter · s:sess-a",
    activityBlurb: null,
    shortSid: "sess-a",
    color: "#f85149",
    status: "firing",
    sourceHarness: "claude",
    lastActivityAgeS: 30,
    stuck: null,
    rows: [],
    totalEvents: 0,
    peakPerMin: 0,
    ...overrides,
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("SessionStrip header status badge", () => {
  it("renders the friendly status word, not the raw backend enum", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    render(withClient(<SessionStrip data={strip({ status: "awaiting_user" })} selected={false} onSelect={() => {}} />));
    // Operator-facing word is shown…
    expect(screen.getByText("waiting")).toBeInTheDocument();
    // …and the underscored machine enum never reaches the screen text.
    expect(screen.queryByText("awaiting_user")).toBeNull();
  });

  it("translates the sentinel-internal 'firing' enum to 'active'", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    render(withClient(<SessionStrip data={strip({ status: "firing" })} selected={false} onSelect={() => {}} />));
    expect(screen.getByText("active")).toBeInTheDocument();
    expect(screen.queryByText("firing")).toBeNull();
  });

  it("keeps the raw enum discoverable as the badge's title (hover) for power users", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    render(withClient(<SessionStrip data={strip({ status: "awaiting_user" })} selected={false} onSelect={() => {}} />));
    const badge = screen.getByText("waiting");
    expect(badge).toHaveAttribute("title", "awaiting_user");
  });

  it("renders a dash when status is null instead of an empty label", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    render(withClient(<SessionStrip data={strip({ status: null })} selected={false} onSelect={() => {}} />));
    expect(screen.getByText("—")).toBeInTheDocument();
  });
});

describe("SessionStrip header peak-rate summary", () => {
  it("shows a /min intensity token for bursty sessions so they stand out when scanning", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    render(
      withClient(
        <SessionStrip
          data={strip({ totalEvents: 40, peakPerMin: 15 })}
          selected={false}
          onSelect={() => {}}
        />,
      ),
    );
    const rate = screen.getByTestId("session-strip-peak-rate");
    expect(rate.textContent).toContain("15/min");
    expect(rate).toHaveAttribute("title", "peak events/min in window");
  });

  it("omits the token entirely for trickle sessions (peak <= 1) — no '· /min' noise", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    render(
      withClient(
        <SessionStrip
          data={strip({ totalEvents: 2, peakPerMin: 1 })}
          selected={false}
          onSelect={() => {}}
        />,
      ),
    );
    expect(screen.queryByTestId("session-strip-peak-rate")).toBeNull();
    // The event count is still shown — only the peak suffix is gated.
    expect(screen.getByText(/2 ev/)).toBeInTheDocument();
  });

  it("omits the token for stuck/zero-activity strips (peak 0)", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    render(
      withClient(
        <SessionStrip
          data={strip({
            status: "awaiting_user",
            totalEvents: 0,
            peakPerMin: 0,
            stuck: { ageSecs: 3420, kind: "AskUserQuestion", question: "confirm idle?" },
          })}
          selected={false}
          onSelect={() => {}}
        />,
      ),
    );
    expect(screen.queryByTestId("session-strip-peak-rate")).toBeNull();
  });
});

describe("SessionStrip stuck-banner reason kind", () => {
  function stuckStrip(kind: string | null) {
    return strip({
      status: "awaiting_user",
      totalEvents: 0,
      peakPerMin: 0,
      stuck: { ageSecs: 3420, kind, question: "confirm idle?" },
    });
  }

  it("translates the raw awaiting_kind to operator phrasing in the red STUCK banner", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    render(withClient(<SessionStrip data={stuckStrip("AskUserQuestion")} selected={false} onSelect={() => {}} />));
    const kindEl = screen.getByTestId("session-strip-stuck-kind");
    // Operator-facing phrase, not the camel-case tool identifier.
    expect(kindEl.textContent).toBe("your answer");
    expect(screen.queryByText("AskUserQuestion")).toBeNull();
  });

  it("keeps the raw kind discoverable on hover (title) for power users", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    render(withClient(<SessionStrip data={stuckStrip("PreToolUse")} selected={false} onSelect={() => {}} />));
    const kindEl = screen.getByTestId("session-strip-stuck-kind");
    expect(kindEl.textContent).toBe("tool approval");
    expect(kindEl).toHaveAttribute("title", "PreToolUse");
  });

  it("falls back to 'awaiting' when the kind is null instead of an empty token", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    render(withClient(<SessionStrip data={stuckStrip(null)} selected={false} onSelect={() => {}} />));
    expect(screen.getByTestId("session-strip-stuck-kind").textContent).toBe("awaiting");
  });
});
