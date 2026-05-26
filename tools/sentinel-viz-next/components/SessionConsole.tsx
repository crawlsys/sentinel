"use client";

import { useEffect, useMemo, useState } from "react";
import { Chip } from "@mui/material";
import ExpandMoreIcon from "@mui/icons-material/ExpandMoreRounded";
import ChevronRightIcon from "@mui/icons-material/ChevronRightRounded";

import { fetchActivity } from "../adapters/http";
import { indexActivity } from "../adapters/activity-cache";
import { colorForSession } from "../domain/session-colors";
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
/// Display limit ≠ fetch limit. SessionConsole only shows 5 (or 10)
/// segments but the EventTicker's flyouts can need 50+ tool calls'
/// worth of cached summaries. Decouple: pull deep, display shallow.
/// Bumped from 6 to 50 in P3-26 so heavy sessions' older rolled-
/// row members stop falling back to TC# stubs.
const CACHE_WARM_FETCH_LIMIT = 50;

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
    // Build a session_id → harness lookup so we can skip non-claude
    // sessions below — /api/activity reads claude transcript JSONLs
    // and 404s for codex/opencode/qwen/gemini. Firing it for every
    // visible non-claude session on every 8s tick spiked load times
    // measurably (5+ codex grinds running ≈ 5+ parallel doomed
    // requests every tick).
    const harnessBySid = new Map<string, string>();
    for (const n of graph.nodes) {
      if (n.type !== "SentinelSession") continue;
      const sid = typeof n.data?.session_id === "string" ? (n.data.session_id as string) : null;
      const h = typeof n.data?.source_harness === "string"
        ? (n.data.source_harness as string)
        : "";
      if (sid && h) harnessBySid.set(sid, h);
    }
    for (const e of graph.events) {
      const sid = typeof e.payload?.session_id === "string"
        ? (e.payload.session_id as string)
        : null;
      const h = typeof e.payload?.source_harness === "string"
        ? (e.payload.source_harness as string)
        : "";
      if (sid && h && !harnessBySid.has(sid)) harnessBySid.set(sid, h);
    }
    const isClaude = (sid: string) => {
      const h = harnessBySid.get(sid);
      return !h || h === "claude";
    };

    // Active sessions (have SentinelSession nodes) come first.
    for (const n of graph.nodes) {
      if (n.type !== "SentinelSession") continue;
      const sid = typeof n.data?.session_id === "string" ? (n.data.session_id as string) : null;
      if (sid && isClaude(sid) && !out.includes(sid)) out.push(sid);
    }
    // ALSO include sessions whose events appear in the window even
    // if they don't have a node — those are older sessions whose
    // user_input rows would otherwise stay content-less because we
    // wouldn't warm their activity cache. Without this, the operator
    // sees bare "user prompt" rows for dormant sessions even after
    // P3-27 added the prompt cache. Cap to avoid runaway fan-out
    // on huge windows.
    const MAX_BACKFILL_SIDS = 12;
    for (const e of graph.events) {
      if (out.length >= MAX_BACKFILL_SIDS) break;
      const sid =
        typeof e.payload?.session_id === "string"
          ? (e.payload.session_id as string)
          : null;
      if (sid && isClaude(sid) && !out.includes(sid)) out.push(sid);
    }
    return out;
  }, [graph]);

  const focused = selectedSessionId ?? null;
  const sessionIdsKey = focused ? `focus:${focused}` : visibleSessionIds.join(",");

  useEffect(() => {
    if (paused) return;
    let cancelled = false;
    const sessionIds = focused ? [focused] : visibleSessionIds;
    if (sessionIds.length === 0) {
      setEntries([]);
      return;
    }
    const maxEntries = focused ? MAX_ENTRIES_FOCUSED : MAX_ENTRIES_MERGED;
    // Always fetch DEEP (CACHE_WARM_FETCH_LIMIT) so the ticker's
    // rolled-row flyouts have cached summaries for older members.
    // Display still uses maxEntries for the live-log itself.
    const perSessionLimit = CACHE_WARM_FETCH_LIMIT;

    async function refresh() {
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
          // Warm the activity-cache so EventTicker rows for this
          // session can augment their labels with real tool args
          // (e.g. "Bash · gh run view 26197..."). Previously the
          // cache only filled when the operator opened the
          // inspector; rolls of identical "Bash"/"Read" labels
          // dominated the ticker. Now the live-log's 8s sweep
          // doubles as ambient warming for the ticker.
          indexActivity(sid, r);
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
    refresh();
    const id = window.setInterval(refresh, REFRESH_INTERVAL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionIdsKey, paused]);

  return (
    <div
      data-testid="session-console"
      // P3-37: hide on mobile (md and below) — the SessionStrips
      // panel already surfaces per-session AI summaries and
      // activity sparklines, so the live-log feed is largely
      // redundant on small screens AND eats prime vertical real
      // estate the strips need. Operator can still inspect any
      // individual event via the modal that opens on tap.
      className="hidden md:block border-t border-[#222] bg-[#000] font-mono text-xs"
      onMouseEnter={() => setPaused(true)}
      onMouseLeave={() => setPaused(false)}
    >
      <div
        className="flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:bg-[#111]"
        onClick={() => setOpen((o) => !o)}
      >
        {open ? (
          <ExpandMoreIcon sx={{ fontSize: 16, color: "var(--text-secondary)" }} />
        ) : (
          <ChevronRightIcon sx={{ fontSize: 16, color: "var(--text-secondary)" }} />
        )}
        <span
          className={`inline-block w-2 h-2 rounded-full ${
            paused ? "bg-[#999]" : loading ? "bg-[#D4A843]" : "bg-[#4A9E5C]"
          }`}
          style={{
            animation: paused || loading ? "none" : "pulse-dot 1.4s ease-in-out infinite",
          }}
        />
        <span className="text-[10px] uppercase tracking-wider text-[#5B9BF6]">
          live log
        </span>
        {focused ? (
          <Chip
            data-testid="session-console-scope"
            label={`scoped · s:${focused.slice(0, 8)}`}
            size="small"
            sx={{
              bgcolor: colorForSession(sessionColors, focused) + "22",
              color: colorForSession(sessionColors, focused),
              borderColor: colorForSession(sessionColors, focused),
              height: 20,
              fontSize: 10,
            }}
          />
        ) : (
          <span
            className="text-[10px] uppercase tracking-wider text-[#999]"
            data-testid="session-console-scope"
          >
            all sessions
          </span>
        )}
        <span className="text-[10px] text-[#999] ml-2">
          {entries.length} of last {focused ? MAX_ENTRIES_FOCUSED : MAX_ENTRIES_MERGED} ·{" "}
          {paused ? "paused (hover)" : "auto 8s"}
        </span>
      </div>
      {open ? (
        <ul className="overflow-y-auto px-3 py-2 space-y-1.5" style={{ maxHeight: "26vh" }}>
          {entries.length === 0 ? (
            <li className="text-[10px] text-[#999] italic">
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
                      <span className="text-[#999] whitespace-nowrap">
                        {timeShort(e.ts)} · s:{e.sessionId.slice(0, 8)}
                      </span>
                    </div>
                    <div
                      className={`text-[11px] whitespace-pre-wrap break-words leading-snug ${
                        e.hadError ? "text-[#D71921]" : "text-[#E8E8E8]"
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
