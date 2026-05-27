import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { EventTicker } from "../../components/EventTicker";
import type { RecentEvent } from "../../types/api";

// Freeze wall-clock just after the sample fixtures' timestamps so
// the pin-TTL gate (PIN_TTL_MS = 30 min) treats them as fresh. The
// real `Date.now()` would put these 1+ days in the past and the
// TTL would correctly drop them — fine for production, wrong for
// tests of pin behaviour that assume the events are "current".
const FROZEN_NOW = new Date("2026-05-26T00:01:00Z");
beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: false });
  vi.setSystemTime(FROZEN_NOW);
});
afterEach(() => {
  vi.useRealTimers();
});

const sampleEvents: RecentEvent[] = [
  {
    seq: 1,
    type: "sentinel.tool_call_observed",
    ts: "2026-05-26T00:00:00Z",
    payload: {
      session_id: "sess-a",
      sentinel_event: "PreToolUse",
      tool: "Bash",
      tool_call_id: "SentinelToolCall#tc1",
      ts_sec: "2026-05-26T00:00:00",
    },
  },
  {
    seq: 2,
    type: "sentinel.tool_call_observed",
    ts: "2026-05-26T00:00:01Z",
    payload: {
      session_id: "sess-a",
      sentinel_event: "PreToolUse",
      tool: "Bash",
      tool_call_id: "SentinelToolCall#tc1",
      ts_sec: "2026-05-26T00:00:01",
    },
  },
  {
    seq: 3,
    type: "sentinel.hook_ingested",
    ts: "2026-05-26T00:00:02Z",
    payload: {
      session_id: "sess-a",
      sentinel_event: "PreToolUse",
      hook: "tool_usage_gate",
      outcome: "deny",
      ts: "2026-05-26T00:00:02",
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
      ts_sec: "2026-05-26T00:00:03",
    },
  },
];

describe("EventTicker — sub-line operator phrasing (P3-20)", () => {
  it("rows show operator-facing phrasing instead of raw lifecycle event names", () => {
    render(<EventTicker events={sampleEvents} onSelectNode={() => {}} />);
    const rendered = screen.getByTestId("ticker-rows").textContent ?? "";
    // No raw lifecycle jargon should appear in the rendered sub-line.
    expect(rendered).not.toMatch(/PreToolUse/);
    expect(rendered).not.toMatch(/UserPromptSubmit/);
    // Friendly phrases should be present instead.
    expect(rendered).toMatch(/about to run|finished|you submitted/i);
  });

  it("no longer prefixes the sub-line with a redundant 's:<sid>…' (color-tab carries that)", () => {
    render(<EventTicker events={sampleEvents} onSelectNode={() => {}} />);
    const subLines = Array.from(
      screen.getByTestId("ticker-rows").querySelectorAll("li"),
    )
      .map((li) => li.querySelector("div.text-\\[10px\\].text-\\[\\#999\\].truncate.pl-4"))
      .filter(Boolean) as HTMLElement[];
    expect(subLines.length).toBeGreaterThan(0);
    for (const sub of subLines) {
      expect(sub.textContent ?? "").not.toMatch(/^\s*s:/);
    }
  });
});

describe("EventTicker — actor distinction (P3-19)", () => {
  it("every ticker row carries a data-actor attribute", () => {
    render(<EventTicker events={sampleEvents} onSelectNode={() => {}} />);
    // Scope to top-level row `<li>` only. Nested <li> exist inside
    // RolledPreview / flyout sub-lists and don't carry data-actor.
    const rows = screen.getByTestId("ticker-rows").querySelectorAll("li[data-actor]");
    expect(rows.length).toBeGreaterThan(0);
    for (const li of Array.from(rows)) {
      const actor = li.getAttribute("data-actor");
      expect(["claude", "sentinel", "user"]).toContain(actor);
    }
  });

  it("UserPromptSubmit rows carry data-actor=user with the ↩ glyph", () => {
    render(<EventTicker events={sampleEvents} onSelectNode={() => {}} />);
    const userRow = Array.from(screen.getByTestId("ticker-rows").querySelectorAll("li")).find(
      (li) => li.getAttribute("data-actor") === "user",
    );
    expect(userRow).toBeDefined();
    expect(userRow?.textContent).toContain("↩");
  });

  it("tool-call rows default to data-actor=claude with the ◇ glyph", () => {
    render(<EventTicker events={sampleEvents} onSelectNode={() => {}} />);
    const claudeRow = Array.from(screen.getByTestId("ticker-rows").querySelectorAll("li")).find(
      (li) => li.getAttribute("data-actor") === "claude",
    );
    expect(claudeRow).toBeDefined();
    expect(claudeRow?.textContent).toContain("◇");
  });

  it("legend chip is present and lists all three actor labels", () => {
    render(<EventTicker events={sampleEvents} onSelectNode={() => {}} />);
    const legend = screen.getByTestId("actor-legend");
    expect(legend.textContent).toMatch(/agent/i);
    expect(legend.textContent).toMatch(/sentinel/i);
    expect(legend.textContent).toMatch(/user/i);
  });

  it("a deny hook row renders data-actor=sentinel and data-intervention=true", () => {
    const denyEvent: RecentEvent[] = [
      {
        seq: 1,
        type: "sentinel.hook_ingested",
        ts: "2026-05-26T00:00:00Z",
        payload: {
          session_id: "sess-z",
          sentinel_event: "PreToolUse",
          hook: "tool_usage_gate",
          outcome: "deny",
          ts: "2026-05-26T00:00:00",
        },
      },
    ];
    render(<EventTicker events={denyEvent} onSelectNode={() => {}} />);
    const row = screen.getByTestId("ticker-rows").querySelector("li");
    expect(row?.getAttribute("data-actor")).toBe("sentinel");
    expect(row?.getAttribute("data-intervention")).toBe("true");
    expect(row?.className).toContain("intervention-row");
  });
});

