"use client";

import { memo, useEffect, useMemo, useState } from "react";

import type { NodeCategory, RecentEvent } from "../types/api";
import { lookup as lookupActivityCache, subscribe as subscribeActivityCache } from "../lib/activity-cache";
import { categoryColor, categoryLabel, tickerTime } from "../lib/format";

interface Props {
  events: RecentEvent[];
  onSelectNode: (nodeId: string, eventTs?: string) => void;
  /** sid → color. From session-colors.sessionColorMap(graph). */
  sessionColors?: Map<string, string>;
  /** session_id → why-stuck context. Sessions present here get
   *  their freshest event pinned to the top of the ticker with a
   *  pulsing-red highlight AND an inline "⚠ STUCK Xm — <question>"
   *  sub-line so the operator can see what's being asked without
   *  having to click anything. */
  stuckMeta?: Map<string, StuckMeta>;
}

export interface StuckMeta {
  /** Seconds since the session's last activity (i.e. how long the
   *  ask has been parked). */
  ageSecs: number;
  /** awaiting_kind from the bridge (e.g. "AskUserQuestion",
   *  "PreToolUse", "Stop"), null when unknown. */
  kind: string | null;
  /** awaiting_question text from the bridge, truncated for display.
   *  Null when no question text is available (e.g. tool-permission
   *  stalls), in which case the kind is shown alone. */
  question: string | null;
}

interface TickerMember {
  ts: string;
  toolCallId: string | null;
  outcome: string | null;
}

/// Who originated this row's underlying event.
///   - "claude"   — the agent doing things (tool calls, observations,
///                  assistant turns). The default for tool_call_observed.
///   - "sentinel" — the control plane intervening: hook-ingested
///                  events or any row whose outcome reflects a real
///                  decision (deny / inject / force_stop).
///   - "user"     — operator input. UserPromptSubmit / ask responses.
///
/// Derivation is deterministic from existing fields (event type +
/// sentinel_event + outcome) — no backend change needed.
export type RowActor = "claude" | "sentinel" | "user";

interface TickerRow {
  /** Stable React key. */
  key: string;
  /** Grouping signature, excludes per-event tool_call_id. */
  sig: string;
  ts: string;
  sessionId: string | null;
  sentinelEvent: string;
  label: string;
  /** Optional " · <snippet>" rendered after the label when the
   *  activity-cache has matched this event's tool input. */
  augment?: string;
  toolCallId: string | null;
  outcome: string | null;
  category: NodeCategory;
  actor: RowActor;
  /** True when this row reflects Sentinel actively intervening
   *  (deny / inject / force_stop) rather than passively observing.
   *  Interventions sticky-pin to the top of the ticker with an
   *  amber pulse — same severity tier as STUCK rows, but visually
   *  distinct so the operator can tell at a glance which is which. */
  isIntervention: boolean;
  members: TickerMember[];
}

const INTERVENTION_OUTCOMES = new Set([
  "deny",
  "denied",
  "inject",
  "injected",
  "force_stop",
  "block",
  "blocked",
]);

/// Decide who originated an event, given the fields we already
/// receive from the bridge. Exported for unit-testing — keep the
/// rules tight; if they get fuzzier than this, the right move is
/// to add a discriminator field at the bridge layer.
export function deriveActor(
  eventType: string,
  sentinelEvent: string,
  outcome: string | null,
): RowActor {
  if (sentinelEvent === "UserPromptSubmit") return "user";
  if (eventType.includes("hook")) return "sentinel";
  if (outcome && INTERVENTION_OUTCOMES.has(outcome)) return "sentinel";
  return "claude";
}

export function isInterventionOutcome(outcome: string | null): boolean {
  return outcome != null && INTERVENTION_OUTCOMES.has(outcome);
}

/// Translate the bridge's lifecycle-event name into a phrase an
/// operator can read at a glance. Unknown values fall through
/// unchanged so we don't accidentally hide new lifecycle events.
export function sentinelEventPhrase(sentinelEvent: string): string {
  switch (sentinelEvent) {
    case "PreToolUse":
      return "about to run";
    case "PostToolUse":
      return "finished";
    case "UserPromptSubmit":
      return "you submitted";
    case "Stop":
      return "stopped";
    case "Notification":
      return "notified";
    case "SubagentStop":
      return "subagent stopped";
    case "PreCompact":
      return "compacting";
    default:
      return sentinelEvent || "event";
  }
}

