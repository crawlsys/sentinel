use serde::{Deserialize, Serialize};

/// One row from the bridge's `events` table.
///
/// `payload` is kept as raw JSON; typed deserialisation for each
/// `type` happens in the rollup layer as we need it.
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