describe("EventTicker", () => {
  it("renders skeleton placeholders in the empty state (P3-23 perceived perf)", () => {
    // P3-23: empty ticker now renders skeleton rows so the operator
    // sees the ticker structure during cold load instead of an
    // empty 360px column. Real rows replace skeletons when data
    // arrives — and crucially, skeletons carry their own testid so
    // tests can distinguish "loading" from "no data".
    render(<EventTicker events={[]} onSelectNode={() => {}} />);
    expect(screen.getByTestId("event-ticker")).toBeInTheDocument();
    const skeletons = screen.getAllByTestId("ticker-skeleton");
    expect(skeletons.length).toBeGreaterThan(0);
    // No REAL rows (no data-actor attribute) in the empty state.
    const realRows = screen
      .getByTestId("ticker-rows")
      .querySelectorAll("li[data-actor]");
    expect(realRows.length).toBe(0);
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

  describe("sticky stuck rows", () => {
    it("does not pin or highlight anything when stuckMeta is empty", () => {
      render(<EventTicker events={sampleEvents} onSelectNode={() => {}} stuckMeta={new Map()} />);
      expect(screen.queryByTestId("stuck-reason-line")).toBeNull();
      const rows = screen.getByTestId("ticker-rows").querySelectorAll("li.stuck-row");
      expect(rows.length).toBe(0);
    });

    it("pins the freshest row of a stuck session to the top with the stuck-row class", () => {
      // sess-b is the freshest event (UserPromptSubmit, last in sampleEvents),
      // sess-a has older events. If we mark sess-a stuck, its newest event
      // should jump to the top of the rendered list.
      const stuckMeta = new Map([
        ["sess-a", { ageSecs: 1800, kind: "AskUserQuestion", question: "still here?" }],
      ]);
      render(<EventTicker events={sampleEvents} onSelectNode={() => {}} stuckMeta={stuckMeta} />);
      const rows = screen.getByTestId("ticker-rows").querySelectorAll("li");
      // First row must belong to the stuck session and carry the
      // stuck-row class. We identify the session via data-session-id
      // (added in P3-20 for testability + future filtering — the
      // previous textContent check was brittle once we removed the
      // redundant `s:<sid>` prefix from the sub-line).
      expect(rows[0].className).toContain("stuck-row");
      expect(rows[0].getAttribute("data-session-id")).toBe("sess-a");
      expect(rows[0].getAttribute("data-stuck")).toBe("true");
    });

    it("renders the stuck-reason sub-line with age, kind, and question", () => {
      const stuckMeta = new Map([
        [
          "sess-a",
          { ageSecs: 18 * 60, kind: "AskUserQuestion", question: "Should we proceed with the migration?" },
        ],
      ]);
      render(<EventTicker events={sampleEvents} onSelectNode={() => {}} stuckMeta={stuckMeta} />);
      const reason = screen.getByTestId("stuck-reason-line");
      expect(reason.textContent).toMatch(/STUCK/);
      expect(reason.textContent).toMatch(/18m/);
      expect(reason.textContent).toMatch(/AskUserQuestion/);
      expect(reason.textContent).toMatch(/Should we proceed/);
    });

    it("truncates long questions to ~88 chars with an ellipsis", () => {
      const longQ = "a".repeat(200);
      const stuckMeta = new Map([
        ["sess-a", { ageSecs: 900, kind: null, question: longQ }],
      ]);
      render(<EventTicker events={sampleEvents} onSelectNode={() => {}} stuckMeta={stuckMeta} />);
      const reason = screen.getByTestId("stuck-reason-line");
      expect(reason.textContent).toMatch(/…/);
      // Should not contain the full 200 a's.
      expect(reason.textContent?.includes("a".repeat(200))).toBe(false);
    });

    it("falls back to 'awaiting' label when awaiting_kind is null", () => {
      const stuckMeta = new Map([
        ["sess-a", { ageSecs: 900, kind: null, question: null }],
      ]);
      render(<EventTicker events={sampleEvents} onSelectNode={() => {}} stuckMeta={stuckMeta} />);
      const reason = screen.getByTestId("stuck-reason-line");
      expect(reason.textContent).toMatch(/awaiting/);
    });

    it("intervention rows pin to top with intervention-row class when a session denies", () => {
      const denyEvents: RecentEvent[] = [
        // Claude tool call (background)
        {
          seq: 10,
          type: "sentinel.tool_call_observed",
          ts: "2026-05-26T00:00:00Z",
          payload: {
            session_id: "sess-claude",
            sentinel_event: "PreToolUse",
            tool: "Read",
            tool_call_id: "SentinelToolCall#tc-read",
            ts_sec: "2026-05-26T00:00:00",
          },
        },
        // Sentinel intervention (deny)
        {
          seq: 11,
          type: "sentinel.hook_ingested",
          ts: "2026-05-26T00:00:01Z",
          payload: {
            session_id: "sess-sentinel",
            sentinel_event: "PreToolUse",
            hook: "tool_usage_gate",
            outcome: "deny",
            ts: "2026-05-26T00:00:01",
          },
        },
      ];
      render(<EventTicker events={denyEvents} onSelectNode={() => {}} />);
      const rows = screen.getByTestId("ticker-rows").querySelectorAll("li");
      // Intervention pins to top.
      expect(rows[0].className).toContain("intervention-row");
      expect(rows[0].getAttribute("data-intervention")).toBe("true");
      expect(rows[0].getAttribute("data-actor")).toBe("sentinel");
    });

    it("only the FIRST event per stuck session is pinned (not every row from that session)", () => {
      // sess-a has 3 events (2 grouped Bash + 1 hook). If pinned, only
      // the freshest row from sess-a should carry the stuck class.
      const stuckMeta = new Map([
        ["sess-a", { ageSecs: 900, kind: "PreToolUse", question: null }],
      ]);
      render(<EventTicker events={sampleEvents} onSelectNode={() => {}} stuckMeta={stuckMeta} />);
      const pinnedRows = screen.getByTestId("ticker-rows").querySelectorAll("li.stuck-row");
      expect(pinnedRows.length).toBe(1);
    });

    it("pin TTL drops intervention rows older than the freshness window", () => {
      // Operator screenshot: a 12:59 user-prompt was pinned hours
      // later because pinning had no recency gate. With PIN_TTL_MS
      // = 30 min, an event timestamped 31 min before "now" must
      // fall back into the normal stream rather than squat the top.
      vi.setSystemTime(new Date("2026-05-26T00:35:00Z")); // 35 min after fixtures
      const staleDeny: RecentEvent[] = [
        {
          seq: 1,
          type: "sentinel.hook_ingested",
          ts: "2026-05-26T00:00:00Z", // 35 min old → past TTL
          payload: {
            session_id: "sess-old",
            sentinel_event: "PreToolUse",
            hook: "tool_usage_gate",
            outcome: "deny",
            ts: "2026-05-26T00:00:00",
          },
        },
        {
          seq: 2,
          type: "sentinel.tool_call_observed",
          ts: "2026-05-26T00:34:30Z", // 30s old → fresh
          payload: {
            session_id: "sess-fresh",
            sentinel_event: "PreToolUse",
            tool: "Read",
            tool_call_id: "SentinelToolCall#tc-r",
            ts_sec: "2026-05-26T00:34:30",
          },
        },
      ];
      render(<EventTicker events={staleDeny} onSelectNode={() => {}} />);
      const rows = screen.getByTestId("ticker-rows").querySelectorAll("li");
      // The stale deny exists but no longer has the intervention-row class.
      const interventionRows = screen.getByTestId("ticker-rows").querySelectorAll("li.intervention-row");
      expect(interventionRows.length).toBe(0);
      // Both events still render, just no pin class.
      expect(rows.length).toBe(2);
    });

    it("pin cap (PIN_MAX_PER_CLASS) bounds intervention pins to N per render", () => {
      // Three distinct sessions each fire one intervention. The cap
      // is 2 per class, so only the first two encountered get the
      // intervention-row class. The third stays in the normal stream.
      const denies: RecentEvent[] = ["sess-i1", "sess-i2", "sess-i3"].map((sid, i) => ({
        seq: i + 1,
        type: "sentinel.hook_ingested",
        ts: `2026-05-26T00:00:${String(i).padStart(2, "0")}Z`,
        payload: {
          session_id: sid,
          sentinel_event: "PreToolUse",
          hook: "tool_usage_gate",
          outcome: "deny",
          ts: `2026-05-26T00:00:${String(i).padStart(2, "0")}`,
        },
      }));
      render(<EventTicker events={denies} onSelectNode={() => {}} />);
      const interventionRows = screen.getByTestId("ticker-rows").querySelectorAll("li.intervention-row");
      expect(interventionRows.length).toBe(2);
    });
  });
});
