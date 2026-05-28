"use client";

import { useEffect, useMemo, useState } from "react";
import { Chip, Stack, ToggleButton, ToggleButtonGroup } from "@mui/material";

import type { GraphResponse, Node, NodeCategory } from "../types/api";
import { buildSessionStrips, type SessionStripData } from "../domain/session-strips";
import { sessionColorMap } from "../domain/session-colors";
import { fetchSessionName } from "../adapters/http";
import { SessionStrip } from "./SessionStrip";

const HARNESS_FILTER_KEY = "sentinel-viz-harness-filter";
const HARNESS_FILTER_VERSION_KEY = "sentinel-viz-harness-filter-version";
const HARNESS_FILTER_VERSION = "2";
// Allowlist: only claude + codex sessions are tracked. The bridge's
// opencode/qwen/gemini shims are gated dormant behind a non-default
// cargo feature; this list mirrors that.
const HARNESSES = ["claude", "codex"] as const;
type HarnessId = (typeof HARNESSES)[number];
const DEFAULT_HARNESSES: HarnessId[] = ["claude", "codex"];
const SESSION_NAME_FETCH_DELAY_MS = 4_000;
const SESSION_NAME_FETCH_BATCH = 2;
const STICKY_CATEGORY_ORDER: NodeCategory[] = ["tc", "planning", "communication", "prompt", "other"];

function defaultHarnessSet(): Set<HarnessId> {
  return new Set(DEFAULT_HARNESSES);
}

