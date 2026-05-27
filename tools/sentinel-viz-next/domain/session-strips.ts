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
  /** Which harness produced this session — claude or codex.
   *  Sourced from SentinelSession.data.source_harness set by
   *  sentinel-bridge during ingestion. Null = unknown.
   *  opencode/qwen/gemini are dormant per the bridge allowlist
   *  but legacy records in the store may still carry those values. */
  sourceHarness: string | null;
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

/// Human-readable label per category for the short-description
/// fallback. Kept separate from `categoryLabel()` (used in the
/// graph legend) so the strip header reads naturally even when
/// the formal label is long.
const CATEGORY_SHORT: Record<NodeCategory, string> = {
  tc: "tool-calls",
  planning: "planning",
  communication: "comms",
  prompt: "prompts",
  other: "activity",
};

/// Status → human phrase for the stuck-only fallback path. Empty
/// for active statuses we don't want to flag inline.
const STATUS_SHORT: Record<string, string> = {
  awaiting_user: "stuck",
  dormant: "dormant",
  dead: "dead",
};

/// Map tool name → verb phrase for prose blurbs. Categorised by
/// what the tool is doing semantically, not by the tool's literal
/// name — "running" reads better than "Bash-ing", and operators
/// already see the harness chip elsewhere.
const TOOL_VERB: Record<string, string> = {
  Edit: "editing",
  Write: "editing",
  MultiEdit: "editing",
  NotebookEdit: "editing",
  Read: "reading",
  Glob: "searching",
  Grep: "searching",
  Bash: "running",
  TaskCreate: "tracking",
  TaskUpdate: "tracking",
  TaskList: "tracking",
  TodoWrite: "tracking",
  WebFetch: "researching",
  WebSearch: "researching",
  Agent: "delegating",
  Skill: "running",
};

/// Fields where a tool's "object" lives on the payload — what file
/// it's editing, what command it's running, etc. Walked in order;
/// first non-empty value wins.
const TOOL_OBJECT_FIELDS = ["file_path", "path", "command", "pattern", "subject", "query"];

/// Extract the "object" string from an event payload. For Bash
/// commands, we take the first whitespace token (the actual
/// command verb — `cargo`, `git`, `pnpm` — not the full chain).
/// For paths, we strip the directory to just the basename so the
/// header doesn't wrap the strip width.
function extractObject(tool: string, payload: Record<string, unknown>): string | null {
  // tool_input may be the carrier instead of the top-level payload.
  const ti = (payload.tool_input as Record<string, unknown> | undefined) ?? payload;
  for (const key of TOOL_OBJECT_FIELDS) {
    const v = ti[key];
    if (typeof v !== "string" || v.length === 0) continue;
    if (key === "command" || tool === "Bash") {
      // Pull the command verb: first non-empty token of the (possibly
      // `cd …;`-prefixed) chain. Operators care about "cargo test"
      // or "git status", not "/home/dev/...; cd …;".
      const cmd = v.replace(/^(?:cd\s+\S+\s*;\s*)+/, "").trim();
      const first = cmd.split(/\s+/)[0] ?? "";
      const second = cmd.split(/\s+/)[1] ?? "";
      // `cargo test`, `git status`, `pnpm build` — two tokens reads
      // better than one for these. Single-token fallback for `ls`,
      // `pwd`, etc.
      return second.length > 0 ? `${first} ${second}` : first;
    }
    if (key === "file_path" || key === "path") {
      const slash = v.lastIndexOf("/");
      return slash >= 0 ? v.slice(slash + 1) : v;
    }
    if (key === "pattern") return `/${v}/`;
    return v;
  }
  return null;
}

export interface ProseBlurbInput {
  tool: string;
  payload: Record<string, unknown>;
}

