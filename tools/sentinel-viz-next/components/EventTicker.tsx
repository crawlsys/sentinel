"use client";

import { memo, useEffect, useMemo, useState } from "react";

import type { NodeCategory, RecentEvent } from "../types/api";
import {
  lookup as lookupActivityCache,
  lookupUserPrompt,
  subscribe as subscribeActivityCache,
} from "../lib/activity-cache";
import { categoryColor, categoryLabel, tickerTime } from "../lib/format";
import {
  compactBashCommand,
  compactPath,
  formatGitDiffStats,
  parseGitDiffStats,
  smartTrunc,
  tildify,
} from "../lib/format-text";

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
  /** Specific tool the member event invoked (Bash, Read, …).
   *  Null for non-tool events. Used by the expand drawer so each
   *  member row in the ×N flyout can show its actual tool. */
  tool: string | null;
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
  /** Deduped list of tools observed across this row's members.
   *  Single-tool rows: ["Bash"]. Multi-tool rolled rows:
   *  ["Bash", "Read", "Edit"]. The rendered label uses this when
   *  it's >1 distinct tool (label = "Bash, Read, Edit"); single
   *  entry falls through to the original tool name + augment-cache
   *  summary. */
  tools: string[];
  members: TickerMember[];
}

const MAX_TOOLS_IN_LABEL = 4;

