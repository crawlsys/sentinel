// WORKSTREAM: sentinel-viz-api — every type below is a hand-mirror
// of a Rust struct in `tools/sentinel-viz-api/src/model.rs`. Rename
// in lockstep. The two trees are intended to peel off into a single
// `sentinel-viz` repo so they live together regardless.

export type SessionStatus =
  | "firing"
  | "busy"
  | "idle"
  | "dormant"
  | "dead"
  | "awaiting_user";

export type NodeCategory = "tc" | "planning" | "communication" | "prompt" | "other";

export interface Node {
  id: string;
  type: string;
  data: Record<string, unknown>;
  ts: string;
  seq: number;
  session_status?: SessionStatus;
  last_activity_age_s?: number | null;
  awaiting_kind?: string | null;
  awaiting_question?: string | null;
  awaiting_options?: unknown[] | null;
  category?: NodeCategory;
}

export interface Edge {
  source: string;
  target: string;
  type: string;
  ts: string;
}

export interface RecentEvent {
  seq: number;
  type: string;
  payload: Record<string, unknown>;
  ts: string;
}

export interface GraphStats {
  nodes_total: number;
  edges_total: number;
  by_type: Record<string, number>;
  by_outcome: Record<string, number>;
  events_total: number;
  corpus_nodes: number;
  corpus_edges: number;
  corpus_by_type: Record<string, number>;
  corpus_by_outcome: Record<string, number>;
}

export interface GraphResponse {
  nodes: Node[];
  edges: Edge[];
  events: RecentEvent[];
  max_seq: number;
  window_limit: number;
  stats: GraphStats;
  error?: string;
}

export interface ToolCallSummary {
  id: string;
  tool: string;
  summary: string;
  result_preview?: string;
  result_ts?: string;
  error?: boolean;
}

export interface Segment {
  ts: string;
  ts_end?: string;
  kind: "user_input" | "assistant_turn";
  label: string;
  preview: string;
  text?: string;
  tools: string[];
  tool_calls?: ToolCallSummary[];
  tool_count: number;
  had_error?: boolean;
}

export interface ActivityEvent {
  ts: string;
  kind: "user" | "assistant" | "tool_use" | "tool_result";
  text?: string;
  tool?: string;
  is_error?: boolean;
}

export interface ActivityResponse {
  session_id: string;
  transcript: string | null;
  events: ActivityEvent[];
  segments: Segment[];
  total?: number;
  total_segments?: number;
  at_ts?: string;
  window_secs?: number;
  error?: string;
}

export interface HealthResponse {
  ok: boolean;
  db_max_seq: number;
  uptime_sec: number;
}
