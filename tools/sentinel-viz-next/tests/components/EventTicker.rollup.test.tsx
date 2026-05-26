import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";

import { EventTicker, shouldShowSubLine } from "../../components/EventTicker";
import type { RecentEvent } from "../../types/api";

/// P3-24: smarter rollup. Adjacent ROUTINE claude tool-call events
/// in the same session+category collapse into a single row whose
/// label is a deduped tools list ("Bash, Read, Edit ×N"). Interventions
/// and user prompts never collapse — they remain one row apiece.
///
/// P3-22: shouldShowSubLine gates the sub-line so it only renders
/// when there's actual signal (an outcome, an unknown event). Routine
/// PreToolUse rows DON'T render a "about to run" sub-line — that
/// was pure visual noise.

function tcEvent(seq: number, sid: string, tool: string, tcid: string): RecentEvent {
  return {
    seq,
    type: "sentinel.tool_call_observed",
    ts: `2026-05-26T00:00:${String(seq).padStart(2, "0")}Z`,
    payload: {
      session_id: sid,
      sentinel_event: "PreToolUse",
      tool,
      tool_call_id: `SentinelToolCall#${tcid}`,
      ts_sec: `2026-05-26T00:00:${String(seq).padStart(2, "0")}`,
    },
  };
}

function denyEvent(seq: number, sid: string): RecentEvent {
  return {
    seq,
    type: "sentinel.hook_ingested",
    ts: `2026-05-26T00:00:${String(seq).padStart(2, "0")}Z`,
    payload: {
      session_id: sid,
      sentinel_event: "PreToolUse",
      hook: "tool_usage_gate",
      outcome: "deny",
      ts: `2026-05-26T00:00:${String(seq).padStart(2, "0")}`,
    },
  };
}

function userPromptEvent(seq: number, sid: string): RecentEvent {
  return {
    seq,
    type: "sentinel.tool_call_observed",
    ts: `2026-05-26T00:00:${String(seq).padStart(2, "0")}Z`,
    payload: {
      session_id: sid,
      sentinel_event: "UserPromptSubmit",
      tool: "",
      tool_call_id: `SentinelToolCall#u${seq}`,
      ts_sec: `2026-05-26T00:00:${String(seq).padStart(2, "0")}`,
    },
  };
}

describe("EventTicker — P3-22 sub-line noise gating", () => {
  it("routine PreToolUse rows do NOT render a 'about to run' sub-line", () => {
    render(
      <EventTicker
        events={[tcEvent(1, "s-a", "Bash", "tc1")]}
        onSelectNode={() => {}}
      />,
    );
    // The pre-P3-22 sub-line read "about to run". After P3-22 it's
    // gated away on routine rows. We assert the literal text is
    // absent — the row itself still renders with its label.
    const ticker = screen.getByTestId("ticker-rows").textContent ?? "";
    expect(ticker).toContain("Bash"); // label still there
    expect(ticker).not.toContain("about to run");
  });

  it("intervention rows (deny) DO render a sub-line with the outcome", () => {
    render(
      <EventTicker
        events={[denyEvent(2, "s-a")]}
        onSelectNode={() => {}}
      />,
    );
    const ticker = screen.getByTestId("ticker-rows").textContent ?? "";
    // Operator MUST see "deny" — that's the whole point of the sub-line existing.
    expect(ticker).toContain("deny");
  });

  it("user prompt rows do not need a sub-line (the label + glyph already convey it)", () => {
    render(
      <EventTicker
        events={[userPromptEvent(3, "s-a")]}
        onSelectNode={() => {}}
      />,
    );
    const ticker = screen.getByTestId("ticker-rows").textContent ?? "";
    expect(ticker).toContain("user prompt");
    expect(ticker).not.toContain("you submitted");
  });

  it("shouldShowSubLine — outcome present → true", () => {
    expect(
      shouldShowSubLine({
        outcome: "deny",
        sentinelEvent: "PreToolUse",
        actor: "sentinel",
      }),
    ).toBe(true);
  });

  it("shouldShowSubLine — known sentinel event with no outcome → false", () => {
    expect(
      shouldShowSubLine({
        outcome: null,
        sentinelEvent: "PreToolUse",
        actor: "claude",
      }),
    ).toBe(false);
  });

  it("shouldShowSubLine — unknown sentinel event → true (don't hide novel events)", () => {
    expect(
      shouldShowSubLine({
        outcome: null,
        sentinelEvent: "SomeFutureEvent",
        actor: "claude",
      }),
    ).toBe(true);
  });
});

