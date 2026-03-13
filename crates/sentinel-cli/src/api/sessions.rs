//! Sentinel Session API Endpoints
//!
//! GET  /sessions          — list all session summaries
//! GET  /sessions/:id      — full session state
//! GET  /config            — hooks.toml + workflows.toml summary
//! GET  /stats             — aggregated stats across all sessions
//!
//! Reads session state JSON files from ~/.claude/sentinel/state/

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::Path;
use axum::routing::get;
use axum::{Json, Router};

use super::AppState;

const CACHE_TTL: Duration = Duration::from_secs(5);

static SESSION_CACHE: Mutex<Option<(Instant, Vec<SessionSummary>)>> = Mutex::new(None);

fn state_dir() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory")
        .join(".claude")
        .join("sentinel")
        .join("state")
}

fn config_dir() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory")
        .join(".claude")
        .join("sentinel")
        .join("config")
}

#[derive(Clone, serde::Serialize)]
struct SessionSummary {
    id: String,
    file: String,
    started_at: Option<String>,
    active: bool,
    active_skill: Option<String>,
    // **Attack #148 fix**: Use u64 to match SessionState's field type.
    // u32 truncation silently wraps counts above 4.3 billion, corrupting audit data.
    tool_calls: u64,
    phases_read: Vec<String>,
    workflow_count: usize,
    proof_chain_count: usize,
    hook_stats: Option<serde_json::Value>,
}

fn load_sessions() -> Vec<SessionSummary> {
    // **Attack #150 fix**: Use HMAC-verified state_store::load() instead of raw
    // fs::read_to_string(). Raw reads bypass HMAC verification, allowing an attacker
    // with filesystem access to inject forged session state into the dashboard.
    let dir = state_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // Only process .json files (skip .sig files)
        if path.extension().is_some_and(|e| e == "json") {
            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let session_id = file_name.trim_end_matches(".json");

            // Use HMAC-verified load
            let state = match sentinel_infrastructure::state_store::load(session_id) {
                Ok(Some(s)) => s,
                _ => continue, // Skip invalid, tampered, or unsigned state files
            };

            // Convert phases_read HashMap<String, Vec<String>> to flat list
            let phases_flat: Vec<String> = state
                .phases_read
                .values()
                .flat_map(|v| v.iter().cloned())
                .collect();

            sessions.push(SessionSummary {
                id: state.session_id.clone(),
                file: file_name,
                started_at: Some(state.started_at.to_rfc3339()),
                active: state.active,
                active_skill: state.active_skill.clone(),
                tool_calls: state.tool_calls,
                phases_read: phases_flat,
                workflow_count: state.workflows.len(),
                proof_chain_count: state.proof_chains.len(),
                hook_stats: serde_json::to_value(&state.hook_stats).ok(),
            });
        }
    }

    // Sort by started_at descending (most recent first)
    sessions.sort_by(|a, b| {
        let ta = a.started_at.as_deref().unwrap_or("");
        let tb = b.started_at.as_deref().unwrap_or("");
        tb.cmp(ta)
    });

    sessions
}

fn get_cached_sessions() -> Vec<SessionSummary> {
    let mut cache = SESSION_CACHE.lock().unwrap();
    if let Some((ts, ref data)) = *cache {
        if ts.elapsed() < CACHE_TTL {
            return data.clone();
        }
    }
    let sessions = load_sessions();
    *cache = Some((Instant::now(), sessions.clone()));
    sessions
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session))
        .route("/config", get(get_config))
        .route("/stats", get(get_stats))
}

/// **Attack #172 fix**: Default and max limits for session listing.
/// Prevents DoS from loading thousands of session state files.
const DEFAULT_SESSION_LIMIT: usize = 100;
const MAX_SESSION_LIMIT: usize = 500;

async fn list_sessions(
    query: axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Vec<SessionSummary>> {
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_SESSION_LIMIT)
        .min(MAX_SESSION_LIMIT);

    let offset = query
        .get("offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    let sessions = get_cached_sessions();
    let paginated: Vec<SessionSummary> = sessions.into_iter().skip(offset).take(limit).collect();

    Json(paginated)
}

async fn get_session(
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    // **Attack #146 fix**: Sanitize session ID to prevent path traversal.
    // **Attack #151 fix**: Use HMAC-verified state_store::load() instead of raw
    // fs::read_to_string(). The raw read bypassed HMAC verification, allowing an
    // attacker with filesystem access to inject forged session state into the API.
    let state = sentinel_infrastructure::state_store::load(&id)
        .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?
        .ok_or(axum::http::StatusCode::NOT_FOUND)?;

    let json =
        serde_json::to_value(&state).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json))
}

