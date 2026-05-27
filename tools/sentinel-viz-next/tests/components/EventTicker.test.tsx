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
  vi.useFakeTimers({ toFake: ["Date"] });
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
      // Now is 45 min after the stale deny — past both PIN_TTL_MS
      // (30 min) AND DECAY_TTL_MS (5 min). The stale deny therefore
      // (a) doesn't pin and (b) decays out of the stream. Only the
      // fresh tc-r row remains.
      vi.setSystemTime(new Date("2026-05-26T00:45:00Z"));
      const staleDeny: RecentEvent[] = [
        {
          seq: 1,
          type: "sentinel.hook_ingested",
          ts: "2026-05-26T00:00:00Z", // 45 min old → past both TTLs
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
          ts: "2026-05-26T00:44:30Z", // 30s old → fresh
          payload: {
            session_id: "sess-fresh",
            sentinel_event: "PreToolUse",
            tool: "Read",
            tool_call_id: "SentinelToolCall#tc-r",
            ts_sec: "2026-05-26T00:44:30",
          },
        },
      ];
      render(<EventTicker events={staleDeny} onSelectNode={() => {}} />);
      // Direct children only — avoids RolledPreview nested <li>s if
      // they happen to render under any row in this fixture.
      const rows = screen.getByTestId("ticker-rows").children;
      const interventionRows = screen.getByTestId("ticker-rows").querySelectorAll("li.intervention-row");
      expect(interventionRows.length).toBe(0);
      // Stale deny is decayed out — only the fresh tc-r row remains.
      expect(rows.length).toBe(1);
    });

    it("intervention decay drops an unpinned deny once DECAY_NEWER_EVENT_COUNT same-session newer events arrive", () => {
      // Three distinct sessions each fire one deny — pin cap is 2,
      // so the third session's deny is forced into the normal stream.
      // Then that third session produces 3 newer tool-call events.
      // Decay should drop the deny entirely; the 3 newer events
      // collapse to one ×3 Bash row. Operator scenario: a long-
      // finished teammate process whose deny is no longer current
      // context but used to sit in the ticker forever.
      vi.setSystemTime(new Date("2026-05-26T00:02:00Z"));
      // buildRows iterates events NEWEST → OLDEST (lower index =
      // older). So the fixture is laid out oldest-first by index so
      // pin order matches iteration order: sess-decay's deny is the
      // OLDEST entry (index 0) and arrives at the pin gate LAST,
      // after sess-pin-1 + sess-pin-2 have filled the cap.
      const events: RecentEvent[] = [
        // OLDEST — sess-decay's deny. Gets visited last by the pin
        // gate, falls through past the cap, then decays because
        // sess-decay has 3 newer events.
        {
          seq: 1,
          type: "sentinel.hook_ingested",
          ts: "2026-05-26T00:00:00Z",
          payload: {
            session_id: "sess-decay",
            sentinel_event: "PreToolUse",
            hook: "tool_usage_gate",
            outcome: "deny",
            ts: "2026-05-26T00:00:00",
          },
        },
        // sess-pin-1 deny — pins (visited second-to-last).
        {
          seq: 2,
          type: "sentinel.hook_ingested",
          ts: "2026-05-26T00:00:30Z",
          payload: {
            session_id: "sess-pin-1",
            sentinel_event: "PreToolUse",
            hook: "tool_usage_gate",
            outcome: "deny",
            ts: "2026-05-26T00:00:30",
          },
        },
        // sess-pin-2 deny — pins (visited third-to-last, before
        // sess-pin-1 in iteration order since it's newer).
        {
          seq: 3,
          type: "sentinel.hook_ingested",
          ts: "2026-05-26T00:00:45Z",
          payload: {
            session_id: "sess-pin-2",
            sentinel_event: "PreToolUse",
            hook: "tool_usage_gate",
            outcome: "deny",
            ts: "2026-05-26T00:00:45",
          },
        },
        // 3 newer events on sess-decay — newest, visited first.
        // These bump the per-session newer-count past the decay
        // threshold so sess-decay's deny gets dropped.
        ...[1, 2, 3].map((i): RecentEvent => ({
          seq: i + 10,
          type: "sentinel.tool_call_observed",
          ts: `2026-05-26T00:01:0${i}Z`,
          payload: {
            session_id: "sess-decay",
            sentinel_event: "PreToolUse",
            tool: "Bash",
            tool_call_id: `SentinelToolCall#tc-${i}`,
            ts_sec: `2026-05-26T00:01:0${i}`,
          },
        })),
      ];
      render(<EventTicker events={events} onSelectNode={() => {}} />);
      // Only the two pinned denies remain visible as interventions —
      // the third was decayed out of the stream entirely.
      const interventionRows = screen.getByTestId("ticker-rows").querySelectorAll("li.intervention-row");
      expect(interventionRows.length).toBe(2);
      // Count DIRECT children of ticker-rows only — RolledPreview
      // nests its own <li>s, and the tc-rollup will render up to 3
      // preview lines under it. We care about top-level row count.
      const topLevelRows = screen.getByTestId("ticker-rows").children;
      // 2 pinned denies + 1 sess-decay tc rollup = 3 top-level rows.
      expect(topLevelRows.length).toBe(3);
    });

    it("decay treats unsuffixed (naked) timestamps as UTC, not local — regression", () => {
      // Bug shipped in 12cb10a7: the decay code used `new Date(r.ts)`
      // directly, but the bridge writes `ts_sec` without a timezone
      // marker. JS parses naked strings as LOCAL time. An operator
      // in UTC-7 reading a "15:04" event would see `t` come back as
      // 22:04 UTC, putting the row 7 hours in the FUTURE. `now - t`
      // then went negative and `now - t > DECAY_TTL_MS` returned
      // false — so 3+ hour old denies never aged out.
      //
      // This test simulates the failure mode: an old deny with a
      // naked-UTC ts_sec, viewed from a wall-clock "now" that's
      // hours later. With the fix (parseTsMs appends Z), the row
      // decays correctly. Without it, the row survives and this
      // assertion fails.
      vi.setSystemTime(new Date("2026-05-27T21:30:00Z"));
      const oldDeny: RecentEvent[] = [
        {
          seq: 1,
          type: "sentinel.hook_ingested",
          ts: "2026-05-27T15:04:00", // ~6.5h old, NO Z suffix
          payload: {
            session_id: "sess-flight",
            sentinel_event: "PreToolUse",
            hook: "tool_usage_gate",
            outcome: "deny",
            ts_sec: "2026-05-27T15:04:00", // bridge format
          },
        },
        // A fresh tool-call so the ticker doesn't render skeletons
        // (which would make `rows.length` checks ambiguous).
        {
          seq: 2,
          type: "sentinel.tool_call_observed",
          ts: "2026-05-27T21:29:30Z",
          payload: {
            session_id: "sess-active",
            sentinel_event: "PreToolUse",
            tool: "Read",
            tool_call_id: "SentinelToolCall#tc-r",
            ts_sec: "2026-05-27T21:29:30",
          },
        },
      ];
      render(<EventTicker events={oldDeny} onSelectNode={() => {}} />);
      // Old deny must NOT survive. Without the UTC fix it would,
      // because the local-parsed "future" timestamp dodges every
      // age check.
      const interventionRows = screen.getByTestId("ticker-rows").querySelectorAll("li.intervention-row");
      expect(interventionRows.length).toBe(0);
      const topLevelRows = screen.getByTestId("ticker-rows").children;
      expect(topLevelRows.length).toBe(1); // only the fresh tc-r row
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
