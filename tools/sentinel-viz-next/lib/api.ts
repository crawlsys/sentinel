import type { ActivityResponse, GraphResponse, HealthResponse } from "../types/api";

const DEFAULT_BASE = "http://127.0.0.1:8082";

/// SECURITY: this app is a LOCALHOST-ONLY operator tool. The API base
/// below is interpolated into every fetch/EventSource URL, so it must
/// never be pointed at — or this binary deployed to — a shared origin
/// where the value could be attacker-controlled. The validation here
/// rejects non-http(s) schemes (e.g. javascript:, file:, data:) so a
/// bad NEXT_PUBLIC_VIZ_API can't smuggle in a non-network URL; it does
/// NOT make a public deployment safe.
function isValidHttpBase(value: string): boolean {
  try {
    const u = new URL(value);
    return u.protocol === "http:" || u.protocol === "https:";
  } catch {
    return false;
  }
}

/// WORKSTREAM: sentinel-viz-api — this is the ONLY cross-boundary
/// URL the web app uses. All data flows through this base. Keeping
/// every server call funnelled through here is what makes the web
/// crate cleanly peelable from the rest of the Sentinel repo.
export function apiBase(): string {
  const configured =
    typeof process !== "undefined" ? process.env?.NEXT_PUBLIC_VIZ_API : undefined;
  if (configured && isValidHttpBase(configured)) {
    return configured;
  }
  if (configured && !isValidHttpBase(configured) && typeof console !== "undefined") {
    console.warn(
      `[sentinel-viz] ignoring invalid NEXT_PUBLIC_VIZ_API (${configured}) — falling back to ${DEFAULT_BASE}`,
    );
  }
  return DEFAULT_BASE;
}

export async function fetchGraph(
  limit = 100,
  signal?: AbortSignal,
  opts: { focusedSession?: string | null } = {},
): Promise<GraphResponse> {
  const params = new URLSearchParams({ limit: String(limit) });
  if (opts.focusedSession) params.set("focused_session", opts.focusedSession);
  const url = `${apiBase()}/api/graph?${params}`;
  const res = await fetch(url, { signal });
  if (!res.ok) throw new Error(`graph: ${res.status}`);
  return res.json();
}

export async function fetchActivity(
  sessionId: string,
  opts: { limit?: number; atTs?: string; windowSecs?: number } = {},
  signal?: AbortSignal,
): Promise<ActivityResponse> {
  const params = new URLSearchParams();
  if (opts.limit != null) params.set("limit", String(opts.limit));
  if (opts.atTs) params.set("at_ts", opts.atTs);
  if (opts.windowSecs != null) params.set("window", String(opts.windowSecs));
  const qs = params.toString();
  const url = `${apiBase()}/api/activity/${encodeURIComponent(sessionId)}${qs ? `?${qs}` : ""}`;
  const res = await fetch(url, { signal });
  if (!res.ok) throw new Error(`activity: ${res.status}`);
  return res.json();
}

export async function fetchHealth(signal?: AbortSignal): Promise<HealthResponse> {
  const res = await fetch(`${apiBase()}/api/healthz`, { signal });
  if (!res.ok) throw new Error(`healthz: ${res.status}`);
  return res.json();
}

export function streamUrl(): string {
  return `${apiBase()}/api/stream`;
}

export interface NameResponse {
  session_id: string;
  name: string | null;
  source: string;
  cached: boolean;
}

export async function fetchSessionName(
  sessionId: string,
  signal?: AbortSignal,
): Promise<NameResponse> {
  const url = `${apiBase()}/api/name-session/${encodeURIComponent(sessionId)}`;
  const res = await fetch(url, { signal });
  if (!res.ok) throw new Error(`name-session: ${res.status}`);
  return res.json();
}

export interface SummaryResponse {
  session_id: string;
  kind: string;
  at_ts: string | null;
  text: string | null;
  source: string;
  cached: boolean;
}

export interface ConfigResponse {
  model: string;
  has_key: boolean;
}

export async function fetchConfig(signal?: AbortSignal): Promise<ConfigResponse> {
  const res = await fetch(`${apiBase()}/api/config`, { signal });
  if (!res.ok) throw new Error(`config: ${res.status}`);
  return res.json();
}

export async function setConfig(body: {
  model: string;
  openai_api_key?: string;
  ollama_url?: string;
}): Promise<ConfigResponse> {
  const res = await fetch(`${apiBase()}/api/config`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`config: ${res.status} ${await res.text()}`);
  return res.json();
}

export async function fetchSummary(
  sessionId: string,
  opts: { kind?: "card" | "wait" | "narrative"; atTs?: string } = {},
  signal?: AbortSignal,
): Promise<SummaryResponse> {
  const params = new URLSearchParams();
  if (opts.kind) params.set("kind", opts.kind);
  if (opts.atTs) params.set("at_ts", opts.atTs);
  const qs = params.toString();
  const url = `${apiBase()}/api/summary/${encodeURIComponent(sessionId)}${qs ? `?${qs}` : ""}`;
  const res = await fetch(url, { signal });
  if (!res.ok) throw new Error(`summary: ${res.status}`);
  return res.json();
}