const ACTOR_GLYPH: Record<RowActor, string> = {
  // White diamond — neutral, agent doing things. Picked over `▸`
  // because `▸` is already used for the ×N expand toggle.
  claude: "◇",
  // Gear — control-plane vibe; the Sentinel logo language.
  sentinel: "⚙",
  // Return arrow — universal submit/enter glyph; matches user
  // intent ("you typed something").
  user: "↩",
};

const ACTOR_LABEL: Record<RowActor, string> = {
  claude: "agent (Claude)",
  sentinel: "control plane (Sentinel)",
  user: "operator (you)",
};

export function EventTicker({ events, onSelectNode, sessionColors, stuckMeta }: Props) {
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  // PERF: `now` used to live here and trigger a full list re-render
  // every 5 seconds purely to roll relative timestamps forward. The
  // re-render was visible during quiet periods (single-frame jank
  // every 5s, plus full reconciliation across hundreds of rows).
  // Each timestamp is now isolated in <TimeAgo> below — it owns its
  // own 5s ticker and only re-renders that one <span>.

  // Subscribe to the activity-cache so richer labels appear as the
  // inspector pulls JSONL detail. Bumping `cacheTick` triggers a
  // re-render; we don't need the value itself.
  const [cacheTick, setCacheTick] = useState(0);
  useEffect(() => subscribeActivityCache(() => setCacheTick((n) => n + 1)), []);

  const rows = useMemo(() => buildRows(events), [events]);
  // Augment each row from the activity cache (cheap O(rows) lookup).
  // Recompute on cache updates via the cacheTick dep.
  const augmentedRows = useMemo(
    () =>
      rows.map((r) => {
        if (!r.sessionId) return r;
        const lookupTool = r.label === "user prompt" ? "" : r.label;
        if (!lookupTool) return r;
        const tc = lookupActivityCache(r.sessionId, lookupTool, r.ts);
        if (!tc || !tc.summary) return r;
        const trimmed = tc.summary.length > 80 ? `${tc.summary.slice(0, 78)}…` : tc.summary;
        return { ...r, augment: trimmed };
      }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [rows, cacheTick],
  );

  // Sticky-stuck: float the freshest event per stuck session to the
  // top of the list, then the rest in normal newest-first order.
  // The pinned rows render with a pulsing red border to make them
  // unmissable.
  // Two-tier pin:
  //   Tier 1 — stuck-row (red pulse): one per stuck session, freshest event.
  //   Tier 2 — intervention-row (amber pulse): Sentinel actually
  //            denied / injected / blocked / force-stopped. These are
  //            the "operator MUST notice this" rows — same severity
  //            class as stuck, but distinguishable by colour.
  // pinnedKeys = stuck only (carries the existing stuck-row class).
  // interventionKeys = intervention only (carries the new
  //                     intervention-row class). The two sets are
  //                     mutually exclusive — if a row is both, stuck
  //                     wins so the operator sees the stronger signal
  //                     once instead of two highlights stacking.
  const { orderedRows, pinnedKeys, interventionKeys } = useMemo(() => {
    const stuckSet = stuckMeta && stuckMeta.size > 0 ? stuckMeta : null;
    const seenStuckSid = new Set<string>();
    const seenInterventionSid = new Set<string>();
    const stuckPin: typeof augmentedRows = [];
    const intervPin: typeof augmentedRows = [];
    const rest: typeof augmentedRows = [];
    const pinnedKeys = new Set<string>();
    const interventionKeys = new Set<string>();

    for (const r of augmentedRows) {
      const isStuck = !!(stuckSet && r.sessionId && stuckSet.has(r.sessionId) && !seenStuckSid.has(r.sessionId));
      if (isStuck && r.sessionId) {
        seenStuckSid.add(r.sessionId);
        pinnedKeys.add(r.key);
        stuckPin.push(r);
        continue;
      }
      // Intervention rows: pin one per session per intervention-batch
      // (de-dup by sid to keep the head of the ticker tight when one
      // session generates many denies in a row — the freshest one is
      // still informative). Skip if already stuck-pinned above.
      if (r.isIntervention && r.sessionId && !seenInterventionSid.has(r.sessionId)) {
        seenInterventionSid.add(r.sessionId);
        interventionKeys.add(r.key);
        intervPin.push(r);
        continue;
      }
      rest.push(r);
    }
    return {
      orderedRows: [...stuckPin, ...intervPin, ...rest],
      pinnedKeys,
      interventionKeys,
    };
  }, [augmentedRows, stuckMeta]);

  function toggle(key: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }

  return (
    <aside
      data-testid="event-ticker"
      className="flex flex-col h-full w-[360px] border-l border-[#30363d] bg-[#0d1117] text-[#c9d1d9] text-xs font-mono"
    >
      <header className="px-3 py-2 border-b border-[#30363d] uppercase tracking-wider text-[10px] text-[#6e7681] flex justify-between items-baseline gap-2">
        <span>events</span>
        <span
          className="text-[9px] tracking-normal normal-case flex gap-2"
          data-testid="actor-legend"
          title="event origin: agent (Claude) / control plane (Sentinel) / operator (you)"
        >
          <span style={{ color: "#6e7681" }}>◇ agent</span>
          <span style={{ color: "#d29922" }}>⚙ sentinel</span>
          <span style={{ color: "#58a6ff" }}>↩ user</span>
        </span>
        <span>{rows.length} rows</span>
      </header>
      <ul className="overflow-y-auto flex-1" data-testid="ticker-rows">
        {orderedRows.map((row) => {
          const isOpen = expanded.has(row.key);
          const focus = () => {
            if (row.toolCallId) onSelectNode(row.toolCallId, row.ts);
            else if (row.sessionId) onSelectNode(`SentinelSession#${row.sessionId}`, row.ts);
          };
          const sessionColor = row.sessionId && sessionColors
            ? sessionColors.get(row.sessionId) ?? null
            : null;
          return (
            <li
              key={row.key}
              data-actor={row.actor}
              data-session-id={row.sessionId ?? undefined}
              data-intervention={row.isIntervention ? "true" : undefined}
              data-stuck={pinnedKeys.has(row.key) ? "true" : undefined}
              className={`pl-0 pr-3 py-1 border-b border-[#21262d] hover:bg-[#1f6feb22] flex ${
                pinnedKeys.has(row.key)
                  ? "stuck-row"
                  : interventionKeys.has(row.key)
                    ? "intervention-row"
                    : ""
              }`}
            >
              {/* Session-color tab — 4px wide, full row height, matches
                  the same color as that session's node in the graph. */}
              <span
                className="shrink-0 self-stretch"
                style={{
                  width: "4px",
                  backgroundColor: sessionColor ?? "#21262d",
                  marginRight: "8px",
                  borderLeft: row.outcome === "deny" || row.outcome === "denied" ? "2px solid #f85149" : undefined,
                }}
                title={sessionColor ? `session ${row.sessionId?.slice(0, 8)}` : ""}
              />
              <div className="flex-1 min-w-0">
              <div
                role="button"
                tabIndex={0}
                className="flex gap-2 items-baseline cursor-pointer"
                onClick={focus}
                onKeyDown={(e) => {
                  if (e.target !== e.currentTarget) return;
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    focus();
                  }
                }}
              >
                {/* Actor glyph — disambiguates "who initiated this
                    event" at a glance: ◇ Claude, ⚙ Sentinel, ↩ user.
                    Fixed-width column so labels still line up.
                    Sentinel rows get a touch of amber so they stand
                    out without needing a new palette tier. */}
                <span
                  data-testid="actor-glyph"
                  className="inline-block w-3 text-center text-[11px] shrink-0 leading-none"
                  style={{
                    color:
                      row.actor === "sentinel"
                        ? "#d29922"
                        : row.actor === "user"
                          ? "#58a6ff"
                          : "#6e7681",
                  }}
                  title={ACTOR_LABEL[row.actor]}
                >
                  {ACTOR_GLYPH[row.actor]}
                </span>
                <span
                  className="inline-block w-2 h-2 rounded-full shrink-0"
                  style={{ backgroundColor: categoryColor(row.category) }}
                  title={categoryLabel(row.category)}
                />
                <TimeAgo ts={row.ts} className="text-[#6e7681] text-[10px] whitespace-nowrap" />
                {row.members.length > 1 ? (
                  <button
                    type="button"
                    onClick={(e) => { e.stopPropagation(); toggle(row.key); }}
                    className="px-1 rounded bg-[#21262d] text-[#58a6ff] text-[10px] hover:bg-[#30363d]"
                    title="show grouped members"
                  >
                    ×{row.members.length} {isOpen ? "▾" : "▸"}
                  </button>
                ) : null}
                <span className="truncate flex-1">
                  {row.label}
                  {row.augment ? (
                    <span className="text-[#6e7681] ml-1">· {row.augment}</span>
                  ) : null}
                </span>
              </div>
              <div className="text-[10px] text-[#6e7681] truncate pl-4">
                {/* Operator-friendly status text. The session-color
                    tab already encodes which session this is — no
                    need to repeat the sid prefix. The sentinel_event
                    name is internal jargon (PreToolUse, PostToolUse,
                    Stop, UserPromptSubmit); translate it. Falls
                    through to the raw event name for anything we
                    haven't covered. */}
                {sentinelEventPhrase(row.sentinelEvent)}
                {row.outcome ? ` · ${row.outcome}` : ""}
              </div>
              {pinnedKeys.has(row.key) && row.sessionId && stuckMeta?.get(row.sessionId) ? (
                <StuckReasonLine meta={stuckMeta.get(row.sessionId)!} />
              ) : null}
              {isOpen ? (
                <ul className="mt-1 pl-4 border-l border-dashed border-[#30363d]">
                  {row.members.map((m, i) => (
                    <li
                      key={`${row.key}-m-${i}`}
                      role={m.toolCallId ? "button" : undefined}
                      tabIndex={m.toolCallId ? 0 : undefined}
                      onClick={() => m.toolCallId && onSelectNode(m.toolCallId, m.ts)}
                      onKeyDown={(e) => {
                        if (!m.toolCallId) return;
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          onSelectNode(m.toolCallId, m.ts);
                        }
                      }}
                      className="py-0.5 text-[10px] text-[#c9d1d9] hover:text-[#58a6ff] cursor-pointer"
                    >
                      <TimeAgo ts={m.ts} className="text-[#6e7681] mr-2" />
                      {m.toolCallId ? m.toolCallId.replace("SentinelToolCall#", "TC#") : "(no tc id)"}
                    </li>
                  ))}
                </ul>
              ) : null}
              </div>
            </li>
          );
        })}
      </ul>
    </aside>
  );
}

