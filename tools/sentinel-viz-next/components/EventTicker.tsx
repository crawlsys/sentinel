"use client";

import { useEffect, useMemo, useState } from "react";

import type { NodeCategory, RecentEvent } from "../types/api";
import { lookup as lookupActivityCache, subscribe as subscribeActivityCache } from "../lib/activity-cache";
import { categoryColor, categoryLabel, tickerTime } from "../lib/format";

interface Props {
  events: RecentEvent[];
  onSelectNode: (nodeId: string, eventTs?: string) => void;
  /** sid → color. From session-colors.sessionColorMap(graph). */
  sessionColors?: Map<string, string>;
}

interface TickerMember {
  ts: string;
  toolCallId: string | null;
  outcome: string | null;
}

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
  members: TickerMember[];
}

export function EventTicker({ events, onSelectNode, sessionColors }: Props) {
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  // Force a 5s re-render so "30s ago" rolls forward in quiet periods.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 5_000);
    return () => window.clearInterval(id);
  }, []);

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
      <header className="px-3 py-2 border-b border-[#30363d] uppercase tracking-wider text-[10px] text-[#6e7681] flex justify-between">
        <span>events</span>
        <span>{rows.length} rows · {events.length} raw</span>
      </header>
      <ul className="overflow-y-auto flex-1" data-testid="ticker-rows">
        {augmentedRows.map((row) => {
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
              className="pl-0 pr-3 py-1 border-b border-[#21262d] hover:bg-[#1f6feb22] flex"
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
              <div className="flex gap-2 items-baseline cursor-pointer" onClick={focus}>
                <span
                  className="inline-block w-2 h-2 rounded-full shrink-0"
                  style={{ backgroundColor: categoryColor(row.category) }}
                  title={categoryLabel(row.category)}
                />
                <span className="text-[#6e7681] text-[10px] whitespace-nowrap">
                  {tickerTime(row.ts, now)}
                </span>
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
                {row.sessionId ? `s:${row.sessionId.slice(0, 8)}…` : ""} {row.sentinelEvent}
                {row.outcome ? ` · ${row.outcome}` : ""}
              </div>
              {isOpen ? (
                <ul className="mt-1 pl-4 border-l border-dashed border-[#30363d]">
                  {row.members.map((m, i) => (
                    <li
                      key={`${row.key}-m-${i}`}
                      onClick={() => m.toolCallId && onSelectNode(m.toolCallId, m.ts)}
                      className="py-0.5 text-[10px] text-[#c9d1d9] hover:text-[#58a6ff] cursor-pointer"
                    >
                      <span className="text-[#6e7681] mr-2">{tickerTime(m.ts, now)}</span>
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
    const ts = bestTs(e);
    const { label, category } = deriveLabelAndCategory(e.type, sentinelEvent, tool);
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
  return { label: sentinelEvent || evType.replace(/^sentinel\./, ""), category: "other" };
}
