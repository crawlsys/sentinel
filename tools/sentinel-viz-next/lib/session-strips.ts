/// Build the data each SessionStrip needs: one row per active
/// session, with per-tool-category sparkline buckets covering the
/// requested time window. Pure function; testable.

import type { GraphResponse, Node, NodeCategory, RecentEvent } from "../types/api";
import { categoryForTool, deriveLabelAndCategory } from "./event-category";

export interface SessionStripCategoryRow {
  /** Operator-facing tool category. */
  category: NodeCategory;
  /** Per-minute event counts. Length === windowMinutes. Oldest first. */
  counts: number[];
  /** Sum of counts across the window. */
  total: number;
  /** Max single-bucket count (for normalising the sparkline bar height). */
  peak: number;
}

export interface SessionStripData {
  sessionId: string;
  /** LLM-assigned session name when available, otherwise the
   *  short sid prefix. */
  displayName: string;
  shortSid: string;
  /** Hex colour from the session palette. */
  color: string;
  status: string | null;
  /** Seconds since the session's last activity. Null when unknown. */
  lastActivityAgeS: number | null;
  /** Stuck context when the session is awaiting_user past the
   *  stuck threshold. Mirrors EventTicker.StuckMeta shape — the
   *  SessionStrip surfaces a stuck banner for these. */
  stuck: {
    ageSecs: number;
    kind: string | null;
    question: string | null;
  } | null;
  /** One row per category that actually saw activity in the
   *  window. Order: tc, planning, communication, prompt, other.
   *  Empty categories are dropped so the strip stays compact. */
  rows: SessionStripCategoryRow[];
  /** Total events in the window across all categories. */
  totalEvents: number;
  /** Highest events-per-minute bucket across all categories. */
  peakPerMin: number;
}

/// Category render order — tc first because it's the dominant
/// flavour for most sessions. "other" last so it never crowds out
/// the meaningful categories.
const CATEGORY_ORDER: NodeCategory[] = ["tc", "planning", "communication", "prompt", "other"];

export interface BuildOptions {
  /** Time window in minutes. Default 60. Used to size the
   *  bucket array (one entry per minute). */
  windowMinutes: number;
  /** Map session_id → palette colour (from sessionColorMap). */
  colors: Map<string, string>;
  /** Optional session_id → llm-name map for the strip header. */
  names?: Map<string, string>;
  /** session_id → stuck context, from stuck.ts. */
  stuck?: Map<string, { ageSecs: number; kind: string | null; question: string | null }>;
  /** Anchor "now" for testing. Defaults to Date.now(). */
  now?: number;
}

/// Build session-strip data from a graph snapshot.
export function buildSessionStrips(
  graph: GraphResponse | null,
  opts: BuildOptions,
): SessionStripData[] {
  if (!graph) return [];
  const now = opts.now ?? Date.now();
  const windowMs = opts.windowMinutes * 60_000;
  const windowStart = now - windowMs;
  const numBuckets = opts.windowMinutes;

  // sid → SentinelSession node (for status, age, awaiting fields).
  const sessionNodes = new Map<string, Node>();
  for (const n of graph.nodes) {
    if (n.type !== "SentinelSession") continue;
    const sid = typeof n.data?.session_id === "string" ? (n.data.session_id as string) : null;
    if (sid) sessionNodes.set(sid, n);
  }

  // sid → category → counts[numBuckets]
  const perSession = new Map<string, Map<NodeCategory, number[]>>();
  for (const e of graph.events) {
    const sid = typeof e.payload?.session_id === "string" ? (e.payload.session_id as string) : null;
    if (!sid) continue;
    const t = parseEventTs(e);
    if (Number.isNaN(t)) continue;
    if (t < windowStart) continue;

    const bucketIdx = Math.min(
      numBuckets - 1,
      Math.max(0, Math.floor((t - windowStart) / 60_000)),
    );
    const sentinelEvent =
      typeof e.payload?.sentinel_event === "string"
        ? (e.payload.sentinel_event as string)
        : e.type.replace(/^sentinel\./, "");
    const tool =
      typeof e.payload?.tool === "string" && (e.payload.tool as string).length > 0
        ? (e.payload.tool as string)
        : null;
    const category = categoryForTool(sentinelEvent, tool);

    let bySid = perSession.get(sid);
    if (!bySid) {
      bySid = new Map();
      perSession.set(sid, bySid);
    }
    let counts = bySid.get(category);
    if (!counts) {
      counts = new Array(numBuckets).fill(0);
      bySid.set(category, counts);
    }
    counts[bucketIdx] += 1;
  }

  // Build the SessionStripData[] sorted by recency (last activity
  // age ascending — freshest at the top).
  const out: SessionStripData[] = [];
  for (const [sid, byCat] of perSession) {
    const node = sessionNodes.get(sid);
    const rows: SessionStripCategoryRow[] = [];
    let totalEvents = 0;
    let peakPerMin = 0;
    for (const category of CATEGORY_ORDER) {
      const counts = byCat.get(category);
      if (!counts) continue;
      const total = counts.reduce((a, b) => a + b, 0);
      if (total === 0) continue;
      const peak = counts.reduce((a, b) => (b > a ? b : a), 0);
      rows.push({ category, counts, total, peak });
      totalEvents += total;
      if (peak > peakPerMin) peakPerMin = peak;
    }
    if (totalEvents === 0) continue;

    const status = node?.session_status ?? null;
    const lastActivityAgeS = node?.last_activity_age_s ?? null;
    const stuck = opts.stuck?.get(sid) ?? null;
    const color = opts.colors.get(sid) ?? "#6e7681";
    const name = opts.names?.get(sid) ?? null;
    const shortSid = sid.slice(0, 8);
    const displayName = name && name.length > 0 ? `${name} · s:${shortSid}` : `s:${shortSid}`;

    out.push({
      sessionId: sid,
      displayName,
      shortSid,
      color,
      status,
      lastActivityAgeS,
      stuck,
      rows,
      totalEvents,
      peakPerMin,
    });
  }

  out.sort((a, b) => {
    const aw = a.stuck ? -1 : a.lastActivityAgeS ?? Number.POSITIVE_INFINITY;
    const bw = b.stuck ? -1 : b.lastActivityAgeS ?? Number.POSITIVE_INFINITY;
    return aw - bw;
  });
  return out;
}

/// Render a per-minute count array as a unicode bar sparkline. The
/// 8 block characters give a clean step from "empty" (mid-dot) to
/// "full" (full block) — works on monospace fonts without needing
/// SVG. Normalised against the local peak so a quiet category
/// still shows its rhythm.
const BAR_CHARS = ["·", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
export function bucketsToSparkline(counts: number[], peak: number): string {
  if (counts.length === 0) return "";
  const scale = peak > 0 ? (BAR_CHARS.length - 1) / peak : 0;
  let out = "";
  for (const c of counts) {
    if (c === 0) out += BAR_CHARS[0];
    else out += BAR_CHARS[Math.min(BAR_CHARS.length - 1, Math.max(1, Math.round(c * scale)))];
  }
  return out;
}

function parseEventTs(e: RecentEvent): number {
  const tsStr =
    typeof e.payload?.ts_sec === "string"
      ? (e.payload.ts_sec as string)
      : typeof e.payload?.ts === "string"
        ? (e.payload.ts as string)
        : e.ts;
  if (!tsStr) return NaN;
  const parseable = /T\d{2}:\d{2}:\d{2}(\.\d+)?$/.test(tsStr) ? `${tsStr}Z` : tsStr;
  return Date.parse(parseable);
}

// Suppress unused-warning when other consumers don't use this.
void deriveLabelAndCategory;
