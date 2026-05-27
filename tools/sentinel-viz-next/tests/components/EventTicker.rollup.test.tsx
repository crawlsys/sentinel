import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { EventTicker, shouldShowSubLine, compactSummaryFor } from "../../components/EventTicker";
import { indexActivity, _resetActivityCache } from "../../adapters/activity-cache";
import type { ActivityResponse, RecentEvent } from "../../types/api";

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

  it("shouldShowSubLine — intervention outcome → true", () => {
    expect(
      shouldShowSubLine({
        outcome: "deny",
        sentinelEvent: "PreToolUse",
        actor: "sentinel",
      }),
    ).toBe(true);
  });

  it("shouldShowSubLine — routine 'allow' outcome → false (no 'about to run · allow' spam)", () => {
    expect(
      shouldShowSubLine({
        outcome: "allow",
        sentinelEvent: "PreToolUse",
        actor: "claude",
      }),
    ).toBe(false);
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

  it("user prompts within the multi-hook fanout window (≤5s) merge into one row", () => {
    // Reality: every UserPromptSubmit fires N hooks (memory_inject,
    // phase_validator, hygiene_reminders, error_reporter, ...) and
    // the bridge emits one sentinel.hook_ingested per hook —
    // identical content, ts within ms. Previously the ticker
    // refused to collapse any UserPromptSubmit rows, so each
    // prompt rendered as 6-8 duplicate rows in the operator's
    // view. Now we merge when sig matches AND inter-event delta
    // ≤5s. Two events 1s apart represent the same operator turn
    // with multi-hook fanout — collapse to one row with ×2.
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
    expect(rows.length).toBe(1);
    expect(rows[0].textContent).toContain("×2");
  });

  it("user prompts >5s apart stay distinct — genuine back-to-back operator turns", () => {
    // Operator-typed back-to-back prompts in a real session are
    // separated by at least the assistant's response time
    // (typically 10s+). Anything beyond the 5s multi-hook fanout
    // window is treated as a distinct turn and rendered as its
    // own row so we don't hide operator content behind a multiplier.
    render(
      <EventTicker
        events={[
          userPromptEvent(1, "s-a"),
          userPromptEvent(30, "s-a"),
        ]}
        onSelectNode={() => {}}
      />,
    );
    const rows = screen.getByTestId("ticker-rows").querySelectorAll("li[data-actor='user']");
    expect(rows.length).toBe(2);
  });

  it("compactSummaryFor strips cd-prefix, tildifies, and truncates Bash commands", () => {
    const out = compactSummaryFor(
      "Bash",
      "cd /home/kcrawley/projects/basilisk; git push -u origin feat/foo",
    );
    expect(out).not.toContain("cd ");
    expect(out).not.toContain("/home/kcrawley");
    expect(out).toContain("git push");
  });

  it("compactSummaryFor tildifies + end-truncates file paths for Read/Write/Edit", () => {
    const longPath = "/home/kcrawley/projects/basilisk/tools/sentinel-viz-next/components/EventTicker.tsx";
    const out = compactSummaryFor("Edit", longPath);
    expect(out).not.toContain("/home/kcrawley");
    expect(out.endsWith("EventTicker.tsx")).toBe(true);
  });

  it("clicking the row body (not just the ×N badge) toggles the flyout open (P3-27)", () => {
    const sessionId = "sess-clickopen";
    const events: RecentEvent[] = [
      tcEvent(1, sessionId, "Bash", "tcA"),
      tcEvent(2, sessionId, "Read", "tcB"),
      tcEvent(3, sessionId, "Edit", "tcC"),
    ];
    const onSelect = vi.fn();
    const { container } = render(
      <EventTicker events={events} onSelectNode={onSelect} />,
    );
    // Before click — no flyout rendered (rolled-preview is shown,
    // but the flyout list is gated on isOpen).
    expect(container.querySelector('[data-testid="ticker-flyout"]')).toBeNull();
    // Click the row body via the label span (not the ×N badge).
    // The whole non-flyout area is now a single clickable surface,
    // so any element inside should bubble to the row-click handler.
    const row = container.querySelector('[data-testid="ticker-rows"] li[data-actor]');
    expect(row).not.toBeNull();
    const labelTextEl = row!.querySelector("span.truncate.flex-1");
    fireEvent.click(labelTextEl!);
    // Flyout should now be open.
    expect(container.querySelector('[data-testid="ticker-flyout"]')).not.toBeNull();
    // AND the row's underlying node was selected — both actions
    // happen on a single click for rolled rows.
    expect(onSelect).toHaveBeenCalled();
  });

  it("single-tool rows DO NOT auto-toggle a flyout on click (no flyout to open)", () => {
    const sessionId = "sess-single";
    // Just ONE event = members.length === 1, no flyout exists.
    const events: RecentEvent[] = [tcEvent(1, sessionId, "Bash", "tcA")];
    const onSelect = vi.fn();
    const { container } = render(
      <EventTicker events={events} onSelectNode={onSelect} />,
    );
    const row = container.querySelector('[data-testid="ticker-rows"] li[data-actor]');
    fireEvent.click(row!.querySelector("span.truncate.flex-1")!);
    // Click still selects; no flyout opens (there's nothing to expand).
    expect(onSelect).toHaveBeenCalled();
    expect(container.querySelector('[data-testid="ticker-flyout"]')).toBeNull();
  });

  it("events from dormant sessions are filtered out — they don't pad the ticker (P3-28)", () => {
    // Two active session rows + two stale dormant ones. The
    // dormant ones should never reach buildRows; we expect ONLY
    // the two active rows to render.
    const events: RecentEvent[] = [
      tcEvent(1, "s-active", "Bash", "tcA"),
      userPromptEvent(2, "s-dormant"),
      tcEvent(3, "s-active", "Read", "tcB"),
      userPromptEvent(4, "s-dormant"),
    ];
    const dormant = new Set(["s-dormant"]);
    const { container } = render(
      <EventTicker events={events} onSelectNode={() => {}} dormantSessionIds={dormant} />,
    );
    const rows = container.querySelectorAll('[data-testid="ticker-rows"] li[data-actor]');
    // No row for s-dormant.
    const sessionIds = Array.from(rows).map((r) => r.getAttribute("data-session-id"));
    expect(sessionIds).not.toContain("s-dormant");
    expect(sessionIds.filter((s) => s === "s-active").length).toBeGreaterThan(0);
  });

  it("empty dormantSessionIds set lets ALL events through (no filtering)", () => {
    const events: RecentEvent[] = [
      tcEvent(1, "s-a", "Bash", "tcA"),
      userPromptEvent(2, "s-b"),
    ];
    const { container } = render(
      <EventTicker events={events} onSelectNode={() => {}} dormantSessionIds={new Set()} />,
    );
    const sessionIds = Array.from(
      container.querySelectorAll('[data-testid="ticker-rows"] li[data-actor]'),
    ).map((r) => r.getAttribute("data-session-id"));
    expect(sessionIds).toContain("s-a");
    expect(sessionIds).toContain("s-b");
  });

  it("singleton row with augment renders TWO-LINE: label on one line, indented preview below (P3-28)", () => {
    _resetActivityCache();
    const sessionId = "sess-two-line";
    const fakeActivity: ActivityResponse = {
      session_id: sessionId,
      transcript: "x.jsonl",
      events: [],
      segments: [
        {
          ts: "2026-05-26T00:00:00",
          kind: "assistant_turn",
          label: "TaskUpdate",
          preview: "",
          tools: ["TaskUpdate"],
          tool_calls: [
            { id: "tc-task", tool: "TaskUpdate", summary: "task #19 → completed" },
          ],
          tool_count: 1,
        },
      ],
    };
    indexActivity(sessionId, fakeActivity);
    const events: RecentEvent[] = [tcEvent(1, sessionId, "TaskUpdate", "tcOne")];
    const { container } = render(
      <EventTicker events={events} onSelectNode={() => {}} />,
    );
    // The singleton-augment block (label-below content) is present.
    const augBlock = container.querySelector('[data-testid="singleton-augment"]');
    expect(augBlock).not.toBeNull();
    expect(augBlock!.textContent).toContain("task #19");
    // The header line should NOT also embed the augment — that'd
    // be duplication. The header span (truncate flex-1) carries
    // ONLY the label.
    const row = container.querySelector('[data-testid="ticker-rows"] li[data-actor]');
    const headerLabel = row!.querySelector("span.truncate.flex-1");
    expect(headerLabel?.textContent?.trim()).toBe("TaskUpdate");
  });

  it("user prompt rows surface the actual prompt text from the prompt cache (P3-27)", () => {
    _resetActivityCache();
    const sessionId = "sess-userprompt";
    // Pre-seed activity-cache with a user_input segment whose preview
    // is the operator's prompt text. The ticker's user-prompt row at
    // that ts should now display the text alongside the "user prompt"
    // label instead of being content-free.
    const fakeActivity: ActivityResponse = {
      session_id: sessionId,
      transcript: "x.jsonl",
      events: [],
      segments: [
        {
          ts: "2026-05-26T00:00:00",
          kind: "user_input",
          label: "user input",
          preview: "fix the rolled-row preview list please",
          tools: [],
          tool_count: 0,
        },
      ],
    };
    indexActivity(sessionId, fakeActivity);

    const events: RecentEvent[] = [userPromptEvent(1, sessionId)];
    // The default userPromptEvent fixture uses ts_sec "2026-05-25T00:00:01"
    // which is a day off — fix the timestamp to match our seeded segment.
    events[0].payload.ts_sec = "2026-05-26T00:00:00";
    events[0].ts = "2026-05-26T00:00:00Z";

    const { container } = render(
      <EventTicker events={events} onSelectNode={() => {}} />,
    );
    const userRow = container.querySelector('[data-testid="ticker-rows"] li[data-actor="user"]');
    expect(userRow).not.toBeNull();
    expect(userRow!.textContent).toContain("user prompt");
    expect(userRow!.textContent).toContain("fix the rolled-row preview list please");
  });

  it("flyout members show actual command from the activity-cache (not just tcid)", async () => {
    _resetActivityCache();
    const sessionId = "sess-flyout";
    // Pre-seed the activity-cache with a ToolCallSummary for one
    // Bash invocation at ts = 2026-05-26T00:00:00.
    const fakeActivity: ActivityResponse = {
      session_id: sessionId,
      transcript: "x.jsonl",
      events: [],
      segments: [
        {
          ts: "2026-05-26T00:00:00",
          kind: "assistant_turn",
          label: "Bash",
          preview: "cd /tmp; ls",
          tools: ["Bash"],
          tool_calls: [
            {
              id: "tc-bash-1",
              tool: "Bash",
              summary: "cd /home/kcrawley/projects/x; cargo test --workspace",
              result_preview: "test result: ok. 42 passed",
            },
          ],
          tool_count: 1,
        },
      ],
    };
    indexActivity(sessionId, fakeActivity);

    const events: RecentEvent[] = [
      // Same minute bucket as the cached entry; rolled row of two identical Bashes.
      tcEvent(1, sessionId, "Bash", "tcA"),
      tcEvent(2, sessionId, "Bash", "tcB"),
    ];
    const { findByText, container } = render(
      <EventTicker events={events} onSelectNode={() => {}} />,
    );
    // Open the ×2 flyout.
    const badge = await findByText(/×2/);
    fireEvent.click(badge);
    // The expanded flyout should render the compacted Bash command
    // (tildified, cd-stripped, smart-trunc). The lookup is exact
    // minute by default — fixture and event share the same minute.
    const flyout = container.querySelector('[data-testid="ticker-flyout"]');
    const text = flyout?.textContent ?? "";
    expect(text).toContain("cargo test --workspace");
    expect(text).not.toContain("/home/kcrawley");
  });

  it("rolled rows render inline preview lines so operator sees content without expanding (P3-26)", () => {
    _resetActivityCache();
    const sessionId = "sess-preview";
    const fakeActivity: ActivityResponse = {
      session_id: sessionId,
      transcript: "x.jsonl",
      events: [],
      segments: [
        {
          ts: "2026-05-26T00:00:00",
          kind: "assistant_turn",
          label: "Bash",
          preview: "tests",
          tools: ["Bash", "Read", "Edit"],
          tool_calls: [
            {
              id: "tc-1",
              tool: "Bash",
              summary: "cd /home/kcrawley/projects/x; cargo test --workspace",
              result_preview: "test result: ok. 42 passed",
            },
            {
              id: "tc-2",
              tool: "Read",
              summary: "/home/kcrawley/projects/x/src/lib.rs",
              result_preview: "...",
            },
            {
              id: "tc-3",
              tool: "Edit",
              summary: "/home/kcrawley/projects/x/src/foo.rs",
              result_preview: "...",
            },
          ],
          tool_count: 3,
        },
      ],
    };
    indexActivity(sessionId, fakeActivity);

    const events: RecentEvent[] = [
      tcEvent(10, sessionId, "Bash", "tcA"),
      tcEvent(11, sessionId, "Read", "tcB"),
      tcEvent(12, sessionId, "Edit", "tcC"),
      tcEvent(13, sessionId, "Bash", "tcD"),
    ];
    const { container } = render(
      <EventTicker events={events} onSelectNode={() => {}} />,
    );

    // The rolled row should embed a preview list — NO need to expand.
    const preview = container.querySelector('[data-testid="rolled-preview"]');
    expect(preview).not.toBeNull();
    const lines = preview!.querySelectorAll("li");
    expect(lines.length).toBeGreaterThanOrEqual(2);
    const previewText = preview!.textContent ?? "";
    // Tildified bash command (no /home/kcrawley).
    expect(previewText).not.toContain("/home/kcrawley");
    expect(previewText).toContain("cargo test");
  });

  it("strict-collapsed single-tool rows DO render an inline preview (reversed from P3-27)", () => {
    // 3 identical Bash events strict-collapse to ONE row with
    // tools=[Bash] (length 1) BUT members.length === 3. The earlier
    // rule "single-tool rolls skip the preview, the label augment
    // carries it" turned out to be wrong: the singleton-augment
    // block is gated on members.length === 1, so an ×3 Bash row
    // had no payload at all. Operator screenshot showed a column
    // of bare `×9 ▶ Bash` rows with no indication of what was run.
    // RolledPreview now fires whenever members.length > 1 — the
    // dedupe-by-(tool, summary) inside it handles the "all calls
    // identical" case gracefully (one line, not nine copies).
    _resetActivityCache();
    const sessionId = "sess-strict-1tool";
    const fakeActivity: ActivityResponse = {
      session_id: sessionId,
      transcript: "x.jsonl",
      events: [],
      segments: [
        {
          ts: "2026-05-26T00:00:00",
          kind: "assistant_turn",
          label: "Bash",
          preview: "",
          tools: ["Bash"],
          tool_calls: [
            { id: "tc-X", tool: "Bash", summary: "git status --short" },
          ],
          tool_count: 1,
        },
      ],
    };
    indexActivity(sessionId, fakeActivity);
    const events: RecentEvent[] = [
      tcEvent(1, sessionId, "Bash", "tcA"),
      tcEvent(2, sessionId, "Bash", "tcB"),
      tcEvent(3, sessionId, "Bash", "tcC"),
    ];
    const { container } = render(
      <EventTicker events={events} onSelectNode={() => {}} />,
    );
    const preview = container.querySelector('[data-testid="rolled-preview"]');
    expect(preview).not.toBeNull();
    // All three members map to the same activity-cache entry — the
    // dedupe inside RolledPreview should collapse them to a single
    // preview line, not three.
    const lines = preview!.querySelectorAll("li");
    expect(lines.length).toBe(1);
    expect(preview!.textContent ?? "").toContain("git status --short");
  });

  it("rolled-preview shows '(loading…)' placeholder when cache hasn't warmed yet (P3-26)", () => {
    _resetActivityCache(); // ensure NO cache entries
    const sessionId = "sess-cold";
    const events: RecentEvent[] = [
      tcEvent(1, sessionId, "Bash", "tcA"),
      tcEvent(2, sessionId, "Read", "tcB"),
    ];
    const { container } = render(
      <EventTicker events={events} onSelectNode={() => {}} />,
    );
    const preview = container.querySelector('[data-testid="rolled-preview"]');
    expect(preview).not.toBeNull();
    // Each line should show the tool name + (loading…) placeholder.
    const text = preview!.textContent ?? "";
    expect(text).toContain("Bash");
    expect(text).toContain("Read");
    expect(text).toContain("(loading…)");
  });

  it("flyout members show git diff stats chip when result_preview includes the standard footer", async () => {
    _resetActivityCache();
    const sessionId = "sess-diff";
    const fakeActivity: ActivityResponse = {
      session_id: sessionId,
      transcript: "x.jsonl",
      events: [],
      segments: [
        {
          ts: "2026-05-26T00:00:00",
          kind: "assistant_turn",
          label: "Bash",
          preview: "git commit",
          tools: ["Bash"],
          tool_calls: [
            {
              id: "tc-commit",
              tool: "Bash",
              summary: "git commit -m 'feat: x'",
              result_preview: "[main abc1234] feat: x\n 3 files changed, 25 insertions(+), 5 deletions(-)",
            },
          ],
          tool_count: 1,
        },
      ],
    };
    indexActivity(sessionId, fakeActivity);

    const events: RecentEvent[] = [
      tcEvent(1, sessionId, "Bash", "tcA"),
      tcEvent(2, sessionId, "Bash", "tcB"),
    ];
    const { findByText, container } = render(
      <EventTicker events={events} onSelectNode={() => {}} />,
    );
    const badge = await findByText(/×2/);
    fireEvent.click(badge);
    const stats = container.querySelectorAll('[data-testid="flyout-diff-stats"]');
    expect(stats.length).toBeGreaterThan(0);
    const text = stats[0].textContent ?? "";
    expect(text).toContain("+25");
    expect(text).toContain("-5");
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