/// Leaf timestamp — owns its own 5s ticker so refreshing the visible
/// "5m ago" doesn't re-render any ancestor. ONLY this <span>
/// reconciles. The chosen 5s cadence is a compromise: fast enough to
/// keep "30s ago" feeling live, slow enough to be free.
const TimeAgo = memo(function TimeAgo({ ts, className }: { ts: string; className?: string }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 5_000);
    return () => window.clearInterval(id);
  }, []);
  return <span className={className}>{tickerTime(ts, now)}</span>;
});

function StuckReasonLine({ meta }: { meta: StuckMeta }) {
  const ageLabel = formatAge(meta.ageSecs);
  const kindLabel = meta.kind ?? "awaiting";
  const question = meta.question
    ? meta.question.length > 90
      ? `${meta.question.slice(0, 88)}…`
      : meta.question
    : null;
  return (
    <div
      data-testid="stuck-reason-line"
      className="pl-4 mt-0.5 text-[10px] font-bold text-[#f85149] truncate"
      title={meta.question ?? kindLabel}
    >
      ⚠ STUCK {ageLabel} · {kindLabel}
      {question ? <span className="font-normal text-[#ffa198] ml-1">— {question}</span> : null}
    </div>
  );
}

function formatAge(secs: number): string {
  if (secs < 60) return `${Math.round(secs)}s`;
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  if (secs < 86400) return `${Math.round(secs / 3600)}h`;
  return `${Math.round(secs / 86400)}d`;
}

