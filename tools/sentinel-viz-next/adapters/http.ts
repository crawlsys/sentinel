import type { ActivityResponse, GraphResponse, HealthResponse } from "../types/api";

const DEFAULT_BASE = "http://127.0.0.1:8082";

/// WORKSTREAM: sentinel-viz-api — this is the ONLY cross-boundary
/// URL the web app uses. All data flows through this base. Keeping
/// every server call funnelled through here is what makes the web
/// crate cleanly peelable from the rest of the Sentinel repo.
export function apiBase(): string {
  if (typeof process !== "undefined" && process.env?.NEXT_PUBLIC_VIZ_API) {
    return process.env.NEXT_PUBLIC_VIZ_API;
  }
  return DEFAULT_BASE;
}

export async function fetchGraph(
  // Matches backend GraphOpts::default().limit. Needs enough event
  // history for session sparklines, not just the latest hook burst.
  limit = 6_000,
  signal?: AbortSignal,
  opts: { focusedSession?: string | null } = {},
): Promise<GraphResponse> {
  const params = new URLSearchParams({ limit: String(limit) });
  if (opts.focusedSession) params.set("focused_session", opts.focusedSession);
  // include_hooks=true so sentinel.hook_ingested events flow through
  // the response. The dashboard's session-strip panel groups by
  // session_id from these events; without them, the strip list is
  // empty for everything except sessions with a (rare) leaked
  // sentinel.session_started event.
  params.set("include_hooks", "true");
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
  return `${apiBase()}/api/stream?include_hooks=true`;
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

export interface RollupMember {
  tool: string;
  /** Pre-extracted summary line from the activity-cache, or "" when
   *  the client doesn't have one yet. Server falls back to the bare
   *  tool name in that case. */
  summary: string;
}

export interface RollupSummaryRequest {
  cache_key: string;
  session_id: string;
  members: RollupMember[];
}

export interface RollupSummaryResponse {
  cache_key: string;
  session_id: string;
  /** 5-10 word blurb, or null when the LLM is unavailable / failed. */
  summary: string | null;
  /** Diagnostic — "cache", "llm:<model>", "no-model", "no-members",
   *  "llm-error". Useful for surfacing why a blurb didn't appear. */
  source: string;
  cached: boolean;
}

export async function fetchRollupSummary(
  req: RollupSummaryRequest,
  signal?: AbortSignal,
): Promise<RollupSummaryResponse> {
  const res = await fetch(`${apiBase()}/api/rollup-summary`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(req),
    signal,
  });
  if (!res.ok) throw new Error(`rollup-summary: ${res.status}`);
  return res.json();
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

// ─────────────────────────────────────────────────────────────────
// Port adapter object
//
// Bundles the free functions above into a single object that
// satisfies the port interfaces from ports/repos.ts. Components
// still import the free functions directly today; the bundled
// adapter is the seam a future ServicesProvider context will plug
// into when a second adapter (mock, WebSocket, etc.) exists.

import type {
  ActivityRepo,
  ConfigRepo,
  GraphRepo,
  HealthRepo,
  KpiRepo,
  SessionNameRepo,
  SummaryRepo,
} from "../ports/repos";

async function fetchKpis(): ReturnType<KpiRepo["fetchKpis"]> {
  const res = await fetch(`${apiBase()}/api/kpis`);
  if (!res.ok) throw new Error(`kpis: ${res.status}`);
  return res.json();
}

export const httpAdapter: GraphRepo &
  ActivityRepo &
  SummaryRepo &
  ConfigRepo &
  SessionNameRepo &
  HealthRepo &
  KpiRepo = {
  fetchGraph,
  streamUrl,
  fetchActivity,
  fetchSummary,
  fetchConfig,
  setConfig,
  fetchSessionName,
  fetchHealth,
  fetchKpis,
};
