"use client";

import { useMemo, useState } from "react";

import type { NodeCategory, RecentEvent } from "../types/api";
import { categoryColor, categoryLabel, shortTime } from "../lib/format";

interface Props {
  events: RecentEvent[];
  onSelectNode: (nodeId: string) => void;
}

interface TickerMember {
  ts: string;
  toolCallId: string | null;
  outcome: string | null;
}

interface TickerRow {
  key: string;
  ts: string;
  sessionId: string | null;
  sentinelEvent: string;
  label: string;
  toolCallId: string | null;
  outcome: string | null;
  category: NodeCategory;
  members: TickerMember[];
}

export function EventTicker({ events, onSelectNode }: Props) {
  const rows = useMemo(() => buildRows(events), [events]);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

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
        {rows.map((row) => {
          const isOpen = expanded.has(row.key);
          const focus = () => {
            if (row.toolCallId) onSelectNode(row.toolCallId);
            else if (row.sessionId) onSelectNode(`SentinelSession#${row.sessionId}`);
          };
          return (
            <li
              key={row.key}
              className={`px-3 py-1 border-b border-[#21262d] hover:bg-[#1f6feb22] ${
                row.outcome === "deny" || row.outcome === "denied"
                  ? "border-l-2 border-l-[#f85149]"
                  : ""
              }`}
            >
              <div className="flex gap-2 items-baseline cursor-pointer" onClick={focus}>
                <span
                  className="inline-block w-2 h-2 rounded-full shrink-0"
                  style={{ backgroundColor: categoryColor(row.category) }}
                  title={categoryLabel(row.category)}
                />
                <span className="text-[#6e7681] text-[10px] whitespace-nowrap">
                  {shortTime(row.ts)}
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
                <span className="truncate flex-1">{row.label}</span>
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
                      onClick={() => m.toolCallId && onSelectNode(m.toolCallId)}
                      className="py-0.5 text-[10px] text-[#c9d1d9] hover:text-[#58a6ff] cursor-pointer"
                    >
                      <span className="text-[#6e7681] mr-2">{shortTime(m.ts)}</span>
                      {m.toolCallId ? m.toolCallId.replace("SentinelToolCall#", "TC#") : "(no tc id)"}
                    </li>
                  ))}
                </ul>
              ) : null}
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
    const sig = `${sid ?? ""}|${e.type}|${sentinelEvent}|${tcid ?? ""}|${outcome ?? ""}`;
    const prev = rows[rows.length - 1];
    if (
      prev
      && prev.key.split("|").slice(0, 5).join("|") === sig
    ) {
      prev.members.push({ ts, toolCallId: tcid, outcome });
      continue;
    }
    rows.push({
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