describe("EventTicker — P3-24 smarter rollup", () => {
  it("collapses adjacent same-session Bash/Read/Edit into ONE row labelled 'Bash, Read, Edit'", () => {
    // 5 routine claude tool calls, same session, different tools.
    // Pre-P3-24: 5 rows. Post-P3-24: 1 row with deduped tools + ×5.
    render(
      <EventTicker
        events={[
          tcEvent(1, "s-a", "Bash", "tc1"),
          tcEvent(2, "s-a", "Read", "tc2"),
          tcEvent(3, "s-a", "Edit", "tc3"),
          tcEvent(4, "s-a", "Bash", "tc4"),
          tcEvent(5, "s-a", "Read", "tc5"),
        ]}
        onSelectNode={() => {}}
      />,
    );
    const rows = screen.getByTestId("ticker-rows").querySelectorAll("li[data-actor]");
    expect(rows.length).toBe(1);
    const txt = rows[0].textContent ?? "";
    // Deduped tools join — order is first-seen as we walk newest-
    // first, so the freshest tool comes first and dedup drops
    // earlier repeats. The exact order is "Read, Bash, Edit" with
    // the fixture above, but we only care that all three tool
    // names appear.
    expect(txt).toContain("Bash");
    expect(txt).toContain("Read");
    expect(txt).toContain("Edit");
    // ×5 badge shows total members.
    expect(txt).toContain("×5");
  });

  it("does NOT collapse across sessions even when category matches", () => {
    render(
      <EventTicker
        events={[
          tcEvent(1, "s-a", "Bash", "tc1"),
          tcEvent(2, "s-b", "Read", "tc2"),
        ]}
        onSelectNode={() => {}}
      />,
    );
    const rows = screen.getByTestId("ticker-rows").querySelectorAll("li[data-actor]");
    expect(rows.length).toBe(2);
  });

  it("does NOT collapse across categories (compute + planning stay distinct)", () => {
    render(
      <EventTicker
        events={[
          tcEvent(1, "s-a", "Bash", "tc1"), // tc category
          tcEvent(2, "s-a", "TaskUpdate", "tc2"), // planning category
        ]}
        onSelectNode={() => {}}
      />,
    );
    const rows = screen.getByTestId("ticker-rows").querySelectorAll("li[data-actor]");
    expect(rows.length).toBe(2);
  });

  it("intervention deny-storm collapses via strict sig (deny ×N) so it doesn't dominate the ticker", () => {
    // Three identical deny events share the same strict sig
    // (same sid, hook, outcome, no tool). The strict path collapses
    // them — operator sees one pinned "deny ×3" row instead of
    // three identical red-pulsing rows that would dominate the
    // ticker head. The flyout (×3) still gives access to each
    // individual member.
    render(
      <EventTicker
        events={[denyEvent(1, "s-a"), denyEvent(2, "s-a"), denyEvent(3, "s-a")]}
        onSelectNode={() => {}}
      />,
    );
    const rows = screen.getByTestId("ticker-rows").querySelectorAll("li[data-actor]");
    expect(rows.length).toBe(1);
    expect(rows[0].getAttribute("data-actor")).toBe("sentinel");
    expect(rows[0].getAttribute("data-intervention")).toBe("true");
    expect(rows[0].textContent).toContain("×3");
  });

  it("user prompts NEVER collapse — operator content is too signal-dense to merge silently", () => {
    // P3-24 fix: strict-sig collapse used to also merge user
    // prompts because two UserPromptSubmit events share an
    // identical sig (no tool, no outcome). User prompts almost
    // always have different content, so collapsing would silently
    // hide an operator turn. Now gated.
    render(
      <EventTicker
        events={[
          userPromptEvent(1, "s-a"),
          userPromptEvent(2, "s-a"),
        ]}
        onSelectNode={() => {}}
      />,
    );
    const rows = screen.getByTestId("ticker-rows").querySelectorAll("li[data-actor='user']");
    expect(rows.length).toBe(2);
  });

  it("single-tool same-session burst still collapses to one row labelled with just the tool", () => {
    // 3 identical Bash calls — strict-sig path still collapses, label = "Bash".
    render(
      <EventTicker
        events={[
          tcEvent(1, "s-a", "Bash", "tc1"),
          tcEvent(2, "s-a", "Bash", "tc2"),
          tcEvent(3, "s-a", "Bash", "tc3"),
        ]}
        onSelectNode={() => {}}
      />,
    );
    const rows = screen.getByTestId("ticker-rows").querySelectorAll("li[data-actor]");
    expect(rows.length).toBe(1);
    const txt = rows[0].textContent ?? "";
    expect(txt).toContain("Bash");
    expect(txt).toContain("×3");
    // Label should NOT contain a comma — single tool, not a roll-up.
    expect(txt.split("\n")[0]).not.toContain(",");
  });
});
