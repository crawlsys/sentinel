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
  const t = Date.parse(ts);
  if (Number.isNaN(t)) return "—";
  const d = new Date(t);
  return d.toISOString().slice(11, 19);
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

export function nodeColor(kind: string, outcome?: string): string {
  if (outcome === "denied") return "#f85149";
  if (outcome === "injected") return "#d29922";
  return NODE_COLOR[kind] ?? "#6e7681";
}