/// Render the label for a rolled-up row. Single-tool rows render
/// the tool name directly; multi-tool rows render up to N distinct
/// tools comma-separated, then "+M more". The label still fits in
/// the existing `truncate flex-1` slot — CSS handles the right-edge
/// ellipsis if needed.
function formatRolledLabel(tools: string[]): string {
  if (tools.length === 0) return "";
  if (tools.length === 1) return tools[0];
  if (tools.length <= MAX_TOOLS_IN_LABEL) return tools.join(", ");
  return `${tools.slice(0, MAX_TOOLS_IN_LABEL).join(", ")} +${tools.length - MAX_TOOLS_IN_LABEL} more`;
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
  // Rolled rows (multiple tools) don't get a single-tool augment —
  // the per-member flyout carries that detail. Singleton-tool rows
  // (tools.length === 1) get the augment-cache lookup; we
  // compact-format based on the tool kind so paths get tildified
  // and bash chains get cd-stripped.
  const augmentedRows = useMemo(
    () =>
      rows.map((r) => {
        if (!r.sessionId) return r;
        // User-prompt rows: pull the actual prompt text from the
        // separate promptCache. P3-27: operator screenshot showed
        // a tail of bare "user prompt" rows with no content because
        // we only indexed tool_calls.
        if (r.actor === "user") {
          const prompt = lookupUserPrompt(r.sessionId, r.ts);
          if (prompt) {
            const compact = tildify(prompt).replace(/\s+/g, " ").trim();
            const truncated = compact.length > 80 ? `${compact.slice(0, 78)}…` : compact;
            return { ...r, augment: truncated };
          }
          return r;
        }
        if (r.tools.length > 1) return r; // rolled row — no augment
        const lookupTool = r.tools[0] ?? "";
        if (!lookupTool) return r;
        const tc = lookupActivityCache(r.sessionId, lookupTool, r.ts);
        if (!tc || !tc.summary) return r;
        return { ...r, augment: compactSummaryFor(lookupTool, tc.summary) };
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
      className="flex flex-col h-full w-full md:w-[360px] border-t md:border-t-0 md:border-l border-[#30363d] bg-[#0d1117] text-[#c9d1d9] text-xs font-mono"
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
        {/* Skeleton rows on cold load — operator sees STRUCTURE
            immediately instead of an empty 360px column. Each
            skeleton mirrors the height + color-tab + glyph slot of
            a real row so the layout doesn't jump when real data
            arrives. data-testid="ticker-skeleton" keeps it easy to
            assert "loading → loaded" transitions in Playwright. */}
        {orderedRows.length === 0
          ? Array.from({ length: 8 }).map((_, i) => (
              <li
                key={`skel-${i}`}
                data-testid="ticker-skeleton"
                className="pl-0 pr-3 py-1 border-b border-[#21262d] flex animate-pulse"
                style={{ opacity: Math.max(0.15, 1 - i * 0.1) }}
              >
                <span
                  className="shrink-0 self-stretch"
                  style={{ width: "4px", backgroundColor: "#21262d", marginRight: "8px" }}
                />
                <div className="flex-1 min-w-0 flex items-baseline gap-2 py-0.5">
                  <span className="w-3 h-3 rounded-sm bg-[#21262d]" />
                  <span className="w-2 h-2 rounded-full bg-[#21262d]" />
                  <span className="w-8 h-2 rounded bg-[#21262d]" />
                  <span className="flex-1 h-2 rounded bg-[#21262d]" />
                </div>
              </li>
            ))
          : null}
        {orderedRows.map((row) => {
          const isOpen = expanded.has(row.key);
          const isRolled = row.members.length > 1;
          const focus = () => {
            // Single-click on the row body does BOTH things for
            // rolled rows: selects the underlying node AND toggles
            // the flyout open (or closed). For singletons it just
            // selects. The ×N badge keeps stopPropagation so
            // clicking it directly only toggles (no node selection
            // change). Operator screenshot: "when any of these are
            // clicked on they should fly out any folds
            // automatically (dont require the fold specifically)."
            if (isRolled) toggle(row.key);
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
              className={`pl-0 pr-3 py-0.5 border-b border-[#21262d] hover:bg-[#1f6feb22] flex ${
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
              {/* Click target spans the header + sub-line + stuck-
                  reason + rolled-preview. Operator screenshot:
                  "when any of these are clicked on they should fly
                  out any folds automatically (dont require the fold
                  specifically)." The flyout itself is rendered
                  OUTSIDE this clickable region so clicks on flyout
                  members don't collapse the parent. */}
              <div className="cursor-pointer" onClick={focus}>
              <div className="flex gap-2 items-baseline">
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
                    <span className="text-[#6e7681] ml-1 text-[10px]" title={row.augment}>
                      · {row.augment}
                    </span>
                  ) : null}
                </span>
              </div>
              {/* Sub-line gating: render ONLY when there's signal —
                  an outcome (intervention or error), or an unusual
                  event we don't have a friendly label for. Routine
                  PreToolUse rows (99% of traffic) used to read
                  "about to run" forever; that just duplicated the
                  tool name in the label. Hide it on the routine
                  case to drop visual noise. The
                  shouldShowSubLine() helper is exported so the
                  rule is testable. */}
              {shouldShowSubLine(row) ? (
                <div className="text-[10px] text-[#6e7681] truncate pl-4">
                  {subLineText(row)}
                </div>
              ) : null}
              {pinnedKeys.has(row.key) && row.sessionId && stuckMeta?.get(row.sessionId) ? (
                <StuckReasonLine meta={stuckMeta.get(row.sessionId)!} />
              ) : null}
              {/* Rolled rows: render 2-3 inline preview lines so the
                  operator sees WHAT was rolled up without expanding
                  the flyout. Guarded on tools.length > 1 (MULTI-tool
                  soft-rolled rows ONLY) so single-tool strict-sig
                  collapsed rows like `×5 Bash` keep the 1-line
                  rendering — the augment cache already gives them
                  inline context via the label, an inline preview
                  list would just duplicate that. */}
              {row.tools.length > 1 && row.sessionId && !isOpen ? (
                <RolledPreview row={row} />
              ) : null}
              </div>{/* /click-target */}
              {isOpen ? (
                <ul
                  className="mt-1 pl-4 border-l border-dashed border-[#30363d]"
                  data-testid="ticker-flyout"
                >
                  {row.members.map((m, i) => (
                    <FlyoutMember
                      key={`${row.key}-m-${i}`}
                      sessionId={row.sessionId}
                      member={m}
                      onSelect={onSelectNode}
                    />
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

/// Tool-aware summary compaction. Different tools have different
/// data shapes:
///   - Bash: chained "cd …; cmd" → strip cd, tildify, smart-trunc
///   - Read/Write/Edit/NotebookEdit/Glob: file paths → tildify,
///     end-truncate so the filename stays visible
///   - Other: generic smart-truncate + tildify
///
/// Output is the string that gets appended to the row label as
/// "Bash · <compact>". Kept under 80 chars by default — the 360px
/// column at ~10px mono fits roughly that.
export function compactSummaryFor(tool: string, summary: string): string {
  if (!summary) return "";
  if (tool === "Bash") return compactBashCommand(summary, 80);
  if (
    tool === "Read" ||
    tool === "Write" ||
    tool === "Edit" ||
    tool === "MultiEdit" ||
    tool === "NotebookEdit" ||
    tool === "Glob"
  ) {
    return compactPath(summary, 80);
  }
  return smartTrunc(tildify(summary), 80);
}

/// Inline preview lines under a rolled-up row. Operator screenshot:
///   "the individual items here could be 2 or 3x as high, with more
///    information (dense) to help the user understand what all was
///    executed within that rollup"
/// Surface 2-3 DISTINCT command/path previews from the row's
/// members without requiring the operator to expand the flyout.
/// Dedupe by (tool + summary) so 5 identical Bashes don't take
/// 3 lines; show variety instead.
const PREVIEW_LINES = 3;

function RolledPreview({ row }: { row: TickerRow }) {
  const sessionId = row.sessionId;
  if (!sessionId) return null;
  // Walk members newest → oldest, look up each in the activity-
  // cache, dedupe on (tool, summary) so identical adjacent calls
  // count once. Stop at PREVIEW_LINES distinct entries.
  const seen = new Set<string>();
  const lines: Array<{ tool: string; summary: string; isError: boolean }> = [];
  for (const m of row.members) {
    if (!m.tool) continue;
    const tc = lookupActivityCache(sessionId, m.tool, m.ts);
    const summary = tc?.summary ? compactSummaryFor(m.tool, tc.summary) : null;
    const key = `${m.tool}\t${summary ?? ""}`;
    if (seen.has(key)) continue;
    seen.add(key);
    if (!summary) {
      // No cache hit yet — emit a tool-only placeholder so the
      // operator still sees "Bash" / "Edit" variety. The cache
      // will fill in on the next 8s refresh.
      lines.push({ tool: m.tool, summary: "", isError: false });
    } else {
      lines.push({ tool: m.tool, summary, isError: !!tc?.error });
    }
    if (lines.length >= PREVIEW_LINES) break;
  }
  if (lines.length === 0) return null;
  return (
    <ul
      data-testid="rolled-preview"
      className="mt-0.5 pl-3 space-y-0 text-[10px] leading-tight"
    >
      {lines.map((l, i) => (
        <li
          key={`prev-${i}`}
          className="flex gap-1.5 items-baseline truncate"
        >
          <span className="text-[#484f58] shrink-0 w-10 truncate">{l.tool}</span>
          <span
            className={`truncate ${l.isError ? "text-[#f85149]" : "text-[#8b949e]"}`}
            title={l.summary || l.tool}
          >
            {l.summary || <span className="text-[#484f58] italic">(loading…)</span>}
          </span>
        </li>
      ))}
    </ul>
  );
}

/// One row in the ×N flyout. Looks up the activity-cache for THIS
/// specific (sid, tool, ts) so each member shows what it actually
/// did — not just the tool name + tcid.
function FlyoutMember({
  sessionId,
  member,
  onSelect,
}: {
  sessionId: string | null;
  member: TickerMember;
  onSelect: (nodeId: string, eventTs?: string) => void;
}) {
  const tc = sessionId && member.tool
    ? lookupActivityCache(sessionId, member.tool, member.ts)
    : null;
  const summary = tc?.summary ? compactSummaryFor(member.tool ?? "", tc.summary) : null;
  const diffStats = tc?.result_preview ? parseGitDiffStats(tc.result_preview) : null;
  const err = !!tc?.error;
  return (
    <li
      onClick={() => member.toolCallId && onSelect(member.toolCallId, member.ts)}
      className="py-0.5 text-[10px] text-[#c9d1d9] hover:text-[#58a6ff] cursor-pointer flex gap-2 items-baseline"
    >
      <TimeAgo ts={member.ts} className="text-[#6e7681] shrink-0" />
      {member.tool ? (
        <span className="text-[#c9d1d9] shrink-0 w-12 truncate">{member.tool}</span>
      ) : null}
      <span
        className={`text-[9px] truncate flex-1 ${err ? "text-[#f85149]" : "text-[#8b949e]"}`}
        title={tc?.summary ?? undefined}
      >
        {summary ?? (
          <span className="text-[#484f58]">
            {member.toolCallId ? member.toolCallId.replace("SentinelToolCall#", "TC#") : ""}
          </span>
        )}
      </span>
      {diffStats ? (
        <span
          data-testid="flyout-diff-stats"
          className="text-[9px] shrink-0 px-1 rounded bg-[#21262d] border border-[#30363d]"
          title={`${diffStats.insertions} insertions, ${diffStats.deletions} deletions, ${diffStats.files} files`}
        >
          <span className="text-[#3fb950]">+{diffStats.insertions}</span>
          <span className="text-[#6e7681]">/</span>
          <span className="text-[#f85149]">-{diffStats.deletions}</span>
        </span>
      ) : null}
    </li>
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

/// Show a sub-line ONLY when it adds info. Routine PreToolUse rows
/// would otherwise read "about to run" — useless duplication of the
/// row label. Real signal: an outcome (intervention/error), the
/// fall-through case where sentinel_event is unknown (so the
/// translation didn't kick in), or a user prompt where the actor
/// helps distinguish from a system event with the same label.
///
/// Exported for testing — the rule is small but central to keeping
/// the ticker readable; lock it down so it can't silently regress.
export function shouldShowSubLine(row: {
  outcome: string | null;
  sentinelEvent: string;
  actor: RowActor;
}): boolean {
  // Real signal: any outcome the operator should see.
  if (row.outcome && row.outcome.length > 0) return true;
  // Unknown sentinel events — sentinelEventPhrase falls through so
  // the operator sees the raw event name (better than hiding it).
  if (row.sentinelEvent && !KNOWN_SENTINEL_EVENTS.has(row.sentinelEvent)) return true;
  // User prompts already show "user prompt" as their LABEL — the
  // glyph + label combo carries the actor info. The sub-line would
  // just say "you submitted" which is the same fact twice.
  return false;
}

const KNOWN_SENTINEL_EVENTS = new Set([
  "PreToolUse",
  "PostToolUse",
  "UserPromptSubmit",
  "Stop",
  "Notification",
  "SubagentStop",
  "PreCompact",
]);

/// Subset of sentinel events that are tool-lifecycle bookkeeping:
/// when one of these arrives with NO tool / hook / outcome, it's
/// the bridge failing to extract the tool name — pure noise to the
/// operator. Used by buildRows to drop those rows.
const KNOWN_TOOL_LIFECYCLE = new Set([
  "PreToolUse",
  "PostToolUse",
]);

/// Sub-line text. Only called when shouldShowSubLine() returns
/// true, so we always have signal to render. Format:
///   <translated event> [· <outcome>]
function subLineText(row: { sentinelEvent: string; outcome: string | null }): string {
  const base = sentinelEventPhrase(row.sentinelEvent);
  return row.outcome ? `${base} · ${row.outcome}` : base;
}

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
    // P3-22 cleanup: skip "informationless" events entirely. The
    // bridge sometimes emits PreToolUse / PostToolUse rows with no
    // tool, no hook, and no outcome — we literally don't know
    // what happened. On a real DB this was ~60% of traffic and
    // produced "×N about to run" rolled rows that dominated the
    // ticker. Bridge fix is the proper home; until then, hide
    // the noise so the ticker shows actual operator signal.
    if (
      !tool &&
      !hook &&
      !outcome &&
      sentinelEvent !== "UserPromptSubmit" &&
      KNOWN_TOOL_LIFECYCLE.has(sentinelEvent)
    ) {
      continue;
    }
    const ts = bestTs(e);
    const { label, category } = deriveLabelAndCategory(e.type, sentinelEvent, tool, hook);
    // Grouping signature deliberately excludes `tool_call_id` — every
    // `sentinel.tool_call_observed` event has a unique tcid, so
    // including it would make `×N` flyouts unreachable. We still keep
    // tcid per member so each flyout entry remains clickable to the
    // specific node it represents.
    const sig = `${sid ?? ""}|${e.type}|${sentinelEvent}|${tool ?? ""}|${outcome ?? ""}`;
    const actor = deriveActor(e.type, sentinelEvent, outcome);
    const isIntervention = isInterventionOutcome(outcome);
    const prev = rows[rows.length - 1];

    // STRICT merge: adjacent events sharing the full sig (same
    // session + event type + tool + outcome). Preserves the
    // single-tool label so the augment-cache lookup still hits.
    // EXCEPTION: user prompts. Two adjacent UserPromptSubmit
    // events share the same sig (no tool, no outcome) but almost
    // always have different content — collapsing would silently
    // hide one of the operator's prompts. Keep them distinct.
    if (prev && prev.sig === sig && sentinelEvent !== "UserPromptSubmit") {
      prev.members.push({ ts, toolCallId: tcid, outcome, tool });
      prev.ts = ts;
      continue;
    }

    // SOFT merge (P3-24): collapse adjacent ROUTINE claude
    // tool-call activity in the same session+category into a
    // single rolled row. "Routine" = no outcome, not an
    // intervention, actor=claude, not a user prompt. This is the
    // big win — a typical session does 10 Bash + 3 Read + 2 Edit
    // in a burst; without this they're 15 separate rows and the
    // operator can't see which sessions are active. After: one
    // row "Bash, Read, Edit ×15" they can expand on demand.
    if (
      prev &&
      !outcome &&
      !isIntervention &&
      actor === "claude" &&
      prev.actor === "claude" &&
      !prev.outcome &&
      !prev.isIntervention &&
      category !== "prompt" &&
      prev.category !== "prompt" &&
      sid &&
      prev.sessionId === sid &&
      category === prev.category &&
      tool
    ) {
      prev.members.push({ ts, toolCallId: tcid, outcome, tool });
      if (!prev.tools.includes(tool)) prev.tools.push(tool);
      prev.label = formatRolledLabel(prev.tools);
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
      actor,
      isIntervention,
      tools: tool ? [tool] : [],
      members: [{ ts, toolCallId: tcid, outcome, tool }],
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
