"use client";

import { useMemo } from "react";

import type { RecentEvent } from "../types/api";
import { shortTime } from "../lib/format";

interface Props {
  events: RecentEvent[];
  onSelectNode: (nodeId: string) => void;
}

interface TickerRow {
  key: string;
  ts: string;
  sessionId: string | null;
  kind: string;
  toolCallId: string | null;
  outcome: string | null;
  summary: string;
  count: number;
}

export function EventTicker({ events, onSelectNode }: Props) {
  const rows = useMemo(() => buildRows(events), [events]);

  return (
    <aside
      data-testid="event-ticker"
      className="flex flex-col h-full w-[360px] border-l border-[#30363d] bg-[#0d1117] text-[#c9d1d9] text-xs font-mono"
    >
      <header className="px-3 py-2 border-b border-[#30363d] uppercase tracking-wider text-[10px] text-[#6e7681] flex justify-between">
        <span>events</span>
        <span>{rows.length}</span>
      </header>
      <ul className="overflow-y-auto flex-1" data-testid="ticker-rows">
        {rows.map((row) => (
          <li
            key={row.key}
            onClick={() => {
              if (row.toolCallId) onSelectNode(row.toolCallId);
              else if (row.sessionId) onSelectNode(`SentinelSession#${row.sessionId}`);
            }}
            className={`px-3 py-1 border-b border-[#21262d] cursor-pointer hover:bg-[#1f6feb22] ${
              row.outcome === "denied" ? "border-l-2 border-l-[#f85149]" : ""
            }`}
          >
            <div className="flex gap-2 items-baseline">
              <span className="text-[#6e7681] text-[10px] whitespace-nowrap">
                {shortTime(row.ts)}
              </span>
              {row.count > 1 ? (
                <span className="px-1 rounded bg-[#21262d] text-[#58a6ff] text-[10px]">
                  ×{row.count}
                </span>
              ) : null}
              <span className="truncate">{row.summary}</span>
            </div>
            <div className="text-[10px] text-[#6e7681] truncate">
              {row.sessionId ? `s:${row.sessionId.slice(0, 8)}…` : ""} {row.kind}
            </div>
          </li>
        ))}
      </ul>
    </aside>
  );
}

/// Group consecutive events sharing
/// `(session_id, sentinel_event, tool_call_id, outcome)` — matches the
/// signature the Python ticker uses (plan gotcha #9: tool_call_id must
/// be in the key, else distinct tool-calls collapse into one row).
function buildRows(events: RecentEvent[]): TickerRow[] {
  const rows: TickerRow[] = [];
  // Walk newest → oldest so the visible top is the freshest event.
  for (let i = events.length - 1; i >= 0; i--) {
    const e = events[i];
    const sid = strField(e.payload, "session_id");
    const tcid = strField(e.payload, "tool_call_id");
    const outcome = strField(e.payload, "outcome");
    const tool = strField(e.payload, "tool") ?? strField(e.payload, "hook_event") ?? e.type;
    const sig = `${sid ?? ""}|${e.type}|${tcid ?? ""}|${outcome ?? ""}`;
    const prev = rows[rows.length - 1];
    if (prev && prev.key.split("|").slice(0, 4).join("|") === sig) {
      prev.count += 1;
      continue;
    }
    rows.push({
      key: `${sig}|${e.seq}`,
      ts: e.ts,
      sessionId: sid,
      kind: e.type.replace(/^sentinel\./, ""),
      toolCallId: tcid,
      outcome,
      summary: tool ?? e.type,
      count: 1,
    });
  }
  return rows;
}

function strField(p: Record<string, unknown>, k: string): string | null {
  const v = p[k];
  return typeof v === "string" ? v : null;
}
