"use client";

import { useEffect, useMemo, useState } from "react";

import { fetchActivity } from "../lib/api";
import { colorForSession } from "../lib/session-colors";
import type { GraphResponse, Segment } from "../types/api";

interface Props {
  /** Latest graph snapshot — gives us the active session list. */
  graph: GraphResponse | null;
  /** sid → color, same mapping the ticker + galaxies use. */
  sessionColors: Map<string, string>;
  /** When set, the live log scopes to just this session and shows
   *  ~10 segments instead of merging across all sessions. Null/undef
   *  falls back to the cross-session merged view. */
  selectedSessionId?: string | null;
  defaultOpen?: boolean;
}

interface MergedEntry {
  sessionId: string;
  /// Label for the row (assistant_turn or "user input").
  label: string;
  /// One-line preview (already truncated by the API).
  preview: string;
  /// Tools mentioned in this segment, for visual hint.
  tools: string[];
  /// Whether the segment had_error.
  hadError: boolean;
  /// ISO timestamp.
  ts: string;
}

/// Live log = the last 5 activity segments across the visible
/// sessions, merged chronologically. No LLM call — just the
/// rollups the activity panel already produces. Refreshes every
/// 8s and on every graph change.
const REFRESH_INTERVAL_MS = 8_000;
const MAX_ENTRIES_MERGED = 5;
const MAX_ENTRIES_FOCUSED = 10;