/// Compose a short prose blurb from recent tool activity. The
/// goal is something readable like "editing EventTicker.tsx" or
/// "running cargo test" — replacing the bare `s:fafafafa`
/// placeholder when no LLM name is available.
///
/// Strategy:
///   1. Bucket events by verb (TOOL_VERB lookup), pick the most-
///      common bucket.
///   2. Within that bucket, bucket again by object (file basename,
///      command first-word, etc.) and pick the most common object.
///   3. Compose "<verb> <object>". If no object survives (e.g.
///      all the events were verb-only like a Skill invocation),
///      return just "<verb>".
///
/// Returns null when:
///   - The events list is empty
///   - No event maps to a known verb (TOOL_VERB miss)
///
/// Exported for unit tests.
export function deriveProseBlurb(events: ProseBlurbInput[]): string | null {
  if (events.length === 0) return null;
  const verbCounts = new Map<string, number>();
  const objectsByVerb = new Map<string, Map<string, number>>();
  for (const e of events) {
    const verb = TOOL_VERB[e.tool];
    if (!verb) continue;
    verbCounts.set(verb, (verbCounts.get(verb) ?? 0) + 1);
    const obj = extractObject(e.tool, e.payload);
    if (!obj) continue;
    let byObj = objectsByVerb.get(verb);
    if (!byObj) {
      byObj = new Map();
      objectsByVerb.set(verb, byObj);
    }
    byObj.set(obj, (byObj.get(obj) ?? 0) + 1);
  }
  if (verbCounts.size === 0) return null;
  // Pick the dominant verb (highest count; ties broken by insertion
  // order, which matches event arrival order — newest first).
  let topVerb = "";
  let topVerbCount = 0;
  for (const [v, n] of verbCounts) {
    if (n > topVerbCount) {
      topVerb = v;
      topVerbCount = n;
    }
  }
  const byObj = objectsByVerb.get(topVerb);
  if (!byObj || byObj.size === 0) return topVerb;
  let topObj = "";
  let topObjCount = 0;
  for (const [o, n] of byObj) {
    if (n > topObjCount) {
      topObj = o;
      topObjCount = n;
    }
  }
  return `${topVerb} ${topObj}`;
}

