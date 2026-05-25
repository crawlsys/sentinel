import type { ActivityResponse, GraphResponse, HealthResponse } from "../types/api";

const DEFAULT_BASE = "http://127.0.0.1:8082";

export function apiBase(): string {
  if (typeof process !== "undefined" && process.env?.NEXT_PUBLIC_VIZ_API) {
    return process.env.NEXT_PUBLIC_VIZ_API;
  }
  return DEFAULT_BASE;
}

export async function fetchGraph(limit = 100, signal?: AbortSignal): Promise<GraphResponse> {
  const url = `${apiBase()}/api/graph?limit=${limit}`;
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
