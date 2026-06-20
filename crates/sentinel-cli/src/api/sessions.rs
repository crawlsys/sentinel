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
use std::io::{ErrorKind, Write as _};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use sentinel_domain::state::HookStats;
use sentinel_domain::workflow::SkillWorkflow;
use sentinel_infrastructure::operational_api_read_graph::OperationalApiReadSurface;
use sentinel_infrastructure::session_api_read_graph::{SessionApiReadGraph, SessionApiReadSurface};

use super::{operational_read_audit, AppState};
use crate::phase_graph_projection::{graph_projected_workflows, load_workflow_configs};

const CACHE_TTL: Duration = Duration::from_secs(5);

static SESSION_CACHE: Mutex<Option<(Instant, Vec<SessionSummary>)>> = Mutex::new(None);

#[derive(Clone, Debug, serde::Serialize)]
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
    langgraph_workflow_count: usize,
    workflow_authority: String,
    proof_chain_count: usize,
    hook_stats: HookStats,
}

fn load_sessions() -> Result<Vec<SessionSummary>, String> {
    // **Attack #150 fix**: Use HMAC-verified state_store::load() instead of raw
    // fs::read_to_string(). Raw reads bypass HMAC verification, allowing an attacker
    // with filesystem access to inject forged session state into the local API.
    let dir = sentinel_infrastructure::state_store::state_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(format!(
                "failed to read Sentinel state directory '{}': {err}",
                dir.display()
            ));
        }
    };

    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "failed to read Sentinel state directory entry in '{}': {err}",
                dir.display()
            )
        })?;
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
                Ok(Some(state)) => state,
                Ok(None) => {
                    return Err(format!(
                        "LangGraph session authority state '{}' at '{}' could not be loaded; \
                         state_store rejected or removed it as missing, unsupported, or tampered",
                        session_id,
                        path.display()
                    ));
                }
                Err(err) => {
                    return Err(format!(
                        "LangGraph session authority state '{}' at '{}' failed authenticated load: {err}",
                        session_id,
                        path.display()
                    ));
                }
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
                langgraph_workflow_count: 0,
                workflow_authority: "langgraph".to_string(),
                proof_chain_count: state.proof_chain_count(),
                hook_stats: state.hook_stats.clone(),
            });
        }
    }

    // Sort by started_at descending (most recent first)
    sessions.sort_by(|a, b| {
        let ta = a.started_at.as_deref().unwrap_or("");
        let tb = b.started_at.as_deref().unwrap_or("");
        tb.cmp(ta)
    });

    Ok(sessions)
}

async fn project_session_langgraph_workflow_counts(
    sessions: &mut [SessionSummary],
    workflow_configs: &HashMap<String, SkillWorkflow>,
) -> std::result::Result<(), String> {
    for session in sessions {
        let workflows = graph_projected_workflows(&session.id, workflow_configs)
            .await
            .map_err(|e| {
                format!(
                    "LangGraph workflow projection failed for session '{}': {e}",
                    session.id
                )
            })?;
        session.langgraph_workflow_count = workflows.len();
    }
    Ok(())
}

fn get_cached_sessions() -> Result<Vec<SessionSummary>, String> {
    let mut cache = SESSION_CACHE.lock().unwrap();
    if let Some((ts, ref data)) = *cache {
        if ts.elapsed() < CACHE_TTL {
            return Ok(data.clone());
        }
    }
    let sessions = load_sessions()?;
    *cache = Some((Instant::now(), sessions.clone()));
    Ok(sessions)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session))
        .route("/config", get(get_config))
        .route("/stats", get(get_stats))
}

/// **Attack #172 fix**: Default and max limits for session listing.
/// Prevents `DoS` from loading thousands of session state files.
const DEFAULT_SESSION_LIMIT: usize = 100;
const MAX_SESSION_LIMIT: usize = 500;

