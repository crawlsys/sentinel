use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One row from the bridge's `events` table.
///
/// `payload` is kept as raw JSON; typed deserialisation per `kind`
/// happens in the rollup layer as we need it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: i64,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub actor: Option<String>,
    pub payload: serde_json::Value,
    pub frame_id: Option<String>,
    pub caused_by: Option<String>,
    pub timestamp: String,
    pub run_id: String,
}

/// Event-type discriminator strings emitted by the bridge.
pub mod kind {
    pub const OBJECT_CREATED: &str = "object.created";
    pub const RELATION_CREATED: &str = "relation.created";
    pub const SESSION_STARTED: &str = "sentinel.session_started";
    pub const HOOK_INGESTED: &str = "sentinel.hook_ingested";
    pub const HOOK_DENIED: &str = "sentinel.hook_denied";
    pub const TOOL_CALL_OBSERVED: &str = "sentinel.tool_call_observed";
}

/// Node-type discriminator strings emitted by the bridge as
/// `object.created → object.type`.
pub mod node_kind {
    pub const SESSION: &str = "SentinelSession";
    pub const HOOK_INVOCATION: &str = "SentinelHookInvocation";
    pub const TOOL_CALL: &str = "SentinelToolCall";
}

/// One node in the graph (`object.created` materialisation).
///
/// Wire format matches the Python `viz_server.py` `load_graph` response,
/// extended with optional per-session liveness fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: serde_json::Value,
    pub ts: String,
    pub seq: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_status: Option<SessionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_age_s: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub awaiting_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub awaiting_question: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub awaiting_options: Option<serde_json::Value>,
    /// Coarse categorisation for `SentinelToolCall` nodes. Lets the UI
    /// colour by intent (compute vs. planning vs. communication)
    /// without inspecting `data.tool` client-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<NodeCategory>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeCategory {
    /// Bash, Read, Write, Edit, Grep, Glob, `NotebookEdit`
    Tc,
    /// TaskCreate/Update/List, `WebFetch`, `WebSearch`, plan tools
    Planning,
    /// Agent, `AskUserQuestion`, Stop
    Communication,
    /// `UserPromptSubmit` (no tool — it's a user message)
    Prompt,
    /// Anything else
    Other,
}

impl NodeCategory {
    pub fn from_tool(tool: &str, sentinel_event: Option<&str>) -> Self {
        if sentinel_event == Some("UserPromptSubmit") {
            return Self::Prompt;
        }
        match tool {
            "Bash" | "Read" | "Write" | "Edit" | "Grep" | "Glob" | "NotebookEdit"
            | "MultiEdit" => Self::Tc,
            "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet" | "TaskStop" | "TaskOutput"
            | "WebFetch" | "WebSearch" | "Plan" | "ExitPlanMode" | "EnterPlanMode" => {
                Self::Planning
            }
            "Agent" | "AskUserQuestion" | "Stop" | "ToolSearch" => Self::Communication,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Firing,
    Busy,
    Idle,
    Dormant,
    Dead,
    AwaitingUser,
}

/// One edge in the graph (`relation.created` materialisation + derived).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub source: String,
    pub target: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub ts: String,
}

/// One sentinel-domain event surfaced in the right-rail ticker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentEvent {
    pub seq: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub payload: serde_json::Value,
    pub ts: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphStats {
    pub nodes_total: usize,
    pub edges_total: usize,
    pub by_type: BTreeMap<String, usize>,
    pub by_outcome: BTreeMap<String, usize>,
    pub events_total: usize,
    pub corpus_nodes: usize,
    pub corpus_edges: usize,
    pub corpus_by_type: BTreeMap<String, usize>,
    pub corpus_by_outcome: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphResponse {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub events: Vec<RecentEvent>,
    pub max_seq: i64,
    pub window_limit: usize,
    pub stats: GraphStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub db_max_seq: i64,
    pub uptime_sec: u64,
}

// ---------- /api/activity payload ----------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActivityEvent {
    pub ts: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolCallSummary {
    pub id: String,
    pub tool: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Segment {
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_end: Option<String>,
    pub kind: String,
    pub label: String,
    pub preview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub tools: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallSummary>,
    pub tool_count: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub had_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActivityResponse {
    pub session_id: String,
    pub transcript: Option<String>,
    pub events: Vec<ActivityEvent>,
    pub segments: Vec<Segment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_segments: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
