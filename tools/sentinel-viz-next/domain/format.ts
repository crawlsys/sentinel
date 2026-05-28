export function relTime(ts: string, now: number = Date.now()): string {
  const t = Date.parse(ts);
  if (Number.isNaN(t)) return "—";
  const diffSec = Math.round((now - t) / 1000);
  if (diffSec < 0) return "in the future";
  if (diffSec < 60) return `${diffSec}s ago`;
  if (diffSec < 3600) return `${Math.floor(diffSec / 60)}m ago`;
  if (diffSec < 86_400) return `${Math.floor(diffSec / 3600)}h ago`;
  return `${Math.floor(diffSec / 86_400)}d ago`;
}

export function shortTime(ts: string): string {
  if (!ts) return "—";
  // If the string already carries HH:MM:SS, extract directly so we
  // don't get bitten by TZ conversion (the bridge writes ts_sec
  // without a Z suffix; Date.parse would assume local TZ).
  const m = /(\d{2}):(\d{2}):(\d{2})/.exec(ts);
  if (m) return `${m[1]}:${m[2]}:${m[3]}`;
  const t = Date.parse(ts);
  if (Number.isNaN(t)) return "—";
  return new Date(t).toISOString().slice(11, 19);
}

/** Ticker time string — relative for the first 90 minutes, absolute
 *  TZ-aware HH:MM after that, "Mon 14:30" beyond 24 hours.
 *  Buckets:
 *    < 5s    → "now"
 *    < 90s   → "30s ago"
 *    < 1h    → "12m ago"  (minute resolution)
 *    < 90m   → "1h 23m ago"
 *    < 24h   → "14:30"    (user's local TZ)
 *    ≥ 24h   → "Mon 14:30"
 *  The first 90 minutes is the working-context window; after that
 *  absolute is more useful (precise enough to scan visually,
 *  doesn't churn on every render). */
export function tickerTime(ts: string, now: number = Date.now()): string {
  if (!ts) return "—";
  // Bridge writes ts_sec without a TZ marker. Treat it as UTC so
  // the relative diff is correct (the bridge runs in UTC).
  let parseable = ts;
  if (/T\d{2}:\d{2}:\d{2}(\.\d+)?$/.test(ts)) {
    parseable = `${ts}Z`;
  }
  const t = Date.parse(parseable);
  if (Number.isNaN(t)) return "—";
  const diff = (now - t) / 1000;
  if (diff < 0) return "in future";
  if (diff < 5) return "now";
  if (diff < 90) return `${Math.round(diff)}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 5400) {
    const h = Math.floor(diff / 3600);
    const m = Math.floor((diff % 3600) / 60);
    return `${h}h ${m}m ago`;
  }
  const d = new Date(t);
  const hhmm = d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", hour12: false });
  if (diff < 86_400) return hhmm;
  const day = d.toLocaleDateString(undefined, { weekday: "short" });
  return `${day} ${hhmm}`;
}

export function shortSessionId(sid: string): string {
  if (sid.length <= 12) return sid;
  return `${sid.slice(0, 8)}…${sid.slice(-3)}`;
}

const STATUS_COLOR: Record<string, string> = {
  firing: "#3fb950",
  busy: "#58a6ff",
  idle: "#d29922",
  dormant: "#6e7681",
  dead: "#484f58",
  awaiting_user: "#bc8cff",
};

export function statusColor(status?: string | null): string {
  if (!status) return "#6e7681";
  return STATUS_COLOR[status] ?? "#6e7681";
}

// Operator-facing status words. The backend session_status enum is
// machine jargon: `awaiting_user` (underscored), `firing` (sentinel-
// internal for "actively producing events"). Translate to plain words
// for the strip header badge — same key set as STATUS_COLOR so the two
// can't drift. Unknown statuses fall through unchanged (mirrors
// sentinelEventPhrase) so a novel backend status is never swallowed.
// `awaiting_user` reads "waiting", not "stuck": the badge shows for any
// awaiting session, while the red STUCK box is the dedicated past-the-
// threshold signal.
const STATUS_LABEL: Record<string, string> = {
  firing: "active",
  busy: "busy",
  idle: "idle",
  dormant: "dormant",
  dead: "dead",
  awaiting_user: "waiting",
};

export function statusLabel(status?: string | null): string {
  if (!status) return "—";
  return STATUS_LABEL[status] ?? status;
}

// Operator-facing phrasing for the bridge's `awaiting_kind` — WHY a
// session is parked waiting on the operator. The bridge writes the
// raw lifecycle/tool identifier that triggered the wait
// (`AskUserQuestion`, `PreToolUse`, `Stop`) or the API's generic
// classification (`question`, `reply`). Those leak into the highest-
// signal surface the operator scans — the red STUCK banner — as
// camel-case jargon. Translate to the same "what is it waiting on"
// register the stuck box already speaks in.
//
// Same discipline as `statusLabel` / `sentinelEventPhrase`: unknown
// kinds fall through UNCHANGED so a novel backend kind is surfaced,
// not swallowed. null/undefined → "awaiting" — the fallback string
// the stuck banners already used inline, kept here so the two paths
// can't drift.
const AWAITING_KIND_LABEL: Record<string, string> = {
  AskUserQuestion: "your answer",
  question: "your answer",
  reply: "your reply",
  PreToolUse: "tool approval",
  Stop: "stop confirmation",
  Notification: "your attention",
};

export function awaitingKindLabel(kind?: string | null): string {
  if (!kind) return "awaiting";
  return AWAITING_KIND_LABEL[kind] ?? kind;
}

const NODE_COLOR: Record<string, string> = {
  SentinelSession: "#bc8cff",
  SentinelHookInvocation: "#58a6ff",
  SentinelToolCall: "#3fb950",
};

const CATEGORY_COLOR: Record<string, string> = {
  tc: "#3fb950", // green — compute (Bash/Read/Write/Edit/Grep)
  planning: "#d29922", // amber — planning/research (Task/Web)
  communication: "#bc8cff", // purple — agent / questions / stop
  prompt: "#58a6ff", // blue — user prompts (UserPromptSubmit)
  other: "#6e7681", // muted — unrecognised tools
};

export function categoryColor(cat?: string | null): string {
  if (!cat) return "#6e7681";
  return CATEGORY_COLOR[cat] ?? "#6e7681";
}

export function categoryLabel(cat?: string | null): string {
  switch (cat) {
    case "tc": return "compute";
    case "planning": return "planning";
    case "communication": return "comm";
    case "prompt": return "prompt";
    case "other": return "other";
    default: return "—";
  }
}

export function nodeColor(kind: string, outcome?: string, category?: string | null): string {
  if (outcome === "denied") return "#f85149";
  if (outcome === "injected") return "#d29922";
  if (kind === "SentinelToolCall" && category) return categoryColor(category);
  return NODE_COLOR[kind] ?? "#6e7681";
}
