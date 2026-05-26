/// Port interfaces for the sentinel-viz domain.
///
/// The dashboard's UI and domain layers depend only on these
/// interfaces — concrete implementations live under adapters/.
/// Properties this gives us:
///
///   1. Domain code is testable without spinning up an HTTP server.
///      A mock impl satisfies the port.
///   2. The HTTP adapter can be swapped for a WebSocket / IPC / file
///      adapter without touching components.
///   3. Future caching layers (e.g. service-worker, IndexedDB) slot
///      in as decorators around the same port.
///
/// Concrete impl in this codebase: adapters/http.ts exports the
/// `httpAdapter` object that satisfies all of these against the
/// Rust viz-api server.
///
/// Components currently import the free functions from adapters/
/// directly. The interfaces here are a forcing function for
/// keeping the boundary clean. A ServicesProvider context is the
/// natural next step once a second adapter exists.

import type {
  ActivityResponse,
  GraphResponse,
  HealthResponse,
} from "../types/api";

export interface NameResponse {
  session_id: string;
  name: string | null;
  source: string;
  cached: boolean;
}

export interface ConfigResponse {
  model: string;
  has_key: boolean;
  has_openrouter_key?: boolean;
}

export interface SummaryResponse {
  session_id: string;
  text: string | null;
  source: string;
  kind?: string;
}

export interface KpiResponse {
  sessions_active: number;
  sessions_total: number;
  events_5m: number;
  events_per_min: number;
  tokens_5m: {
    input: number;
    cache_creation: number;
    cache_read: number;
    output: number;
  } | null;
  usd_5m: number | null;
  stuck_count: number;
}

export interface GraphRepo {
  fetchGraph(
    limit?: number,
    signal?: AbortSignal,
    opts?: { focusedSession?: string | null },
  ): Promise<GraphResponse>;
  streamUrl(): string;
}

export interface ActivityRepo {
  fetchActivity(
    sessionId: string,
    opts?: { limit?: number; atTs?: string; windowSecs?: number },
    signal?: AbortSignal,
  ): Promise<ActivityResponse>;
}

export interface SummaryRepo {
  fetchSummary(
    sessionId: string,
    opts?: { kind?: "card" | "wait" | "narrative"; atTs?: string },
    signal?: AbortSignal,
  ): Promise<SummaryResponse>;
}

export interface ConfigRepo {
  fetchConfig(signal?: AbortSignal): Promise<ConfigResponse>;
  setConfig(body: {
    model: string;
    openai_api_key?: string;
    openrouter_api_key?: string;
    ollama_url?: string;
  }): Promise<ConfigResponse>;
}

export interface SessionNameRepo {
  fetchSessionName(sessionId: string): Promise<NameResponse>;
}

export interface HealthRepo {
  fetchHealth(signal?: AbortSignal): Promise<HealthResponse>;
}

export interface KpiRepo {
  fetchKpis(): Promise<KpiResponse>;
}