async fn list_sessions(
    query: axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Vec<serde_json::Value>>, axum::http::StatusCode> {
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_SESSION_LIMIT)
        .min(MAX_SESSION_LIMIT);

    let offset = query
        .get("offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    let workflow_configs =
        load_workflow_configs().map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let sessions =
        get_cached_sessions().map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut paginated: Vec<SessionSummary> =
        sessions.into_iter().skip(offset).take(limit).collect();
    project_session_langgraph_workflow_counts(&mut paginated, &workflow_configs)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let response = attach_session_api_summary_graph_audits(paginated)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(response))
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

    let mut json =
        serde_json::to_value(&state).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let workflow_configs =
        load_workflow_configs().map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let workflows = graph_projected_workflows(&state.session_id, &workflow_configs)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let langgraph_workflow_count = workflows.len();
    if let Some(obj) = json.as_object_mut() {
        obj.remove("workflows");
        obj.insert(
            "langgraph_workflows".to_string(),
            serde_json::to_value(workflows)
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?,
        );
        obj.insert(
            "langgraph_workflow_count".to_string(),
            serde_json::json!(langgraph_workflow_count),
        );
        obj.insert(
            "workflow_authority".to_string(),
            serde_json::json!("langgraph"),
        );
    }
    json = attach_session_api_read_graph_audit(SessionApiReadSurface::Detail, json)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json))
}

async fn attach_session_api_summary_graph_audits(
    summaries: Vec<SessionSummary>,
) -> std::result::Result<Vec<serde_json::Value>, String> {
    let graph = sentinel_infrastructure::session_api_read_graph::build_session_api_read_graph()
        .await
        .map_err(|e| format!("build session API read graph: {e}"))?;
    let mut responses = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let value = serde_json::to_value(summary)
            .map_err(|e| format!("serialize session summary response: {e}"))?;
        responses.push(
            attach_session_api_read_graph_audit_with_graph(
                &graph,
                SessionApiReadSurface::Summary,
                value,
            )
            .await?,
        );
    }
    Ok(responses)
}

async fn attach_session_api_read_graph_audit(
    surface: SessionApiReadSurface,
    response: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let graph = sentinel_infrastructure::session_api_read_graph::build_session_api_read_graph()
        .await
        .map_err(|e| format!("build session API read graph: {e}"))?;
    attach_session_api_read_graph_audit_with_graph(&graph, surface, response).await
}

async fn attach_session_api_read_graph_audit_with_graph(
    graph: &SessionApiReadGraph,
    surface: SessionApiReadSurface,
    mut response: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let graph_audit = run_session_api_read_graph_audit(graph, surface, &response).await?;
    response
        .as_object_mut()
        .ok_or_else(|| {
            "session API read graph audit can only attach to object responses".to_string()
        })?
        .insert("graph_audit".to_string(), graph_audit);
    Ok(response)
}