function harnessColor(h: string): string {
  switch (h) {
    case "claude":   return "#5B9BF6";
    case "codex":    return "#D4A843";
    default:         return "#999999";
  }
}

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
  const [prefsLoaded, setPrefsLoaded] = useState(false);
  const [windowMinutes, setWindowMinutes] = useState<number>(DEFAULT_WINDOW);

  // Per-harness filter — operator can hide noisy harnesses without
  // the bridge changing what it ingests. localStorage persists the
  // preference. v2 default includes Codex; older browsers may have
  // persisted the previous Claude-only default, so ignore old-version
  // storage once and rewrite it below.
  const [enabledHarnesses, setEnabledHarnesses] = useState<Set<HarnessId>>(() => defaultHarnessSet());
  const [stripOrder, setStripOrder] = useState<string[]>([]);
  const [stickyCategories, setStickyCategories] = useState<Map<string, Set<NodeCategory>>>(new Map());

  useEffect(() => {
    if (typeof window === "undefined") return;
    const storedWindow = window.localStorage.getItem(WINDOW_STORAGE_KEY);
    const n = storedWindow ? parseInt(storedWindow, 10) : NaN;
    if (Number.isFinite(n) && n > 0) setWindowMinutes(n);

    const version = window.localStorage.getItem(HARNESS_FILTER_VERSION_KEY);
    const raw = window.localStorage.getItem(HARNESS_FILTER_KEY);
    if (raw && version === HARNESS_FILTER_VERSION) {
      try {
        const arr = JSON.parse(raw) as string[];
        const parsed = new Set(arr.filter((h): h is HarnessId => HARNESSES.includes(h as HarnessId)));
        setEnabledHarnesses(parsed.size > 0 ? parsed : defaultHarnessSet());
      } catch { /* fall through to default */ }
    }
    setPrefsLoaded(true);
  }, []);

  useEffect(() => {
    if (typeof window === "undefined" || !prefsLoaded) return;
    window.localStorage.setItem(HARNESS_FILTER_VERSION_KEY, HARNESS_FILTER_VERSION);
    window.localStorage.setItem(
      HARNESS_FILTER_KEY,
      JSON.stringify(Array.from(enabledHarnesses)),
    );
  }, [enabledHarnesses]);
  const toggleHarness = (h: HarnessId) => {
    setEnabledHarnesses((prev) => {
      const next = new Set(prev);
      if (next.has(h)) next.delete(h);
      else next.add(h);
      return next;
    });
  };

  useEffect(() => {
    if (typeof window === "undefined" || !prefsLoaded) return;
    window.localStorage.setItem(WINDOW_STORAGE_KEY, String(windowMinutes));
  }, [windowMinutes, prefsLoaded]);

  const sessionColors = useMemo(() => sessionColorMap(graph), [graph]);

  const [nameMap, setNameMap] = useState<Map<string, string>>(new Map());

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

  const rawStrips = useMemo(() => {
    const all = buildSessionStrips(graph, {
      windowMinutes,
      colors: sessionColors,
      names: nameMap,
      stuck: stuckMap,
    });
    // Per-harness filter — strips whose source_harness isn't in the
    // operator's enabled set get hidden. Unknown-harness sessions
    // (sourceHarness == null) pass through if any harness is enabled
    // so we don't silently hide host claude sessions whose node
    // dropped out of the API window.
    const harnessFiltered = all.filter((s) => {
      if (enabledHarnesses.size === 0) return true; // no filter = show everything
      const h = (s.sourceHarness ?? "claude") as HarnessId;
      return enabledHarnesses.has(h);
    });
    if (!dormantSessionIds || dormantSessionIds.size === 0) return harnessFiltered;
    // Hide dead/long-dormant sessions UNLESS they're stuck — a
    // stuck session is signal even when "old" by the bridge's
    // dormancy clock.
    return harnessFiltered.filter((s) => !dormantSessionIds.has(s.sessionId) || s.stuck);
  }, [graph, windowMinutes, sessionColors, nameMap, stuckMap, dormantSessionIds, enabledHarnesses]);

  useEffect(() => {
    setStickyCategories((prev) => {
      let changed = false;
      const next = new Map(prev);
      for (const strip of rawStrips) {
        if (strip.rows.length === 0) continue;
        let cats = next.get(strip.sessionId);
        for (const row of strip.rows) {
          if (!cats) {
            cats = new Set<NodeCategory>();
            next.set(strip.sessionId, cats);
          }
          if (!cats.has(row.category)) {
            cats.add(row.category);
            changed = true;
          }
        }
      }
      return changed ? next : prev;
    });
  }, [rawStrips]);

  const stickyStrips = useMemo(() => {
    return rawStrips.map((strip): SessionStripData => {
      const cats = stickyCategories.get(strip.sessionId);
      if (!cats || cats.size === 0) return strip;
      const rowsByCategory = new Map(strip.rows.map((row) => [row.category, row]));
      const rows = STICKY_CATEGORY_ORDER
        .filter((category) => cats.has(category) || rowsByCategory.has(category))
        .map((category) =>
          rowsByCategory.get(category) ?? {
            category,
            counts: new Array(windowMinutes * 2).fill(0),
            total: 0,
            peak: 0,
          },
        );
      if (rows.length === strip.rows.length) return strip;
      return { ...strip, rows };
    });
  }, [rawStrips, stickyCategories, windowMinutes]);

  useEffect(() => {
    setStripOrder((prev) => {
      const seen = new Set(prev);
      const appended = stickyStrips.map((s) => s.sessionId).filter((sid) => !seen.has(sid));
      if (appended.length === 0) return prev;
      return [...prev, ...appended];
    });
  }, [stickyStrips]);

  const strips = useMemo(() => {
    if (stripOrder.length === 0) return stickyStrips;
    const order = new Map(stripOrder.map((sid, i) => [sid, i]));
    return stickyStrips
      .slice()
      .sort((a, b) => (order.get(a.sessionId) ?? Number.MAX_SAFE_INTEGER) - (order.get(b.sessionId) ?? Number.MAX_SAFE_INTEGER));
  }, [stickyStrips, stripOrder]);

  const missingVisibleNameKey = useMemo(() => {
    return strips
      .map((s) => s.sessionId)
      .filter((sid) => !nameMap.has(sid))
      .join("|");
  }, [strips, nameMap]);

  // Fetch LLM-assigned session names lazily and only for visible
  // strips. This is deliberately delayed/batched so session summary
  // requests win the first Ollama slots when a dashboard opens.
  useEffect(() => {
    if (!missingVisibleNameKey) return;
    let cancelled = false;
    const sids = missingVisibleNameKey.split("|").filter(Boolean).slice(0, SESSION_NAME_FETCH_BATCH);
    const timer = window.setTimeout(async () => {
      for (const sid of sids) {
        try {
          const resp = await fetchSessionName(sid);
          const name = resp.name;
          if (cancelled || !name) continue;
          setNameMap((prev) => {
            if (prev.has(sid)) return prev;
            const next = new Map(prev);
            next.set(sid, name);
            return next;
          });
        } catch {
          /* low-priority ornamentation; keep the strip usable */
        }
      }
    }, SESSION_NAME_FETCH_DELAY_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [missingVisibleNameKey]);

  return (
    <section
      data-testid="session-strips-panel"
      className="flex flex-col h-full min-h-0 bg-[#000] text-[#E8E8E8] font-mono"
    >
      <header className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2 border-b border-[#222] text-[10px] uppercase tracking-wider text-[#999]">
        <span>sessions</span>
        <span className="text-[#666]">· last {labelForWindow(windowMinutes)}</span>
        <Stack
          direction="row"
          spacing={0.5}
          data-testid="strips-harness-filter"
          sx={{ alignItems: "center" }}
        >
          {HARNESSES.map((h) => {
            const on = enabledHarnesses.has(h);
            return (
              <Chip
                key={h}
                label={h}
                size="small"
                onClick={() => toggleHarness(h)}
                data-harness={h}
                data-on={on ? "true" : "false"}
                title={
                  on
                    ? `click to hide ${h} sessions`
                    : `click to show ${h} sessions`
                }
                sx={{
                  height: 18,
                  fontSize: 9,
                  letterSpacing: "0.06em",
                  borderColor: on ? harnessColor(h) : "var(--border)",
                  color: on ? harnessColor(h) : "var(--text-disabled)",
                  bgcolor: on ? harnessColor(h) + "1A" : "transparent",
                  cursor: "pointer",
                }}
              />
            );
          })}
        </Stack>
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