/// Compose a short description when the LLM name isn't available
/// yet (or never resolves — codex sessions without a transcript,
/// the naming model being disabled, rate-limited fetches, etc.).
/// Goal: replace bare `s:fafafafa` placeholders with something
/// operators can actually read at a glance — "codex · tool-calls
/// 18/min · s:fafafafa" beats the raw shortSid every time.
///
/// Exported for unit tests; the strip builder + the stuck-only
/// backfill both use it so the two paths can't drift.
export function deriveShortDescription(opts: {
  harness: string | null;
  status: string | null;
  topCategory: NodeCategory | null;
  totalEvents: number;
  peakPerMin: number;
  stuckKind: string | null;
  shortSid: string;
}): string {
  const parts: string[] = [];
  if (opts.harness) parts.push(opts.harness);
  // Stuck/dormant/dead status outranks the activity blurb — the
  // operator needs to know the session isn't moving, not what its
  // last-known activity flavour was.
  const statusPhrase = opts.status ? STATUS_SHORT[opts.status] : null;
  if (statusPhrase) {
    parts.push(opts.stuckKind ? `${statusPhrase} ${opts.stuckKind}` : statusPhrase);
  } else if (opts.topCategory && opts.peakPerMin > 0) {
    parts.push(`${CATEGORY_SHORT[opts.topCategory]} ${opts.peakPerMin}/min`);
  } else if (opts.topCategory && opts.totalEvents > 0) {
    parts.push(`${CATEGORY_SHORT[opts.topCategory]} ×${opts.totalEvents}`);
  }
  parts.push(`s:${opts.shortSid}`);
  return parts.join(" · ");
}

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
  // sid → source_harness, derived from the first hook_ingested event
  // for the session. The graph.nodes set is often narrower than the
  // graph.events set (the viz-api returns ~6 top-K session nodes but
  // hundreds of events), so deriving the harness chip from events
  // lets every visible strip carry its tag — not just the few with
  // a node in the response.
  const harnessBySid = new Map<string, string>();
  // sid → most-recent N events with a tool field. Used by
  // deriveProseBlurb to compose "<verb> <object>" labels for the
  // strip header when no LLM name is available. Bounded so a
  // long session doesn't carry hundreds of events into the
  // bucketing function — only recent flavour matters for the
  // header text.
  const PROSE_EVENT_BUDGET = 15;
  const proseEventsBySid = new Map<string, ProseBlurbInput[]>();
  for (const e of graph.events) {
    const sid = typeof e.payload?.session_id === "string" ? (e.payload.session_id as string) : null;
    if (!sid) continue;
    if (!harnessBySid.has(sid)) {
      const h = typeof e.payload?.source_harness === "string"
        ? (e.payload.source_harness as string)
        : "";
      if (h.length > 0) harnessBySid.set(sid, h);
    }
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

    // Collect for prose blurb. Only tool-bearing events count;
    // user-prompt rows would skew the verb dominance toward
    // "tracking" / "researching" misleadingly.
    if (tool) {
      let arr = proseEventsBySid.get(sid);
      if (!arr) {
        arr = [];
        proseEventsBySid.set(sid, arr);
      }
      if (arr.length < PROSE_EVENT_BUDGET) {
        arr.push({ tool, payload: e.payload as Record<string, unknown> });
      }
    }
  }

  // Build the SessionStripData[] sorted by recency (last activity
  // age ascending — freshest at the top).
  //
  // P3-34: STUCK sessions are ALWAYS included even when they have
  // zero events in the window. By definition they're stuck because
  // the operator hasn't responded — so the agent's last event is
  // probably outside the window. Filtering them out hides exactly
  // the rows the operator needs to act on. Synthesise an empty
  // category set so the strip renders with just the stuck banner.
  const out: SessionStripData[] = [];
  const stuckSids = new Set<string>(opts.stuck ? Array.from(opts.stuck.keys()) : []);
  const seenSids = new Set<string>();
  for (const [sid, byCat] of perSession) {
    seenSids.add(sid);
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
    if (totalEvents === 0 && !stuckSids.has(sid)) continue;

    const status = node?.session_status ?? null;
    const lastActivityAgeS = node?.last_activity_age_s ?? null;
    const stuck = opts.stuck?.get(sid) ?? null;
    const color = opts.colors.get(sid) ?? "#6e7681";
    const name = opts.names?.get(sid) ?? null;
    const shortSid = sid.slice(0, 8);
    // Prefer node-level tag (authoritative) but fall back to the
    // event-derived value when the node isn't in this window.
    const sourceHarness =
      (node?.data?.source_harness as string | undefined) ??
      harnessBySid.get(sid) ??
      null;
    // Display-name preference order:
    //   1. LLM-assigned name (best — captures activity flavour in plain English)
    //   2. Prose blurb derived from recent tool activity ("editing
    //      EventTicker.tsx", "running cargo test", "searching for hook").
    //      Operator-readable, deterministic, LLM-free.
    //   3. Stat-style short description (harness + dominant category + rate).
    //   4. Bare s:<shortSid> as the final fallback.
    const prose = deriveProseBlurb(proseEventsBySid.get(sid) ?? []);
    let displayName: string;
    if (name && name.length > 0) {
      displayName = `${name} · s:${shortSid}`;
    } else if (prose) {
      displayName = `${prose} · s:${shortSid}`;
    } else {
      displayName = deriveShortDescription({
        harness: sourceHarness,
        status,
        topCategory: rows[0]?.category ?? null,
        totalEvents,
        peakPerMin,
        stuckKind: stuck?.kind ?? null,
        shortSid,
      });
    }

    out.push({
      sessionId: sid,
      displayName,
      shortSid,
      color,
      status,
      sourceHarness,
      lastActivityAgeS,
      stuck,
      rows,
      totalEvents,
      peakPerMin,
    });
  }

  // P3-34 (cont'd): backfill stuck sessions that had ZERO events
  // in the window. They never made it into `perSession` so the
  // loop above didn't visit them. Render with an empty category
  // list — the stuck banner is the whole point of showing them.
  if (opts.stuck) {
    for (const [sid, _meta] of opts.stuck) {
      if (seenSids.has(sid)) continue;
      const node = sessionNodes.get(sid);
      const shortSid = sid.slice(0, 8);
      const name = opts.names?.get(sid) ?? null;
      const sourceHarness =
        (node?.data?.source_harness as string | undefined) ??
        harnessBySid.get(sid) ??
        null;
      const status = node?.session_status ?? "awaiting_user";
      const stuckMeta = opts.stuck.get(sid) ?? null;
      const displayName = name && name.length > 0
        ? `${name} · s:${shortSid}`
        : deriveShortDescription({
            harness: sourceHarness,
            status,
            topCategory: null,
            totalEvents: 0,
            peakPerMin: 0,
            stuckKind: stuckMeta?.kind ?? null,
            shortSid,
          });
      out.push({
        sessionId: sid,
        displayName,
        shortSid,
        color: opts.colors.get(sid) ?? "#6e7681",
        status,
        sourceHarness,
        lastActivityAgeS: node?.last_activity_age_s ?? null,
        stuck: stuckMeta,
        rows: [],
        totalEvents: 0,
        peakPerMin: 0,
      });
    }
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