async fn run_session_api_read_graph_audit(
    graph: &SessionApiReadGraph,
    surface: SessionApiReadSurface,
    response: &serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let response_hash = sentinel_infrastructure::session_api_read_graph::sha256_json(response);
    let session_id = response
        .get("id")
        .or_else(|| response.get("session_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
        .ok_or_else(|| "session API read graph audit requires session id".to_string())?;
    let surface_label =
        sentinel_infrastructure::session_api_read_graph::session_api_read_surface_label(surface);
    let identifier = format!("{surface_label}-{session_id}:response-{response_hash}");
    let state = sentinel_infrastructure::session_api_read_graph::SessionApiReadState::from_response(
        surface, identifier, response,
    );
    let run =
        sentinel_infrastructure::session_api_read_graph::run_session_api_read_decision_report(
            graph, state,
        )
        .await
        .map_err(|e| format!("run session API read graph: {e}"))?;
    let authorization = run
        .session_api_read_authorization()
        .map_err(|e| format!("session API read graph authorization failed: {e}"))?
        .ok_or_else(|| "session API read graph produced no terminal checkpoint".to_string())?;
    let graph_runs = sentinel_infrastructure::paths::sentinel_root()
        .join("metrics")
        .join("session-api-read.graph-runs.jsonl");
    if let Some(parent) = graph_runs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "create session API read graph audit dir {}: {e}",
                parent.display()
            )
        })?;
    }
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "session_api_read",
        "surface": surface_label,
        "response_sha256": response_hash,
        "decision": sentinel_infrastructure::session_api_read_graph::
            session_api_read_decision_label(authorization.decision()),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": run.thread_id.clone(),
        "run": run,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&graph_runs)
        .map_err(|e| {
            format!(
                "open session API read graph audit {}: {e}",
                graph_runs.display()
            )
        })?;
    serde_json::to_writer(&mut file, &row).map_err(|e| {
        format!(
            "write session API read graph audit {}: {e}",
            graph_runs.display()
        )
    })?;
    file.write_all(b"\n").map_err(|e| {
        format!(
            "terminate session API read graph audit {}: {e}",
            graph_runs.display()
        )
    })?;
    Ok(serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "session_api_read",
        "surface": surface_label,
        "graph_runs_path": graph_runs,
        "response_sha256": row["response_sha256"].clone(),
        "decision": row["decision"].clone(),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": row["thread_id"].clone(),
        "run": row["run"].clone(),
    }))
}

