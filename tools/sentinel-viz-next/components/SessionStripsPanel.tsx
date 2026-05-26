"use client";

import { useEffect, useMemo, useState } from "react";
import { ToggleButton, ToggleButtonGroup } from "@mui/material";

import type { GraphResponse, Node } from "../types/api";
import { buildSessionStrips } from "../domain/session-strips";
import { sessionColorMap } from "../domain/session-colors";
import { fetchSessionName } from "../adapters/http";
import { SessionStrip } from "./SessionStrip";

interface Props {
  graph: GraphResponse | null;
  stuck: Node[];
  selectedSessionId: string | null;
  onSelectSession: (sessionId: string | null) => void;
  /** Sessions whose node status is dead/long-dormant. Same set
   *  consumed by EventTicker. Filtered out of the strip list so
   *  the operator's screen isn't padded with corpses. Stuck
   *  sessions in this set are still surfaced (they're stuck, not
   *  dead) because buildSessionStrips checks stuck-map separately. */
  dormantSessionIds?: Set<string>;
}

const WINDOW_OPTIONS: Array<{ minutes: number; label: string }> = [
  { minutes: 15, label: "15m" },
  { minutes: 30, label: "30m" },
  { minutes: 60, label: "1h" },
  { minutes: 180, label: "3h" },
  { minutes: 360, label: "6h" },
];
const DEFAULT_WINDOW = 60;
const WINDOW_STORAGE_KEY = "sentinel-viz-strips-window-minutes";

/// Replaces the force-directed GraphCanvas (P3-31).
/// Operator: "1/2/3 yes yes yes" — replace the graph entirely with
/// per-session multi-sparkline strips; configurable window, 1h
/// default. The graph's 300-node spaghetti was obscuring the
/// signal we actually wanted — per-session rhythm of work across
/// time.
export function SessionStripsPanel({
  graph,
  stuck,
  selectedSessionId,
  onSelectSession,
  dormantSessionIds,
}: Props) {
  const [windowMinutes, setWindowMinutes] = useState<number>(() => {
    if (typeof window === "undefined") return DEFAULT_WINDOW;
    const stored = window.localStorage.getItem(WINDOW_STORAGE_KEY);
    const n = stored ? parseInt(stored, 10) : NaN;
    return Number.isFinite(n) && n > 0 ? n : DEFAULT_WINDOW;
  });

  useEffect(() => {
    if (typeof window === "undefined") return;
    window.localStorage.setItem(WINDOW_STORAGE_KEY, String(windowMinutes));
  }, [windowMinutes]);

  const sessionColors = useMemo(() => sessionColorMap(graph), [graph]);

  // Fetch LLM-assigned session names lazily for visible sids.
  // Cache lives in the api-side fetch (httpcache) so this is
  // cheap on re-renders.
  const [nameMap, setNameMap] = useState<Map<string, string>>(new Map());
  useEffect(() => {
    if (!graph) return;
    const sids = new Set<string>();
    for (const e of graph.events) {
      const sid = typeof e.payload?.session_id === "string" ? (e.payload.session_id as string) : null;
      if (sid) sids.add(sid);
    }
    let cancelled = false;
    (async () => {
      const next = new Map(nameMap);
      const sidsToFetch = Array.from(sids).filter((s) => !next.has(s));
      if (sidsToFetch.length === 0) return;
      const results = await Promise.allSettled(
        sidsToFetch.map((sid) => fetchSessionName(sid).then((r) => ({ sid, r }))),
      );
      if (cancelled) return;
      let changed = false;
      for (const r of results) {
        if (r.status !== "fulfilled") continue;
        const { sid, r: resp } = r.value;
        if (resp.name && resp.name.length > 0) {
          next.set(sid, resp.name);
          changed = true;
        }
      }
      if (changed) setNameMap(next);
    })();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graph?.max_seq]);

  const stuckMap = useMemo(() => {
    const m = new Map<string, { ageSecs: number; kind: string | null; question: string | null }>();
    for (const n of stuck) {
      const sid = n.data?.session_id;
      if (typeof sid !== "string") continue;
      m.set(sid, {
        ageSecs: n.last_activity_age_s ?? 0,
        kind: n.awaiting_kind ?? null,
        question: n.awaiting_question ?? null,
      });
    }
    return m;
  }, [stuck]);

  const strips = useMemo(() => {
    const all = buildSessionStrips(graph, {
      windowMinutes,
      colors: sessionColors,
      names: nameMap,
      stuck: stuckMap,
    });
    if (!dormantSessionIds || dormantSessionIds.size === 0) return all;
    // Hide dead/long-dormant sessions UNLESS they're stuck — a
    // stuck session is signal even when "old" by the bridge's
    // dormancy clock.
    return all.filter((s) => !dormantSessionIds.has(s.sessionId) || s.stuck);
  }, [graph, windowMinutes, sessionColors, nameMap, stuckMap, dormantSessionIds]);

  return (
    <section
      data-testid="session-strips-panel"
      className="flex flex-col h-full min-h-0 bg-[#000] text-[#E8E8E8] font-mono"
    >
      <header className="flex items-baseline gap-3 px-3 py-2 border-b border-[#222] text-[10px] uppercase tracking-wider text-[#999]">
        <span>sessions</span>
        <span className="text-[#666]">· last {labelForWindow(windowMinutes)}</span>
        <ToggleButtonGroup
          data-testid="strips-window-selector"
          value={windowMinutes}
          exclusive
          size="small"
          onChange={(_, v) => {
            if (typeof v === "number") setWindowMinutes(v);
          }}
          sx={{ ml: "auto" }}
        >
          {WINDOW_OPTIONS.map((opt) => (
            <ToggleButton
              key={opt.minutes}
              value={opt.minutes}
              data-testid={`window-${opt.minutes}m`}
              sx={{
                fontFamily: "var(--font-space-mono), monospace",
                fontSize: 9,
                letterSpacing: 0,
                textTransform: "lowercase",
                px: 1,
                py: 0.25,
                color: "var(--text-secondary)",
                borderColor: "var(--border)",
                "&.Mui-selected": {
                  color: "var(--info)",
                  borderColor: "var(--info)",
                  bgcolor: "rgba(91,155,246,0.12)",
                  "&:hover": { bgcolor: "rgba(91,155,246,0.18)" },
                },
                "&:hover": { color: "var(--text-primary)" },
              }}
            >
              {opt.label}
            </ToggleButton>
          ))}
        </ToggleButtonGroup>
      </header>
      {strips.length === 0 ? (
        <div
          data-testid="session-strips-empty"
          className="flex-1 flex items-center justify-center text-[#999] text-xs px-3"
        >
          no sessions with activity in the last {labelForWindow(windowMinutes)}
        </div>
      ) : (
        <ul
          data-testid="session-strips"
          className="overflow-y-auto flex-1"
        >
          {strips.map((d) => (
            <SessionStrip
              key={d.sessionId}
              data={d}
              selected={selectedSessionId === d.sessionId}
              onSelect={() =>
                onSelectSession(selectedSessionId === d.sessionId ? null : d.sessionId)
              }
            />
          ))}
        </ul>
      )}
    </section>
  );
}

function labelForWindow(minutes: number): string {
  if (minutes < 60) return `${minutes}m`;
  if (minutes % 60 === 0) return `${minutes / 60}h`;
  return `${(minutes / 60).toFixed(1)}h`;
}
