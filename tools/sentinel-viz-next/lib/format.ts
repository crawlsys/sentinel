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