async fn get_config() -> Result<Json<serde_json::Value>, StatusCode> {
    let result = match authoritative_config_json() {
        Ok(result) => result,
        Err(error) => config_error_json(error),
    };

    operational_read_audit::attach_operational_api_read_graph_audit(
        OperationalApiReadSurface::Config,
        result,
    )
    .await
    .map(Json)
    .map_err(|error| {
        tracing::error!(
            error = %error,
            "sentinel config API read graph audit failed; refusing unaudited response"
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

fn authoritative_config_json() -> std::result::Result<serde_json::Value, String> {
    let config = sentinel_infrastructure::config::config_dir();
    let hooks_path = config.join("hooks.toml");
    let workflows_path = config.join("workflows.toml");
    let hooks = sentinel_infrastructure::config::load_hooks(&config)
        .map_err(|e| format!("authoritative hooks.toml load failed: {e:#}"))?;
    let workflows = sentinel_infrastructure::config::load_workflows(&config)
        .map_err(|e| format!("authoritative workflows.toml load failed: {e:#}"))?;
    Ok(serde_json::json!({
        "graph_authority": "langgraph",
        "hooksTomlExists": hooks_path.exists(),
        "workflowsTomlExists": workflows_path.exists(),
        "hookCount": hooks.len(),
        "workflowCount": workflows.len(),
        "hooks": hooks,
        "workflows": workflows,
    }))
}

fn config_error_json(error: String) -> serde_json::Value {
    let config = sentinel_infrastructure::config::config_dir();
    serde_json::json!({
        "error": error,
        "hooksTomlExists": config.join("hooks.toml").exists(),
        "workflowsTomlExists": config.join("workflows.toml").exists(),
    })
}

async fn get_stats() -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let workflow_configs = match load_workflow_configs() {
        Ok(configs) => configs,
        Err(e) => {
            tracing::error!(
                error = %e,
                "session stats workflow config load failed; refusing unaudited response"
            );
            return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let mut sessions = match get_cached_sessions() {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::error!(
                error = %e,
                "session stats load failed; refusing unaudited response"
            );
            return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    if let Err(e) =
        project_session_langgraph_workflow_counts(&mut sessions, &workflow_configs).await
    {
        tracing::error!(
            error = %e,
            "session stats LangGraph projection failed; refusing unaudited response"
        );
        return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    let mut total_proof_chains: usize = 0;
    let mut total_hook_invocations: u64 = 0;
    let mut total_blocked: u64 = 0;
    let mut hook_timings: HashMap<String, (u64, u64)> = HashMap::new(); // (total_ms, count)
    let mut skill_usage: HashMap<String, u64> = HashMap::new();

    for session in &sessions {
        total_proof_chains += session.proof_chain_count;

        total_hook_invocations += session.hook_stats.total_invocations;
        total_blocked += session.hook_stats.total_blocked;

        for (hook, ms_val) in &session.hook_stats.per_hook_time_ms {
            let per_hook_count = *session.hook_stats.per_hook.get(hook).ok_or_else(|| {
                tracing::error!(
                    session_id = %session.id,
                    hook,
                    "session stats hook timing has no matching invocation count; refusing unaudited aggregate"
                );
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            })?;
            if per_hook_count == 0 {
                tracing::error!(
                    session_id = %session.id,
                    hook,
                    "session stats hook timing has zero invocation count; refusing unaudited aggregate"
                );
                return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            }

            let entry = hook_timings.entry(hook.clone()).or_insert((0, 0));
            entry.0 += *ms_val;
            entry.1 += per_hook_count;
        }

        if let Some(ref skill) = session.active_skill {
            *skill_usage.entry(skill.clone()).or_insert(0) += 1;
        }
    }

    let hook_avg_ms: HashMap<String, u64> = hook_timings
        .into_iter()
        .map(|(hook, (total_ms, count))| {
            let avg = total_ms / count;
            (hook, avg)
        })
        .collect();
    let total_langgraph_workflows: usize =
        sessions.iter().map(|s| s.langgraph_workflow_count).sum();
    let sessions_with_langgraph_workflows = sessions
        .iter()
        .filter(|s| s.langgraph_workflow_count > 0)
        .count();

    let mut stats = serde_json::json!({
        "total_sessions": sessions.len(),
        "active_sessions": sessions.iter().filter(|s| s.active).count(),
        "workflow_authority": "langgraph",
        "total_langgraph_workflows": total_langgraph_workflows,
        "sessions_with_langgraph_workflows": sessions_with_langgraph_workflows,
        "total_proof_chains": total_proof_chains,
        "total_hook_invocations": total_hook_invocations,
        "total_blocked": total_blocked,
        "hook_avg_ms": hook_avg_ms,
        "skill_usage": skill_usage,
    });

    match run_session_api_stats_graph_audit(&stats).await {
        Ok(graph_audit) => {
            if let Some(obj) = stats.as_object_mut() {
                obj.insert("graph_audit".to_string(), graph_audit);
            }
            Ok(Json(stats))
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "session API stats graph audit failed; refusing unaudited response"
            );
            Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn run_session_api_stats_graph_audit(
    stats: &serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let stats_hash = sentinel_infrastructure::session_api_stats_graph::sha256_json(stats);
    let identifier = format!("aggregate-stats-{stats_hash}");
    let state =
        sentinel_infrastructure::session_api_stats_graph::SessionApiStatsState::from_response(
            identifier, stats,
        );
    let graph = sentinel_infrastructure::session_api_stats_graph::build_session_api_stats_graph()
        .await
        .map_err(|e| format!("build session API stats graph: {e}"))?;
    let run =
        sentinel_infrastructure::session_api_stats_graph::run_session_api_stats_decision_report(
            &graph, state,
        )
        .await
        .map_err(|e| format!("run session API stats graph: {e}"))?;
    let authorization = run
        .session_api_stats_authorization()
        .map_err(|e| format!("session API stats graph authorization failed: {e}"))?
        .ok_or_else(|| "session API stats graph produced no terminal checkpoint".to_string())?;
    let graph_runs = sentinel_infrastructure::paths::sentinel_root()
        .join("metrics")
        .join("session-api-stats.graph-runs.jsonl");
    if let Some(parent) = graph_runs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "create session API stats graph audit dir {}: {e}",
                parent.display()
            )
        })?;
    }
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "session_api_stats",
        "stats_sha256": stats_hash,
        "decision": sentinel_infrastructure::session_api_stats_graph::
            session_api_stats_decision_label(authorization.decision()),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": run.thread_id.clone(),
        "run": run,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&graph_runs)
        .map_err(|e| {
            format!(
                "open session API stats graph audit {}: {e}",
                graph_runs.display()
            )
        })?;
    serde_json::to_writer(&mut file, &row).map_err(|e| {
        format!(
            "write session API stats graph audit {}: {e}",
            graph_runs.display()
        )
    })?;
    file.write_all(b"\n").map_err(|e| {
        format!(
            "terminate session API stats graph audit {}: {e}",
            graph_runs.display()
        )
    })?;
    Ok(serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "session_api_stats",
        "graph_runs_path": graph_runs,
        "stats_sha256": row["stats_sha256"].clone(),
        "decision": row["decision"].clone(),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": row["thread_id"].clone(),
        "run": row["run"].clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::judge::JudgeModel;
    use sentinel_domain::state::SessionState;
    use sentinel_domain::workflow::{SkillWorkflow, WorkflowPhase};

    struct EnvGuard {
        previous_home: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_sentinel_home(path: &std::path::Path) -> Self {
            let previous_home = std::env::var_os("SENTINEL_HOME");
            std::env::set_var("SENTINEL_HOME", path);
            Self { previous_home }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous_home.take() {
                Some(value) => std::env::set_var("SENTINEL_HOME", value),
                None => std::env::remove_var("SENTINEL_HOME"),
            }
        }
    }

    struct CheckpointerEnvGuard {
        previous_phase_backend: Option<std::ffi::OsString>,
        previous_phase_pg_url: Option<std::ffi::OsString>,
        previous_phase_pg_schema: Option<std::ffi::OsString>,
        previous_decision_backend: Option<std::ffi::OsString>,
        previous_decision_pg_url: Option<std::ffi::OsString>,
        previous_decision_pg_schema: Option<std::ffi::OsString>,
    }

    impl CheckpointerEnvGuard {
        fn force_sqlite() -> Self {
            let guard = Self {
                previous_phase_backend: std::env::var_os("SENTINEL_PHASE_GRAPH_CHECKPOINTER"),
                previous_phase_pg_url: std::env::var_os("SENTINEL_PHASE_GRAPH_POSTGRES_URL"),
                previous_phase_pg_schema: std::env::var_os("SENTINEL_PHASE_GRAPH_POSTGRES_SCHEMA"),
                previous_decision_backend: std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER"),
                previous_decision_pg_url: std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_URL"),
                previous_decision_pg_schema: std::env::var_os(
                    "SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA",
                ),
            };
            std::env::set_var("SENTINEL_PHASE_GRAPH_CHECKPOINTER", "sqlite");
            std::env::remove_var("SENTINEL_PHASE_GRAPH_POSTGRES_URL");
            std::env::remove_var("SENTINEL_PHASE_GRAPH_POSTGRES_SCHEMA");
            std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
            std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
            std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");
            guard
        }

        fn unsupported_decision_backend() -> Self {
            let guard = Self {
                previous_phase_backend: std::env::var_os("SENTINEL_PHASE_GRAPH_CHECKPOINTER"),
                previous_phase_pg_url: std::env::var_os("SENTINEL_PHASE_GRAPH_POSTGRES_URL"),
                previous_phase_pg_schema: std::env::var_os("SENTINEL_PHASE_GRAPH_POSTGRES_SCHEMA"),
                previous_decision_backend: std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER"),
                previous_decision_pg_url: std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_URL"),
                previous_decision_pg_schema: std::env::var_os(
                    "SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA",
                ),
            };
            std::env::set_var("SENTINEL_PHASE_GRAPH_CHECKPOINTER", "sqlite");
            std::env::remove_var("SENTINEL_PHASE_GRAPH_POSTGRES_URL");
            std::env::remove_var("SENTINEL_PHASE_GRAPH_POSTGRES_SCHEMA");
            std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "unsupported");
            std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
            std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");
            guard
        }
    }

    fn restore_env_var(name: &str, value: &Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    impl Drop for CheckpointerEnvGuard {
        fn drop(&mut self) {
            restore_env_var(
                "SENTINEL_PHASE_GRAPH_CHECKPOINTER",
                &self.previous_phase_backend,
            );
            restore_env_var(
                "SENTINEL_PHASE_GRAPH_POSTGRES_URL",
                &self.previous_phase_pg_url,
            );
            restore_env_var(
                "SENTINEL_PHASE_GRAPH_POSTGRES_SCHEMA",
                &self.previous_phase_pg_schema,
            );
            restore_env_var(
                "SENTINEL_DECISION_GRAPH_CHECKPOINTER",
                &self.previous_decision_backend,
            );
            restore_env_var(
                "SENTINEL_DECISION_GRAPH_POSTGRES_URL",
                &self.previous_decision_pg_url,
            );
            restore_env_var(
                "SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA",
                &self.previous_decision_pg_schema,
            );
        }
    }

    fn clear_session_cache() {
        *SESSION_CACHE.lock().expect("session cache lock") = None;
    }

    fn workflow() -> SkillWorkflow {
        SkillWorkflow {
            skill: "linear".to_string(),
            phases: vec![WorkflowPhase {
                id: "claim".to_string(),
                file: "claim.md".to_string(),
                required: true,
                judge: JudgeModel::Sonnet,
                description: "Claim".to_string(),
                required_dyad: None,
            }],
            blocked_tool_prefixes: Vec::new(),
            blocked_bash_patterns: Vec::new(),
            bash_allowlist: Vec::new(),
        }
    }

    fn write_workflow_config() {
        let config_dir = sentinel_infrastructure::config::config_dir();
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .expect("workflow config");
    }

    #[test]
    fn load_sessions_reads_authoritative_state_dir() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        clear_session_cache();

        let mut state = SessionState::new("api-session-root");
        state.active_skill = Some("linear".to_string());
        state.tool_calls = 42;
        sentinel_infrastructure::state_store::save(&mut state).expect("save session state");

        let sessions = load_sessions().expect("load sessions");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "api-session-root");
        assert_eq!(sessions[0].tool_calls, 42);
        assert_eq!(
            sentinel_infrastructure::state_store::state_dir(),
            tmp.path().join(".claude").join("sentinel").join("state")
        );
    }

    #[test]
    fn load_sessions_fails_visible_when_state_store_rejects_existing_state_file() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        clear_session_cache();

        let state_dir = sentinel_infrastructure::state_store::state_dir();
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let path = state_dir.join("tampered-session.json");
        std::fs::write(&path, br#"{"session_id":"tampered-session"}"#)
            .expect("tampered state file");

        let err = load_sessions().expect_err("tampered state must fail visible");

        assert!(err.contains("LangGraph session authority state"));
        assert!(err.contains("tampered-session"));
        assert!(err.contains("could not be loaded"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_config_reads_authoritative_config_dir() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();

        let config_dir = sentinel_infrastructure::config::config_dir();
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("hooks.toml"),
            r#"
[[hooks]]
id = "phase_gate"
event = "PreToolUse"
"#,
        )
        .expect("hooks config");
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .expect("workflows config");

        let Json(config) = get_config().await.expect("config graph audit");

        assert_eq!(config["hooksTomlExists"], true);
        assert_eq!(config["workflowsTomlExists"], true);
        assert_eq!(config["hookCount"], 1);
        assert_eq!(config["workflowCount"], 1);
        assert_eq!(config["hooks"][0]["id"], "phase_gate");
        assert_eq!(config["workflows"][0]["skill"], "linear");
        assert_eq!(config["graph_authority"], "langgraph");
        assert_eq!(config["graph_audit"]["graph"], "operational_api_read");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_config_reports_authoritative_config_errors_without_synthesized_counts() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();

        let config_dir = sentinel_infrastructure::config::config_dir();
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("hooks.toml"),
            r#"
[[hooks]]
id = "phase_gate"
event = "PreToolUse"
"#,
        )
        .expect("hooks config");
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"
"#,
        )
        .expect("invalid workflows config");

        let Json(config) = get_config()
            .await
            .expect("config errors should still be returned with graph audit");

        assert_eq!(config["graph_authority"], "langgraph");
        assert_eq!(config["hooksTomlExists"], true);
        assert_eq!(config["workflowsTomlExists"], true);
        assert!(config
            .get("workflowCount")
            .is_none_or(serde_json::Value::is_null));
        assert!(config["error"]
            .as_str()
            .is_some_and(|error| error.contains("authoritative workflows.toml load failed")));
        assert_eq!(config["graph_audit"]["graph"], "operational_api_read");
    }

    #[test]
    fn config_error_json_does_not_claim_raw_graph_authority() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());

        let config = config_error_json("authoritative workflows.toml load failed".to_string());

        assert!(config.get("graph_authority").is_none());
        assert_eq!(config["error"], "authoritative workflows.toml load failed");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_stats_projects_langgraph_workflow_counts() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();
        clear_session_cache();
        write_workflow_config();

        let session_id = "stats-langgraph-session";
        let mut state = SessionState::new(session_id);
        state.active_skill = Some("linear".to_string());
        sentinel_infrastructure::state_store::save(&mut state).expect("save session state");

        let db_path =
            crate::phase_graph_projection::phase_graph_db_path(session_id).expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("phase saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(&workflow(), saver)
            .expect("compile graph");
        graph
            .run_until_gate("linear", session_id)
            .await
            .expect("gate checkpoint");

        let Json(stats) = get_stats().await.expect("session stats graph audit");

        assert_eq!(stats["workflow_authority"], "langgraph");
        assert_eq!(stats["total_sessions"], 1);
        assert_eq!(stats["total_langgraph_workflows"], 1);
        assert_eq!(stats["sessions_with_langgraph_workflows"], 1);
        assert_eq!(stats["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(stats["graph_audit"]["graph"], "session_api_stats");
        assert_eq!(stats["graph_audit"]["decision"], "verified");
        assert!(stats["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(
            stats["graph_audit"]["run"]["topology"]["graph"],
            "session_api_stats"
        );
        assert_eq!(
            stats["graph_audit"]["run"]["topology"]["durable_checkpointer"],
            true
        );
        assert!(
            stats["graph_audit"]["run"]["checkpoints"]
                .as_array()
                .is_some_and(|entries| !entries.is_empty()),
            "session API stats graph audit must expose checkpoints: {stats}"
        );
        assert!(
            stats["graph_audit"]["run"]["write_history"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["channel"] == "state")),
            "session API stats graph audit must expose state write history: {stats}"
        );
        let graph_rows = std::fs::read_to_string(
            tmp.path()
                .join(".claude")
                .join("sentinel")
                .join("metrics")
                .join("session-api-stats.graph-runs.jsonl"),
        )
        .expect("session API stats graph audit rows");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"session_api_stats\""));
        assert!(graph_rows.contains("\"authorization_checkpoint\""));
        assert_eq!(stats.get("error"), None);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_stats_fails_closed_when_stats_graph_audit_cannot_run() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::unsupported_decision_backend();
        clear_session_cache();
        write_workflow_config();

        let err = get_stats()
            .await
            .expect_err("session stats must refuse unaudited responses");

        assert_eq!(err, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_stats_fails_closed_on_inconsistent_hook_timing_counts() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();
        clear_session_cache();
        write_workflow_config();

        let mut state = SessionState::new("stats-bad-hook-counts");
        state
            .hook_stats
            .per_hook_time_ms
            .insert("PreToolUse".to_string(), 42);
        sentinel_infrastructure::state_store::save(&mut state).expect("save session state");

        let err = get_stats()
            .await
            .expect_err("inconsistent hook stats must refuse aggregate");

        assert_eq!(err, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn list_sessions_serializes_langgraph_workflow_count_only() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();
        clear_session_cache();
        write_workflow_config();

        let session_id = "list-langgraph-response";
        let mut state = SessionState::new(session_id);
        state.active_skill = Some("linear".to_string());
        sentinel_infrastructure::state_store::save(&mut state).expect("save session state");

        let db_path =
            crate::phase_graph_projection::phase_graph_db_path(session_id).expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("phase saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(&workflow(), saver)
            .expect("compile graph");
        graph
            .run_until_gate("linear", session_id)
            .await
            .expect("gate checkpoint");

        let Json(sessions) = list_sessions(axum::extract::Query(HashMap::new()))
            .await
            .expect("list sessions response");
        let response = serde_json::to_value(&sessions[0]).expect("summary json");

        assert_eq!(response["workflow_authority"], "langgraph");
        assert_eq!(response["langgraph_workflow_count"], 1);
        assert_eq!(response["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(response["graph_audit"]["graph"], "session_api_read");
        assert_eq!(response["graph_audit"]["surface"], "summary");
        assert_eq!(response["graph_audit"]["decision"], "verified");
        assert!(response["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(
            response["graph_audit"]["run"]["topology"]["graph"],
            "session_api_read"
        );
        assert!(
            response["graph_audit"]["run"]["checkpoints"]
                .as_array()
                .is_some_and(|entries| !entries.is_empty()),
            "session API summary graph audit must expose checkpoints: {response}"
        );
        assert!(
            response.get("workflow_count").is_none(),
            "session summaries must not expose generic workflow counters"
        );
        let graph_rows = std::fs::read_to_string(
            tmp.path()
                .join(".claude")
                .join("sentinel")
                .join("metrics")
                .join("session-api-read.graph-runs.jsonl"),
        )
        .expect("session API read graph audit rows");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"session_api_read\""));
        assert!(graph_rows.contains("\"surface\":\"summary\""));
        assert!(graph_rows.contains("\"authorization_checkpoint\""));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_session_returns_only_langgraph_workflows() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();
        clear_session_cache();
        write_workflow_config();

        let session_id = "session-langgraph-response";
        let mut state = SessionState::new(session_id);
        state.active_skill = Some("linear".to_string());
        sentinel_infrastructure::state_store::save(&mut state).expect("save session state");

        let db_path =
            crate::phase_graph_projection::phase_graph_db_path(session_id).expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("phase saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(&workflow(), saver)
            .expect("compile graph");
        graph
            .run_until_gate("linear", session_id)
            .await
            .expect("gate checkpoint");

        let Json(session) = get_session(axum::extract::Path(session_id.to_string()))
            .await
            .expect("session response");

        assert_eq!(session["workflow_authority"], "langgraph");
        assert_eq!(session["langgraph_workflow_count"], 1);
        assert_eq!(session["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(session["graph_audit"]["graph"], "session_api_read");
        assert_eq!(session["graph_audit"]["surface"], "detail");
        assert_eq!(session["graph_audit"]["decision"], "verified");
        assert!(session["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(
            session["graph_audit"]["run"]["topology"]["graph"],
            "session_api_read"
        );
        assert!(
            session["graph_audit"]["run"]["write_history"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["channel"] == "state")),
            "session API detail graph audit must expose state write history: {session}"
        );
        assert!(session.get("workflows").is_none());
        assert_eq!(
            session["langgraph_workflows"]["linear"]["skill"],
            serde_json::json!("linear")
        );
        assert_eq!(
            session["langgraph_workflows"]["linear"]["current_phase"],
            serde_json::json!(0)
        );
        let graph_rows = std::fs::read_to_string(
            tmp.path()
                .join(".claude")
                .join("sentinel")
                .join("metrics")
                .join("session-api-read.graph-runs.jsonl"),
        )
        .expect("session API read graph audit rows");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"session_api_read\""));
        assert!(graph_rows.contains("\"surface\":\"detail\""));
        assert!(graph_rows.contains("\"authorization_checkpoint\""));
    }
}
