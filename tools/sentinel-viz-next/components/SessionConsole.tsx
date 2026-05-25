"use client";

import { useEffect, useState } from "react";

import { fetchSummary, type SummaryResponse } from "../lib/api";

interface Props {
  /** Session whose rolling narrative we follow. Null = collapsed/hidden. */
  sessionId: string | null;
  /** Friendly display name (LLM-generated or UUID slice fallback). */
  sessionLabel: string | null;
}

interface ConsoleEntry {
  ts: number;
  text: string;
  source: string;
  cached: boolean;
}

/// Bottom-pinned narrative console. Polls /api/summary?kind=card
/// every 30s for the focused session and appends the result to a
/// rolling log. Collapsed by default; user expands to see history.
export function SessionConsole({ sessionId, sessionLabel }: Props) {
  const [entries, setEntries] = useState<ConsoleEntry[]>([]);
  const [open, setOpen] = useState<boolean>(true);
  const [loading, setLoading] = useState<boolean>(false);

  // Reset entries when session changes.
  useEffect(() => {
    setEntries([]);
  }, [sessionId]);

  useEffect(() => {
    if (!sessionId) return;
    let cancelled = false;

    async function tick() {
      if (cancelled || !sessionId) return;
      setLoading(true);
      try {
        const r: SummaryResponse = await fetchSummary(sessionId, { kind: "card" });
        if (cancelled) return;
        if (r.text) {
          setEntries((prev) => {
            const last = prev[prev.length - 1];
            // Avoid duplicate cached entries (cache TTL is 10min).
            if (last && last.text === r.text) return prev;
            const next = [...prev, { ts: Date.now(), text: r.text!, source: r.source, cached: r.cached }];
            // Keep last 12 entries.
            return next.slice(-12);
          });
        }
      } catch {
        /* silent — surface errors via Inspector's summary card */
      } finally {
        setLoading(false);
      }
    }

    tick();
    const id = window.setInterval(tick, 30_000);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
  }, [sessionId]);

  if (!sessionId) return null;

  return (
    <div
      data-testid="session-console"
      className="border-t border-[#30363d] bg-[#0d1117] font-mono text-xs"
    >
      <div
        className="flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:bg-[#161b22]"
        onClick={() => setOpen((o) => !o)}
      >
        <span className="text-[#6e7681]">{open ? "▼" : "▶"}</span>
        <span className="text-[10px] uppercase tracking-wider text-[#58a6ff]">
          session console
        </span>
        <span className="text-[10px] text-[#6e7681]">·</span>
        <span className="text-[10px] text-[#c9d1d9] truncate">{sessionLabel ?? sessionId.slice(0, 8) + "…"}</span>
        <span className="text-[10px] text-[#484f58] ml-auto">
          {entries.length} entries{loading ? " · refreshing" : ""}
        </span>
      </div>
      {open ? (
        <ul
          className="overflow-y-auto px-3 py-2 space-y-2"
          style={{ maxHeight: "26vh" }}
        >
          {entries.length === 0 ? (
            <li className="text-[10px] text-[#6e7681] italic">
              {loading ? "first summary in flight…" : "(no entries yet)"}
            </li>
          ) : (
            entries
              .slice()
              .reverse()
              .map((e) => (
                <li
                  key={e.ts}
                  className="border-l-2 border-[#30363d] pl-2"
                >
                  <div className="flex justify-between items-baseline mb-0.5">
                    <span className="text-[10px] text-[#6e7681]">
                      {new Date(e.ts).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false })}
                    </span>
                    <span className="text-[10px] text-[#484f58]">
                      {e.cached ? "cached" : e.source}
                    </span>
                  </div>
                  <div className="text-[11px] text-[#c9d1d9] whitespace-pre-wrap leading-relaxed">
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
