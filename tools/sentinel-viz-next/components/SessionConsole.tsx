"use client";

import { useEffect, useRef, useState } from "react";

import { fetchSummary, type SummaryResponse } from "../lib/api";
import { getCachedName } from "../lib/session-names";
import type { GraphResponse } from "../types/api";

interface Props {
  /** Most recent graph snapshot — we mine its `events` for narrative
   *  inputs and its `max_seq` for delta detection. */
  graph: GraphResponse | null;
  /** Open-by-default. Operator can collapse the panel via the header. */
  defaultOpen?: boolean;
}

interface ConsoleEntry {
  ts: number;
  text: string;
  sessionId: string;
  sessionLabel: string;
  source: string;
  /** Bumped by data updates so the flash animation re-fires when the
   *  same entry appears at the top after a re-render. */
  fresh: boolean;
  err: boolean;
}

// V1 throttle gates (viz_server.py `liveLogTick`).
const POLL_INTERVAL_MS = 5_000;       // tick frequency
const FIRE_INTERVAL_MS = 30_000;      // min seconds between LLM calls
const MIN_EVENT_DELTA = 4;            // need at least N new events
const MAX_ENTRIES = 20;               // keep the last N narratives

export function SessionConsole({ graph, defaultOpen = true }: Props) {
  const [entries, setEntries] = useState<ConsoleEntry[]>([]);
  const [open, setOpen] = useState<boolean>(defaultOpen);
  const [paused, setPaused] = useState<boolean>(false);
  const [inFlight, setInFlight] = useState<boolean>(false);
  const [lastFireMs, setLastFireMs] = useState<number>(0);
  const [sinceLabel, setSinceLabel] = useState<string>("");

  const lastSeqRef = useRef<number>(-1);
  const lastFireRef = useRef<number>(0);
  const hoverRef = useRef<boolean>(false);

  // Bootstrap lastSeq on first graph arrival so we don't summarise
  // the whole 600-event backlog on load.
  useEffect(() => {
    if (graph && lastSeqRef.current === -1) {
      lastSeqRef.current = graph.max_seq;
    }
  }, [graph]);

  async function fireTick(force = false) {
    if (!graph) return;
    if (inFlight) return;
    if (hoverRef.current && !force) return; // pause-on-hover
    const now = Date.now();
    if (!force && now - lastFireRef.current < FIRE_INTERVAL_MS) return;

    const max = graph.max_seq;
    const newEvents = (graph.events ?? []).filter((e) => e.seq > lastSeqRef.current);
    if (!force && newEvents.length < MIN_EVENT_DELTA) {
      lastSeqRef.current = max;
      return;
    }
    if (newEvents.length === 0) return;

    // Group new events by session_id; pick the busiest one.
    const bySid: Record<string, typeof newEvents> = {};
    for (const e of newEvents) {
      const sid = typeof e.payload.session_id === "string" ? e.payload.session_id : null;
      if (!sid) continue;
      (bySid[sid] ??= []).push(e);
    }
    const sids = Object.keys(bySid);
    if (sids.length === 0) {
      lastSeqRef.current = max;
      return;
    }
    sids.sort((a, b) => bySid[b].length - bySid[a].length);
    const topSid = sids[0];

    // Anchor on the middle event's ts so the activity window is tight.
    const sorted = bySid[topSid].slice().sort((a, b) => {
      const ta = (a.payload.ts as string | undefined) ?? a.ts ?? "";
      const tb = (b.payload.ts as string | undefined) ?? b.ts ?? "";
      return ta.localeCompare(tb);
    });
    const mid = sorted[Math.floor(sorted.length / 2)];
    const anchorTs = (mid.payload.ts as string | undefined) ?? mid.ts;

    setInFlight(true);
    lastFireRef.current = now;
    setLastFireMs(now);
    try {
      const r: SummaryResponse = await fetchSummary(topSid, {
        kind: "narrative",
        atTs: anchorTs,
      });
      lastSeqRef.current = max;
      if (r.text) {
        const sessionLabel = getCachedName(topSid) ?? topSid.slice(0, 8) + "…";
        setEntries((prev) => {
          // De-dupe if the same text came back (cache hit).
          const last = prev[prev.length - 1];
          if (last && last.text === r.text && last.sessionId === topSid) return prev;
          const entry: ConsoleEntry = {
            ts: now,
            text: r.text!,
            sessionId: topSid,
            sessionLabel,
            source: r.source,
            fresh: true,
            err: false,
          };
          // Drop the fresh flag on prior entries.
          const next = prev.map((e) => ({ ...e, fresh: false }));
          next.push(entry);
          return next.slice(-MAX_ENTRIES);
        });
      }
    } catch (e) {
      setEntries((prev) => {
        const next = prev.map((e2) => ({ ...e2, fresh: false }));
        next.push({
          ts: now,
          text: `live-log error: ${String(e).slice(0, 160)}`,
          sessionId: topSid,
          sessionLabel: topSid.slice(0, 8) + "…",
          source: "error",
          fresh: true,
          err: true,
        });
        return next.slice(-MAX_ENTRIES);
      });
    } finally {
      setInFlight(false);
    }
  }

  // Poll every POLL_INTERVAL_MS; actual LLM fire is gated by
  // FIRE_INTERVAL_MS + MIN_EVENT_DELTA inside `fireTick`.
  useEffect(() => {
    const id = window.setInterval(() => void fireTick(false), POLL_INTERVAL_MS);
    return () => window.clearInterval(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graph]);

  // "last Ns ago" since-indicator re-renders every 5s.
  useEffect(() => {
    function refresh() {
      if (!lastFireMs) {
        setSinceLabel("");
        return;
      }
      const ago = Math.floor((Date.now() - lastFireMs) / 1000);
      setSinceLabel(`last ${ago}s ago`);
    }
    refresh();
    const id = window.setInterval(refresh, 5_000);
    return () => window.clearInterval(id);
  }, [lastFireMs]);

  const onMouseEnter = () => {
    hoverRef.current = true;
    setPaused(true);
  };
  const onMouseLeave = () => {
    hoverRef.current = false;
    setPaused(false);
  };

  function clearEntries() {
    setEntries([]);
  }
  function testFire() {
    // Bypass gates: temporarily rewind lastSeq + lastFire so the
    // next tick fires immediately with whatever's in the buffer.
    lastFireRef.current = 0;
    lastSeqRef.current = Math.max(-1, (graph?.max_seq ?? 0) - 50);
    void fireTick(true);
  }

  return (
    <div
      data-testid="session-console"
      className="border-t border-[#30363d] bg-[#0d1117] font-mono text-xs"
      onMouseEnter={onMouseEnter}
      onMouseLeave={onMouseLeave}
    >
      <div
        className="flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:bg-[#161b22]"
        onClick={() => setOpen((o) => !o)}
      >
        <span className="text-[#6e7681]">{open ? "▼" : "▶"}</span>
        <span
          className={`inline-block w-2 h-2 rounded-full ${
            paused ? "bg-[#6e7681]" : inFlight ? "bg-[#d29922]" : "bg-[#3fb950]"
          }`}
          style={{
            animation: paused || inFlight ? "none" : "pulse-dot 1.4s ease-in-out infinite",
          }}
        />
        <span className="text-[10px] uppercase tracking-wider text-[#58a6ff]">
          live log
        </span>
        <span className="text-[10px] text-[#6e7681] ml-2">{sinceLabel}</span>
        <span className="text-[10px] text-[#484f58] ml-2">{paused ? "paused (hover)" : ""}</span>
        <span className="ml-auto flex gap-2">
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              testFire();
            }}
            className="text-[10px] px-2 py-0.5 rounded bg-[#21262d] text-[#c9d1d9] hover:bg-[#30363d]"
            title="fire one narrative now, bypassing the throttle gates"
          >
            test fire
          </button>
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              clearEntries();
            }}
            className="text-[10px] px-2 py-0.5 rounded bg-[#21262d] text-[#c9d1d9] hover:bg-[#30363d]"
          >
            clear
          </button>
        </span>
      </div>
      {open ? (
        <ul
          className="overflow-y-auto px-3 py-2 space-y-1.5"
          style={{ maxHeight: "26vh" }}
        >
          {entries.length === 0 ? (
            <li className="text-[10px] text-[#6e7681] italic">
              {inFlight
                ? "first summary in flight…"
                : "waiting on at least 4 new events. test fire bypasses."}
            </li>
          ) : (
            entries
              .slice()
              .reverse()
              .map((e) => (
                <li
                  key={e.ts + ":" + e.sessionId}
                  className={`pl-2 border-l-2 ${
                    e.err ? "border-[#f85149]" : e.fresh ? "border-[#58a6ff] bg-[#1f6feb22]" : "border-[#30363d]"
                  }`}
                  style={{
                    transition: "background-color 1.2s ease-out",
                  }}
                >
                  <div className="flex items-baseline gap-2 text-[10px] mb-0.5">
                    <span className="text-[#6e7681]">
                      {new Date(e.ts).toLocaleTimeString(undefined, {
                        hour: "2-digit",
                        minute: "2-digit",
                        second: "2-digit",
                        hour12: false,
                      })}
                    </span>
                    <span className="text-[#58a6ff] truncate">
                      {e.sessionLabel}
                    </span>
                    <span className="text-[#484f58] ml-auto whitespace-nowrap">
                      {e.source}
                    </span>
                  </div>
                  <div
                    className={`text-[11px] leading-relaxed whitespace-pre-wrap break-words ${
                      e.err ? "text-[#f85149]" : "text-[#c9d1d9]"
                    }`}
                  >
                    {e.text}
                  </div>
                </li>
              ))
          )}
        </ul>
      ) : null}
    </div>
  );
}