/// Group consecutive events sharing
/// `(session_id, sentinel_event, tool_call_id, outcome)` — matches the
/// signature the Python ticker uses (plan gotcha #9). Includes
/// timestamp fallback from payload (the SQL `timestamp` column is
/// empty for `sentinel.*` events) and label derivation that handles
/// UserPromptSubmit (which carries an empty `tool`).
function buildRows(events: RecentEvent[]): TickerRow[] {
  const rows: TickerRow[] = [];
  // Walk newest → oldest so the visible top is the freshest event.
  for (let i = events.length - 1; i >= 0; i--) {
    const e = events[i];
    const sid = strField(e.payload, "session_id");
    const tcid = strField(e.payload, "tool_call_id");
    const outcome = strField(e.payload, "outcome");
    const sentinelEvent = strField(e.payload, "sentinel_event") ?? e.type.replace(/^sentinel\./, "");
    const tool = strField(e.payload, "tool");
    const hook = strField(e.payload, "hook");
    const ts = bestTs(e);
    const { label, category } = deriveLabelAndCategory(e.type, sentinelEvent, tool, hook);
    // Grouping signature deliberately excludes `tool_call_id` — every
    // `sentinel.tool_call_observed` event has a unique tcid, so
    // including it would make `×N` flyouts unreachable. We still keep
    // tcid per member so each flyout entry remains clickable to the
    // specific node it represents.
    const sig = `${sid ?? ""}|${e.type}|${sentinelEvent}|${tool ?? ""}|${outcome ?? ""}`;
    const prev = rows[rows.length - 1];
    if (prev && prev.sig === sig) {
      prev.members.push({ ts, toolCallId: tcid, outcome });
      // Refresh the row's display ts to the freshest member so the
      // visible time keeps up.
      prev.ts = ts;
      continue;
    }
    rows.push({
      sig,
      key: `${sig}|${e.seq}`,
      ts,
      sessionId: sid,
      sentinelEvent,
      label,
      toolCallId: tcid,
      outcome,
      category,
      actor: deriveActor(e.type, sentinelEvent, outcome),
      isIntervention: isInterventionOutcome(outcome),
      members: [{ ts, toolCallId: tcid, outcome }],
    });
  }
  return rows;
}