export function SessionConsole({
  graph,
  sessionColors,
  selectedSessionId,
  defaultOpen = true,
}: Props) {
  const [entries, setEntries] = useState<MergedEntry[]>([]);
  const [open, setOpen] = useState<boolean>(defaultOpen);
  const [loading, setLoading] = useState<boolean>(false);
  const [paused, setPaused] = useState<boolean>(false);

  // Effect depends on the *string* selected sid (and a join of visible
  // sids), not on the whole graph object — otherwise every SSE tick
  // would tear down the interval. The refresh inside the effect can
  // still read the latest graph via the closure on each interval fire.
  const visibleSessionIds = useMemo(() => {
    if (!graph) return [] as string[];
    const out: string[] = [];
    for (const n of graph.nodes) {
      if (n.type !== "SentinelSession") continue;
      const sid = typeof n.data?.session_id === "string" ? (n.data.session_id as string) : null;
      if (sid && !out.includes(sid)) out.push(sid);
    }
    return out;
  }, [graph]);

  const focused = selectedSessionId ?? null;
  const sessionIdsKey = focused ? `focus:${focused}` : visibleSessionIds.join(",");

  useEffect(() => {
    if (paused) return;
    let cancelled = false;
    const sessionIds = focused ? [focused] : visibleSessionIds;
    const perSessionLimit = focused ? MAX_ENTRIES_FOCUSED : 6;
    const maxEntries = focused ? MAX_ENTRIES_FOCUSED : MAX_ENTRIES_MERGED;

    async function refresh() {
      // No visible sessions → clear and skip the fetch entirely (the
      // empty clear happens here, inside the async body, rather than
      // synchronously in the effect body — the latter triggers a
      // cascading-render lint error). Promise.allSettled([]) wouldn't
      // call fetchActivity anyway, but skipping avoids the loading flash.
      if (sessionIds.length === 0) {
        if (!cancelled) setEntries([]);
        return;
      }
      setLoading(true);
      try {
        const responses = await Promise.allSettled(
          sessionIds.map((sid) =>
            fetchActivity(sid, { limit: perSessionLimit }).then((r) => ({ sid, r })),
          ),
        );
        if (cancelled) return;
        const merged: MergedEntry[] = [];
        for (const res of responses) {
          if (res.status !== "fulfilled") continue;
          const { sid, r } = res.value;
          for (const s of r.segments) merged.push(segmentToEntry(sid, s));
        }
        const seen = new Set<string>();
        const sorted = merged
          .sort((a, b) => (a.ts < b.ts ? 1 : -1))
          .filter((e) => {
            const k = `${e.sessionId}|${e.ts}|${e.label}`;
            if (seen.has(k)) return false;
            seen.add(k);
            return true;
          })
          .slice(0, maxEntries);
        setEntries(sorted);
      } catch {
        /* silent */
      } finally {
        if (!cancelled) setLoading(false);
      }
    }
    void refresh();
    // Only poll while there's something to fetch.
    const id =
      sessionIds.length > 0 ? window.setInterval(refresh, REFRESH_INTERVAL_MS) : undefined;
    return () => {
      cancelled = true;
      if (id !== undefined) window.clearInterval(id);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionIdsKey, paused]);

  return (
    <div
      data-testid="session-console"
      className="border-t border-[#30363d] bg-[#0d1117] font-mono text-xs"
      onMouseEnter={() => setPaused(true)}
      onMouseLeave={() => setPaused(false)}
    >
      <button
        type="button"
        aria-expanded={open}
        className="w-full text-left flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:bg-[#161b22]"
        onClick={() => setOpen((o) => !o)}
      >
        <span className="text-[#6e7681]">{open ? "▼" : "▶"}</span>
        <span
          className={`inline-block w-2 h-2 rounded-full ${
            paused ? "bg-[#6e7681]" : loading ? "bg-[#d29922]" : "bg-[#3fb950]"
          }`}
          style={{
            animation: paused || loading ? "none" : "pulse-dot 1.4s ease-in-out infinite",
          }}
        />
        <span className="text-[10px] uppercase tracking-wider text-[#58a6ff]">
          live log
        </span>
        {focused ? (
          <span
            className="text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded"
            style={{
              backgroundColor: colorForSession(sessionColors, focused) + "22",
              color: colorForSession(sessionColors, focused),
              borderColor: colorForSession(sessionColors, focused),
              borderWidth: "1px",
            }}
            data-testid="session-console-scope"
          >
            scoped · s:{focused.slice(0, 8)}
          </span>
        ) : (
          <span
            className="text-[10px] uppercase tracking-wider text-[#6e7681]"
            data-testid="session-console-scope"
          >
            all sessions
          </span>
        )}
        <span className="text-[10px] text-[#6e7681] ml-2">
          {entries.length} of last {focused ? MAX_ENTRIES_FOCUSED : MAX_ENTRIES_MERGED} ·{" "}
          {paused ? "paused (hover)" : "auto 8s"}
        </span>
      </button>
      {open ? (
        <ul className="overflow-y-auto px-3 py-2 space-y-1.5" style={{ maxHeight: "26vh" }}>
          {entries.length === 0 ? (
            <li className="text-[10px] text-[#6e7681] italic">
              {loading ? "fetching segments…" : "no recent segments"}
            </li>
          ) : (
            entries.map((e, i) => {
              const color = colorForSession(sessionColors, e.sessionId);
              return (
                <li
                  key={`${e.sessionId}-${e.ts}-${i}`}
                  className="flex gap-2"
                >
                  <span
                    className="shrink-0 self-stretch"
                    style={{ width: "4px", backgroundColor: color }}
                  />
                  <div className="flex-1 min-w-0">
                    <div className="flex justify-between items-baseline gap-2 text-[10px] mb-0.5">
                      <span className="font-bold truncate" style={{ color }}>
                        {e.label}
                      </span>
                      <span className="text-[#6e7681] whitespace-nowrap">
                        {timeShort(e.ts)} · s:{e.sessionId.slice(0, 8)}
                      </span>
                    </div>
                    <div
                      className={`text-[11px] whitespace-pre-wrap break-words leading-snug ${
                        e.hadError ? "text-[#f85149]" : "text-[#c9d1d9]"
                      }`}
                    >
                      {e.preview || "(no preview)"}
                    </div>
                  </div>
                </li>
              );
            })
          )}
        </ul>
      ) : null}
    </div>
  );
}

function segmentToEntry(sessionId: string, s: Segment): MergedEntry {
  return {
    sessionId,
    label: s.kind === "user_input" ? "user input" : s.label,
    preview: s.preview ?? "",
    tools: s.tools ?? [],
    hadError: !!s.had_error,
    ts: s.ts_end ?? s.ts,
  };
}

function timeShort(ts: string): string {
  const m = /(\d{2}):(\d{2}):(\d{2})/.exec(ts);
  if (m) return `${m[1]}:${m[2]}:${m[3]}`;
  return "—";
}