async fn get_config() -> Json<serde_json::Value> {
    let config = config_dir();
    let mut result = serde_json::json!({
        "hooks": [],
        "workflows": [],
        "hooksTomlExists": false,
        "workflowsTomlExists": false,
    });

    let hooks_path = config.join("hooks.toml");
    if hooks_path.exists() {
        result["hooksTomlExists"] = serde_json::json!(true);
        if let Ok(content) = fs::read_to_string(&hooks_path) {
            // Parse hooks from TOML — extract [[hooks]] entries
            let hooks = parse_hooks_entries(&content);
            result["hooks"] = serde_json::json!(hooks);
        }
    }

    let workflows_path = config.join("workflows.toml");
    if workflows_path.exists() {
        result["workflowsTomlExists"] = serde_json::json!(true);
        if let Ok(content) = fs::read_to_string(&workflows_path) {
            // Count [[workflows]] occurrences
            let count = content.matches("[[workflows]]").count();
            result["workflowCount"] = serde_json::json!(count);
        }
    }

    Json(result)
}

/// Parse [[hooks]] entries from hooks.toml into JSON values
fn parse_hooks_entries(content: &str) -> Vec<serde_json::Value> {
    let mut hooks = Vec::new();
    let mut current: Option<serde_json::Map<String, serde_json::Value>> = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[[hooks]]" {
            if let Some(hook) = current.take() {
                hooks.push(serde_json::Value::Object(hook));
            }
            current = Some(serde_json::Map::new());
            continue;
        }
        if let Some(ref mut hook) = current {
            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim().to_string();
                let value = value.trim();
                // Parse the value
                if value == "true" {
                    hook.insert(key, serde_json::json!(true));
                } else if value == "false" {
                    hook.insert(key, serde_json::json!(false));
                } else if value.starts_with('"') && value.ends_with('"') {
                    hook.insert(key, serde_json::json!(&value[1..value.len() - 1]));
                } else if value.starts_with('[') {
                    // Simple array parsing for string arrays like ["a", "b"]
                    let inner = &value[1..value.len().saturating_sub(1)];
                    let items: Vec<String> = inner
                        .split(',')
                        .map(|s| s.trim().trim_matches('"').to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    hook.insert(key, serde_json::json!(items));
                } else if let Ok(n) = value.parse::<i64>() {
                    hook.insert(key, serde_json::json!(n));
                } else {
                    hook.insert(key, serde_json::json!(value));
                }
            }
        }
    }
    if let Some(hook) = current {
        hooks.push(serde_json::Value::Object(hook));
    }

    hooks
}

async fn get_stats() -> Json<serde_json::Value> {
    let sessions = get_cached_sessions();

    let mut total_proof_chains: usize = 0;
    let mut total_hook_invocations: u64 = 0;
    let mut total_blocked: u64 = 0;
    let mut hook_timings: HashMap<String, (u64, u64)> = HashMap::new(); // (total_ms, count)
    let mut skill_usage: HashMap<String, u64> = HashMap::new();

    for session in &sessions {
        total_proof_chains += session.proof_chain_count;

        if let Some(ref stats) = session.hook_stats {
            total_hook_invocations += stats["total_invocations"].as_u64().unwrap_or(0);
            total_blocked += stats["total_blocked"].as_u64().unwrap_or(0);

            if let Some(per_hook_time) = stats["per_hook_time_ms"].as_object() {
                for (hook, ms) in per_hook_time {
                    let ms_val = ms.as_u64().unwrap_or(0);
                    let per_hook_count = stats["per_hook"]
                        .as_object()
                        .and_then(|ph| ph.get(hook))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(1);

                    let entry = hook_timings.entry(hook.clone()).or_insert((0, 0));
                    entry.0 += ms_val;
                    entry.1 += per_hook_count;
                }
            }
        }

        if let Some(ref skill) = session.active_skill {
            *skill_usage.entry(skill.clone()).or_insert(0) += 1;
        }
    }

    let hook_avg_ms: HashMap<String, u64> = hook_timings
        .into_iter()
        .map(|(hook, (total_ms, count))| {
            let avg = if count > 0 { total_ms / count } else { 0 };
            (hook, avg)
        })
        .collect();

    Json(serde_json::json!({
        "total_sessions": sessions.len(),
        "active_sessions": sessions.iter().filter(|s| s.active).count(),
        "total_proof_chains": total_proof_chains,
        "total_hook_invocations": total_hook_invocations,
        "total_blocked": total_blocked,
        "hook_avg_ms": hook_avg_ms,
        "skill_usage": skill_usage,
    }))
}