function strField(p: Record<string, unknown>, k: string): string | null {
  const v = p[k];
  return typeof v === "string" && v.length > 0 ? v : null;
}

function bestTs(e: RecentEvent): string {
  const p = e.payload as Record<string, unknown>;
  const tsSec = typeof p.ts_sec === "string" ? p.ts_sec : null;
  const ts = typeof p.ts === "string" ? p.ts : null;
  return tsSec ?? ts ?? e.ts ?? "";
}

const TC_TOOLS = new Set(["Bash", "Read", "Write", "Edit", "Grep", "Glob", "NotebookEdit", "MultiEdit"]);
const PLANNING_TOOLS = new Set(["TaskCreate", "TaskUpdate", "TaskList", "TaskGet", "TaskStop", "TaskOutput", "WebFetch", "WebSearch", "Plan", "ExitPlanMode", "EnterPlanMode"]);
const COMMUNICATION_TOOLS = new Set(["Agent", "AskUserQuestion", "Stop", "ToolSearch"]);

function deriveLabelAndCategory(
  evType: string,
  sentinelEvent: string,
  tool: string | null,
  hook: string | null = null,
): { label: string; category: NodeCategory } {
  if (sentinelEvent === "UserPromptSubmit") {
    return { label: "user prompt", category: "prompt" };
  }
  if (tool && tool.length > 0) {
    let cat: NodeCategory = "other";
    if (TC_TOOLS.has(tool)) cat = "tc";
    else if (PLANNING_TOOLS.has(tool)) cat = "planning";
    else if (COMMUNICATION_TOOLS.has(tool)) cat = "communication";
    return { label: tool, category: cat };
  }
  // No tool — typically a hook event or a tool-less observation.
  // Hook name is more informative than the lifecycle event name, so
  // prefer it. Otherwise translate the lifecycle event into
  // operator phrasing so the row label is never raw jargon like
  // "PreToolUse".
  if (hook && hook.length > 0) {
    return { label: hook, category: "other" };
  }
  return {
    label: sentinelEventPhrase(sentinelEvent || evType.replace(/^sentinel\./, "")),
    category: "other",
  };
}
