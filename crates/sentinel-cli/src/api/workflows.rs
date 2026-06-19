//! Workflow Status API Endpoints
//!
//! GET /api/workflows              — list all workflow definitions
//! GET /api/workflows/:skill/status — current workflow state for a skill

use std::collections::BTreeSet;
use std::io::Write as _;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};

use sentinel_domain::workflow::WorkflowState;
use sentinel_graph::PhaseGraphIntrospection;
use sentinel_infrastructure::workflow_api_read_graph::{
    WorkflowApiReadGraph, WorkflowApiReadSurface,
};

use super::AppState;
use crate::phase_graph_projection::{
    graph_checkpoint_projection, graph_history_projection, graph_introspection,
    graph_latest_workflow_state, graph_writes_projection, load_workflow_configs,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_workflows))
        .route("/{skill}/status", get(get_status))
}

async fn list_workflows(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session_id = {
        let session = state.session.read().await;
        session.session_id.clone()
    };
    let workflow_configs = match load_workflow_configs() {
        Ok(configs) => configs,
        Err(e) => {
            return workflow_json(
                WorkflowApiReadSurface::Error,
                workflow_error_json(format!("workflow config load failed: {e}")),
            )
            .await;
        }
    };
    let skills: BTreeSet<String> = workflow_configs.keys().cloned().collect();

    let mut workflows = Vec::new();
    for skill in skills {
        let topology = match graph_introspection(&skill, &session_id, &workflow_configs).await {
            Ok(topology) => topology,
            Err(e) => {
                workflows.push(workflow_skill_error_json(
                    &skill,
                    format!("phase graph introspection failed: {e}"),
                ));
                continue;
            }
        };
        let checkpoints =
            match graph_checkpoint_projection(&skill, &session_id, &workflow_configs).await {
                Ok(checkpoints) => checkpoints,
                Err(e) => {
                    workflows.push(workflow_skill_error_json(
                        &skill,
                        format!("phase graph checkpoint history load failed: {e}"),
                    ));
                    continue;
                }
            };
        let history = match graph_history_projection(&skill, &session_id, &workflow_configs).await {
            Ok(history) => history,
            Err(e) => {
                workflows.push(workflow_skill_error_json(
                    &skill,
                    format!("phase graph history load failed: {e}"),
                ));
                continue;
            }
        };
        let writes =
            match graph_writes_projection(&skill, &session_id, &workflow_configs, None).await {
                Ok(writes) => writes,
                Err(e) => {
                    workflows.push(workflow_skill_error_json(
                        &skill,
                        format!("phase graph write history load failed: {e}"),
                    ));
                    continue;
                }
            };
        let wf = match graph_latest_workflow_state(&skill, &session_id, &workflow_configs).await {
            Ok(Some(wf)) => wf,
            Ok(None) => {
                workflows.push(workflow_no_checkpoint_json(
                    &skill,
                    topology.as_ref(),
                    checkpoints.as_ref(),
                    history.as_ref(),
                    writes.as_ref(),
                ));
                continue;
            }
            Err(e) => {
                workflows.push(workflow_skill_error_json(
                    &skill,
                    format!("phase graph checkpoint load failed: {e}"),
                ));
                continue;
            }
        };
        workflows.push(workflow_summary_json(
            &skill,
            &wf,
            topology.as_ref(),
            checkpoints.as_ref(),
            history.as_ref(),
            writes.as_ref(),
        ));
    }

    workflow_list_json(serde_json::json!({
        "workflow_authority": "langgraph",
        "workflows": workflows,
    }))
    .await
}

async fn get_status(
    State(state): State<AppState>,
    Path(skill): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session_id = {
        let session = state.session.read().await;
        session.session_id.clone()
    };
    let workflow_configs = match load_workflow_configs() {
        Ok(configs) => configs,
        Err(e) => {
            return workflow_json(
                WorkflowApiReadSurface::Error,
                workflow_error_json(format!("workflow config load failed: {e}")),
            )
            .await;
        }
    };
    if !workflow_configs.contains_key(&skill) {
        return workflow_json(
            WorkflowApiReadSurface::Error,
            workflow_skill_error_json(
                &skill,
                format!("No configured LangGraph workflow for skill '{}'", skill),
            ),
        )
        .await;
    }

    match graph_latest_workflow_state(&skill, &session_id, &workflow_configs).await {
        Ok(Some(wf)) => {
            let topology = match graph_introspection(&skill, &session_id, &workflow_configs).await {
                Ok(topology) => topology,
                Err(e) => {
                    return workflow_json(
                        WorkflowApiReadSurface::Error,
                        workflow_skill_error_json(
                            &skill,
                            format!(
                                "phase graph introspection failed for skill '{}': {e}",
                                skill
                            ),
                        ),
                    )
                    .await;
                }
            };
            let checkpoints =
                match graph_checkpoint_projection(&skill, &session_id, &workflow_configs).await {
                    Ok(checkpoints) => checkpoints,
                    Err(e) => {
                        return workflow_json(
                            WorkflowApiReadSurface::Error,
                            workflow_skill_error_json(
                                &skill,
                                format!(
                                "phase graph checkpoint history load failed for skill '{}': {e}",
                                skill
                            ),
                            ),
                        )
                        .await;
                    }
                };
            let history = match graph_history_projection(&skill, &session_id, &workflow_configs)
                .await
            {
                Ok(history) => history,
                Err(e) => {
                    return workflow_json(
                        WorkflowApiReadSurface::Error,
                        workflow_skill_error_json(
                            &skill,
                            format!("phase graph history load failed for skill '{}': {e}", skill),
                        ),
                    )
                    .await;
                }
            };
            let writes =
                match graph_writes_projection(&skill, &session_id, &workflow_configs, None).await {
                    Ok(writes) => writes,
                    Err(e) => {
                        return workflow_json(
                            WorkflowApiReadSurface::Error,
                            workflow_skill_error_json(
                                &skill,
                                format!(
                                    "phase graph write history load failed for skill '{}': {e}",
                                    skill
                                ),
                            ),
                        )
                        .await;
                    }
                };
            workflow_json(
                WorkflowApiReadSurface::Status,
                workflow_state_json(
                    &wf,
                    topology.as_ref(),
                    checkpoints.as_ref(),
                    history.as_ref(),
                    writes.as_ref(),
                ),
            )
            .await
        }
        Ok(None) => {
            let topology = match graph_introspection(&skill, &session_id, &workflow_configs).await {
                Ok(topology) => topology,
                Err(e) => {
                    return workflow_json(
                        WorkflowApiReadSurface::Error,
                        workflow_skill_error_json(
                            &skill,
                            format!(
                                "phase graph introspection failed for skill '{}': {e}",
                                skill
                            ),
                        ),
                    )
                    .await;
                }
            };
            let checkpoints =
                match graph_checkpoint_projection(&skill, &session_id, &workflow_configs).await {
                    Ok(checkpoints) => checkpoints,
                    Err(e) => {
                        return workflow_json(
                            WorkflowApiReadSurface::Error,
                            workflow_skill_error_json(
                                &skill,
                                format!(
                                "phase graph checkpoint history load failed for skill '{}': {e}",
                                skill
                            ),
                            ),
                        )
                        .await;
                    }
                };
            let history = match graph_history_projection(&skill, &session_id, &workflow_configs)
                .await
            {
                Ok(history) => history,
                Err(e) => {
                    return workflow_json(
                        WorkflowApiReadSurface::Error,
                        workflow_skill_error_json(
                            &skill,
                            format!("phase graph history load failed for skill '{}': {e}", skill),
                        ),
                    )
                    .await;
                }
            };
            let writes =
                match graph_writes_projection(&skill, &session_id, &workflow_configs, None).await {
                    Ok(writes) => writes,
                    Err(e) => {
                        return workflow_json(
                            WorkflowApiReadSurface::Error,
                            workflow_skill_error_json(
                                &skill,
                                format!(
                                    "phase graph write history load failed for skill '{}': {e}",
                                    skill
                                ),
                            ),
                        )
                        .await;
                    }
                };
            workflow_json(
                WorkflowApiReadSurface::Status,
                workflow_no_checkpoint_json(
                    &skill,
                    topology.as_ref(),
                    checkpoints.as_ref(),
                    history.as_ref(),
                    writes.as_ref(),
                ),
            )
            .await
        }
        Err(e) => {
            workflow_json(
                WorkflowApiReadSurface::Error,
                workflow_skill_error_json(
                    &skill,
                    format!(
                        "phase graph checkpoint load failed for skill '{}': {e}",
                        skill
                    ),
                ),
            )
            .await
        }
    }
}

async fn workflow_list_json(
    response: serde_json::Value,
) -> Result<Json<serde_json::Value>, StatusCode> {
    attach_workflow_list_graph_audit(response)
        .await
        .map(Json)
        .map_err(|error| {
            tracing::error!(
                error = %error,
                "workflow API read graph audit failed; refusing unaudited response"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn workflow_json(
    surface: WorkflowApiReadSurface,
    response: serde_json::Value,
) -> Result<Json<serde_json::Value>, StatusCode> {
    attach_workflow_api_read_graph_audit(surface, response)
        .await
        .map(Json)
        .map_err(|error| {
            tracing::error!(
                surface = sentinel_infrastructure::workflow_api_read_graph::workflow_api_read_surface_label(surface),
                error = %error,
                "workflow API read graph audit failed; refusing unaudited response"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn attach_workflow_list_graph_audit(
    mut response: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let graph = sentinel_infrastructure::workflow_api_read_graph::build_workflow_api_read_graph()
        .await
        .map_err(|e| format!("build workflow API read graph: {e}"))?;
    let workflows = response
        .get_mut("workflows")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| "workflow API list response must contain workflows array".to_string())?;
    for workflow in workflows {
        let raw = std::mem::take(workflow);
        *workflow = attach_workflow_api_read_graph_audit_with_graph(
            &graph,
            WorkflowApiReadSurface::Item,
            raw,
        )
        .await?;
    }
    attach_workflow_api_read_graph_audit_with_graph(&graph, WorkflowApiReadSurface::List, response)
        .await
}

async fn attach_workflow_api_read_graph_audit(
    surface: WorkflowApiReadSurface,
    response: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let graph = sentinel_infrastructure::workflow_api_read_graph::build_workflow_api_read_graph()
        .await
        .map_err(|e| format!("build workflow API read graph: {e}"))?;
    attach_workflow_api_read_graph_audit_with_graph(&graph, surface, response).await
}

async fn attach_workflow_api_read_graph_audit_with_graph(
    graph: &WorkflowApiReadGraph,
    surface: WorkflowApiReadSurface,
    mut response: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let graph_audit = run_workflow_api_read_graph_audit(graph, surface, &response).await?;
    response
        .as_object_mut()
        .ok_or_else(|| {
            "workflow API read graph audit can only attach to object responses".to_string()
        })?
        .insert("graph_audit".to_string(), graph_audit);
    Ok(response)
}

async fn run_workflow_api_read_graph_audit(
    graph: &WorkflowApiReadGraph,
    surface: WorkflowApiReadSurface,
    response: &serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let response_hash = sentinel_infrastructure::workflow_api_read_graph::sha256_json(response);
    let surface_label =
        sentinel_infrastructure::workflow_api_read_graph::workflow_api_read_surface_label(surface);
    let identifier = workflow_api_read_identifier(surface_label, response, &response_hash);
    let state =
        sentinel_infrastructure::workflow_api_read_graph::WorkflowApiReadState::from_response(
            surface, identifier, response,
        );
    let run =
        sentinel_infrastructure::workflow_api_read_graph::run_workflow_api_read_decision_report(
            graph, state,
        )
        .await
        .map_err(|e| format!("run workflow API read graph: {e}"))?;
    let authorization = run
        .workflow_api_read_authorization()
        .map_err(|e| format!("workflow API read graph authorization failed: {e}"))?
        .ok_or_else(|| "workflow API read graph produced no terminal checkpoint".to_string())?;
    let graph_runs = sentinel_infrastructure::paths::sentinel_root()
        .join("metrics")
        .join("workflow-api-read.graph-runs.jsonl");
    if let Some(parent) = graph_runs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "create workflow API read graph audit dir {}: {e}",
                parent.display()
            )
        })?;
    }
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "workflow_api_read",
        "surface": surface_label,
        "response_sha256": response_hash,
        "decision": sentinel_infrastructure::workflow_api_read_graph::
            workflow_api_read_decision_label(authorization.decision()),
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
                "open workflow API read graph audit {}: {e}",
                graph_runs.display()
            )
        })?;
    serde_json::to_writer(&mut file, &row).map_err(|e| {
        format!(
            "write workflow API read graph audit {}: {e}",
            graph_runs.display()
        )
    })?;
    file.write_all(b"\n").map_err(|e| {
        format!(
            "terminate workflow API read graph audit {}: {e}",
            graph_runs.display()
        )
    })?;
    Ok(serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "workflow_api_read",
        "surface": surface_label,
        "graph_runs_path": graph_runs,
        "response_sha256": row["response_sha256"].clone(),
        "decision": row["decision"].clone(),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": row["thread_id"].clone(),
        "run": row["run"].clone(),
    }))
}

fn workflow_api_read_identifier(
    surface_label: &str,
    response: &serde_json::Value,
    response_hash: &str,
) -> String {
    let id = if let Some(skill) = response.get("skill").and_then(serde_json::Value::as_str) {
        format!("skill-present-true:{skill}")
    } else if let Some(session_id) = response
        .get("session_id")
        .and_then(serde_json::Value::as_str)
    {
        format!("skill-present-false:session-id-present-true:{session_id}")
    } else {
        "skill-present-false:session-id-present-false".to_string()
    };
    format!("{surface_label}-{id}:response-{response_hash}")
}

fn workflow_error_json(error: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "workflow_authority": "langgraph",
        "error": error.into(),
    })
}

fn workflow_skill_error_json(skill: &str, error: impl Into<String>) -> serde_json::Value {
    let mut value = workflow_error_json(error);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("skill".to_string(), serde_json::json!(skill));
    }
    value
}

fn workflow_state_json(
    wf: &WorkflowState,
    topology: Option<&PhaseGraphIntrospection>,
    checkpoints: Option<&serde_json::Value>,
    history: Option<&serde_json::Value>,
    writes: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut value =
        serde_json::to_value(wf).expect("WorkflowState serialization must produce JSON");
    attach_graph_evidence(&mut value, topology, checkpoints, history, writes);
    value
}

fn workflow_no_checkpoint_json(
    skill: &str,
    topology: Option<&PhaseGraphIntrospection>,
    checkpoints: Option<&serde_json::Value>,
    history: Option<&serde_json::Value>,
    writes: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "skill": skill,
        "workflow_authority": "langgraph",
        "checkpoint": null,
        "graph_state": null,
        "status": "no_checkpoint",
    });
    attach_graph_evidence(&mut value, topology, checkpoints, history, writes);
    value
}

fn workflow_summary_json(
    skill: &str,
    wf: &WorkflowState,
    topology: Option<&PhaseGraphIntrospection>,
    checkpoints: Option<&serde_json::Value>,
    history: Option<&serde_json::Value>,
    writes: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "skill": skill,
        "workflow_authority": "langgraph",
        "current_phase": wf.current_phase,
        "completed_phases": wf.completed_phases,
        "complete": wf.complete,
        "current_step": wf.current_step,
        "step_states": wf.step_states,
    });
    attach_graph_evidence(&mut value, topology, checkpoints, history, writes);
    value
}

fn attach_graph_evidence(
    value: &mut serde_json::Value,
    topology: Option<&PhaseGraphIntrospection>,
    checkpoints: Option<&serde_json::Value>,
    history: Option<&serde_json::Value>,
    writes: Option<&serde_json::Value>,
) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    obj.insert(
        "workflow_authority".to_string(),
        serde_json::json!("langgraph"),
    );
    if let Some(topology) = topology {
        obj.insert("graph_topology".to_string(), serde_json::json!(topology));
    }
    if let Some(checkpoints) = checkpoints {
        obj.insert("graph_checkpoints".to_string(), checkpoints.clone());
        if let Some(latest_checkpoint) = latest_checkpoint_json(checkpoints) {
            obj.insert("latest_checkpoint".to_string(), latest_checkpoint.clone());
            let graph_state = latest_checkpoint
                .get("state")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            obj.insert("graph_state".to_string(), graph_state.clone());
            project_top_level_workflow_from_graph_state(obj, &graph_state);
        }
    }
    if let Some(history) = history {
        obj.insert("graph_history".to_string(), history.clone());
    }
    if let Some(writes) = writes {
        obj.insert("graph_writes".to_string(), writes.clone());
    }
}

fn project_top_level_workflow_from_graph_state(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    graph_state: &serde_json::Value,
) {
    let Some(graph_state) = graph_state.as_object() else {
        return;
    };
    for key in [
        "skill",
        "session_id",
        "current_phase",
        "completed_phases",
        "complete",
        "current_step",
        "step_states",
    ] {
        if let Some(value) = graph_state.get(key) {
            obj.insert(key.to_string(), value.clone());
        }
    }
}

fn latest_checkpoint_json(checkpoints: &serde_json::Value) -> Option<&serde_json::Value> {
    checkpoints.as_array()?.last()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use sentinel_domain::state::SessionState;
    use tokio::sync::RwLock;

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

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn list_workflows_response_declares_langgraph_authority() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();
        write_workflow_config();
        let state = AppState {
            session: Arc::new(RwLock::new(SessionState::new("workflow-list-session"))),
        };

        let Json(json) = list_workflows(axum::extract::State(state))
            .await
            .expect("workflow API read graph audit");

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["graph"], "workflow_api_read");
        assert_eq!(json["graph_audit"]["surface"], "list");
        assert_eq!(json["graph_audit"]["decision"], "verified");
        assert!(json["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(json["workflows"].as_array().expect("workflows").len(), 1);
        assert_eq!(json["workflows"][0]["workflow_authority"], "langgraph");
        assert_eq!(
            json["workflows"][0]["graph_audit"]["graph"],
            "workflow_api_read"
        );
        assert_eq!(json["workflows"][0]["graph_audit"]["surface"], "item");
        assert!(
            json["workflows"][0]["graph_audit"]["authorization_checkpoint"]
                .as_str()
                .is_some_and(|checkpoint| checkpoint.contains('#'))
        );
        assert_eq!(json["workflows"][0]["status"], "no_checkpoint");
        let graph_rows = std::fs::read_to_string(
            tmp.path()
                .join(".claude")
                .join("sentinel")
                .join("metrics")
                .join("workflow-api-read.graph-runs.jsonl"),
        )
        .expect("workflow API read graph audit rows");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"workflow_api_read\""));
        assert!(graph_rows.contains("\"surface\":\"item\""));
        assert!(graph_rows.contains("\"surface\":\"list\""));
        assert!(graph_rows.contains("\"authorization_checkpoint\""));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn list_workflows_fails_closed_when_read_graph_audit_cannot_run() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::unsupported_decision_backend();
        write_workflow_config();
        let state = AppState {
            session: Arc::new(RwLock::new(SessionState::new("workflow-list-fail-closed"))),
        };

        let err = list_workflows(axum::extract::State(state))
            .await
            .expect_err("workflow API must refuse unaudited responses");

        assert_eq!(err, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_status_response_declares_workflow_api_read_graph_audit() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();
        write_workflow_config();
        let state = AppState {
            session: Arc::new(RwLock::new(SessionState::new("workflow-status-session"))),
        };

        let Json(json) = get_status(
            axum::extract::State(state),
            axum::extract::Path("linear".to_string()),
        )
        .await
        .expect("workflow API read graph audit");

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["graph"], "workflow_api_read");
        assert_eq!(json["graph_audit"]["surface"], "status");
        assert_eq!(json["graph_audit"]["decision"], "verified");
        assert!(json["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(json["status"], "no_checkpoint");
        assert_eq!(json["graph_topology"]["skill"], "linear");
    }

    #[test]
    fn workflow_summary_includes_graph_projected_step_state() {
        let mut wf = WorkflowState::new("linear", "sess");
        wf.current_phase = Some(1);
        wf.completed_phases = vec!["claim".into()];
        wf.update_step(
            "claim",
            "0.1",
            sentinel_domain::workflow::StepStatus::Completed,
            Some("done".into()),
        );
        let json = workflow_summary_json("linear", &wf, None, None, None, None);

        assert_eq!(json["skill"], "linear");
        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["current_phase"], 1);
        assert_eq!(json["completed_phases"], serde_json::json!(["claim"]));
        assert_eq!(json["step_states"].as_array().unwrap().len(), 1);
        assert_eq!(json["step_states"][0]["step_id"], "0.1");
    }

    #[test]
    fn workflow_error_responses_preserve_langgraph_authority() {
        let json = workflow_error_json("phase graph checkpoint load failed");

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["error"], "phase graph checkpoint load failed");
    }

    #[test]
    fn workflow_skill_error_responses_preserve_langgraph_authority_and_skill() {
        let json = workflow_skill_error_json("linear", "phase graph introspection failed");

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["skill"], "linear");
        assert_eq!(json["error"], "phase graph introspection failed");
    }

    #[test]
    fn workflow_summary_includes_compiled_graph_topology() {
        let wf = WorkflowState::new("linear", "sess");
        let topology = PhaseGraphIntrospection {
            skill: "linear".into(),
            thread_id: "sentinel.phase.linear.sess".into(),
            phase_order: vec!["claim".into()],
            durable_checkpointer: true,
            checkpointer_backend: "sqlite".into(),
            checkpointer_scope: "database_path:/tmp/sentinel-phase.db".into(),
            auto_checkpoint: true,
            max_iterations: 100,
            schemas: sentinel_graph::PhaseGraphSchemas {
                state: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "skill": { "const": "linear" },
                        "phase_order": { "const": ["claim"] }
                    }
                })),
                input: None,
                output: None,
                context: None,
            },
            nodes: vec![sentinel_graph::PhaseGraphNodeInfo {
                id: "claim".into(),
                deferred: false,
                barrier_on: Vec::new(),
                metadata: std::collections::BTreeMap::from([
                    ("sentinel.phase".into(), "claim".into()),
                    ("sentinel.checkpointer_backend".into(), "sqlite".into()),
                    (
                        "sentinel.checkpointer_scope".into(),
                        "database_path:/tmp/sentinel-phase.db".into(),
                    ),
                ]),
                has_error_handler: false,
                has_timeout_policy: true,
                interrupt_before: false,
                interrupt_after: true,
            }],
            edges: vec![sentinel_graph::PhaseGraphEdgeInfo {
                from: "claim".into(),
                kind: "conditional".into(),
                to: None,
            }],
        };

        let json = workflow_summary_json("linear", &wf, Some(&topology), None, None, None);

        assert_eq!(
            json["graph_topology"]["thread_id"],
            "sentinel.phase.linear.sess"
        );
        assert_eq!(json["graph_topology"]["durable_checkpointer"], true);
        assert_eq!(json["graph_topology"]["checkpointer_backend"], "sqlite");
        assert_eq!(
            json["graph_topology"]["checkpointer_scope"],
            "database_path:/tmp/sentinel-phase.db"
        );
        assert_eq!(json["graph_topology"]["auto_checkpoint"], true);
        assert_eq!(json["graph_topology"]["max_iterations"], 100);
        assert_eq!(json["graph_topology"]["nodes"][0]["id"], "claim");
        assert_eq!(
            json["graph_topology"]["nodes"][0]["metadata"]["sentinel.phase"],
            "claim"
        );
        assert_eq!(
            json["graph_topology"]["nodes"][0]["has_timeout_policy"],
            true
        );
        assert_eq!(json["graph_topology"]["nodes"][0]["interrupt_after"], true);
        assert_eq!(
            json["graph_topology"]["schemas"]["state"]["properties"]["skill"]["const"],
            "linear"
        );
    }

    #[test]
    fn no_checkpoint_status_does_not_synthesize_progress() {
        let json = workflow_no_checkpoint_json("linear", None, None, None, None);

        assert_eq!(json["skill"], "linear");
        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["status"], "no_checkpoint");
        assert!(json.get("current_phase").is_none());
        assert!(json.get("completed_phases").is_none());
        assert!(json["graph_state"].is_null());
        assert!(json["checkpoint"].is_null());
        assert!(json.get("graph_history").is_none());
    }

    #[test]
    fn workflow_summary_includes_graph_checkpoint_history() {
        let wf = WorkflowState::new("linear", "sess");
        let checkpoints = serde_json::json!([
            {
                "checkpoint_id": "11111111-1111-4111-8111-111111111111",
                "parent_checkpoint_id": null,
                "thread_id": "sentinel.phase.linear.sess",
                "step_number": 0,
                "created_at": "2026-06-17T00:00:00Z",
                "source": {
                    "step": 0,
                    "source_type": "input",
                    "node": "claim"
                },
                "writes": [
                    {
                        "node_id": "claim",
                        "channel": "state",
                        "ts": "2026-06-17T00:00:00Z"
                    }
                ],
                "tags": {},
                "skill": "linear",
                "session_id": "sess",
                "current_phase": 0,
                "completed_phases": [],
                "current_step": null,
                "step_states": [],
                "complete": false,
                "state": {
                    "skill": "linear",
                    "session_id": "sess",
                    "phase_order": ["claim"],
                    "current_phase": 0,
                    "completed_phases": [],
                    "complete": false,
                    "dyad_verdicts": {},
                    "step_states": [],
                    "current_step": null,
                    "last_verdict": "pending"
                }
            }
        ]);
        let history = serde_json::json!([
            {
                "skill": "linear",
                "session_id": "sess",
                "phase_order": ["claim"],
                "current_phase": 0,
                "completed_phases": [],
                "complete": false,
                "dyad_verdicts": {},
                "step_states": [],
                "current_step": null,
                "last_verdict": "pending"
            }
        ]);

        let json = workflow_summary_json(
            "linear",
            &wf,
            None,
            Some(&checkpoints),
            Some(&history),
            None,
        );

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["graph_checkpoints"].as_array().unwrap().len(), 1);
        assert_eq!(json["graph_checkpoints"][0]["skill"], "linear");
        assert_eq!(
            json["graph_checkpoints"][0]["checkpoint_id"],
            "11111111-1111-4111-8111-111111111111"
        );
        assert_eq!(
            json["latest_checkpoint"]["checkpoint_id"],
            "11111111-1111-4111-8111-111111111111"
        );
        assert_eq!(json["latest_checkpoint"]["source"]["node"], "claim");
        assert_eq!(json["graph_state"]["skill"], "linear");
        assert_eq!(json["graph_state"]["phase_order"][0], "claim");
        assert_eq!(json["graph_checkpoints"][0]["source"]["node"], "claim");
        assert_eq!(
            json["graph_checkpoints"][0]["writes"][0]["channel"],
            "state"
        );
        assert_eq!(
            json["graph_checkpoints"][0]["state"]["phase_order"][0],
            "claim"
        );
        assert_eq!(json["graph_history"].as_array().unwrap().len(), 1);
        assert_eq!(json["graph_history"][0]["phase_order"][0], "claim");
    }

    #[test]
    fn workflow_summary_projects_top_level_fields_from_checkpoint_graph_state() {
        let mut stale = WorkflowState::new("linear", "sess");
        stale.current_phase = Some(0);
        stale.completed_phases = Vec::new();
        stale.complete = false;
        let checkpoints = serde_json::json!([
            {
                "checkpoint_id": "checkpoint-graph-current",
                "thread_id": "sentinel.phase.linear.sess",
                "step_number": 2,
                "source": { "node": "claim", "source_type": "stream_update" },
                "writes": [{ "node_id": "claim", "channel": "state", "ts": "2026-06-17T00:00:00Z" }],
                "state": {
                    "skill": "linear",
                    "session_id": "sess",
                    "phase_order": ["claim"],
                    "current_phase": 1,
                    "completed_phases": ["claim"],
                    "complete": true,
                    "current_step": null,
                    "step_states": []
                }
            }
        ]);

        let json = workflow_summary_json("linear", &stale, None, Some(&checkpoints), None, None);

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(
            json["graph_state"]["completed_phases"],
            serde_json::json!(["claim"])
        );
        assert_eq!(json["current_phase"], 1);
        assert_eq!(json["completed_phases"], serde_json::json!(["claim"]));
        assert_eq!(json["complete"], true);
    }

    #[test]
    fn workflow_summary_includes_graph_write_history() {
        let wf = WorkflowState::new("linear", "sess");
        let writes = serde_json::json!([
            {
                "checkpoint_id": "11111111-1111-4111-8111-111111111111",
                "step_number": 1,
                "channel": "state",
                "node_id": "claim",
                "ts": "2026-06-17T00:00:00Z",
                "value_len": 128,
                "value_sha256": "abc123",
                "value_json": {
                    "skill": "linear",
                    "completed_phases": ["claim"]
                }
            }
        ]);

        let json = workflow_summary_json("linear", &wf, None, None, None, Some(&writes));

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["graph_writes"].as_array().unwrap().len(), 1);
        assert_eq!(json["graph_writes"][0]["channel"], "state");
        assert_eq!(json["graph_writes"][0]["value_json"]["skill"], "linear");
    }

    #[test]
    fn workflow_state_includes_graph_authority_latest_checkpoint_and_state() {
        let mut wf = WorkflowState::new("linear", "sess");
        wf.current_phase = Some(0);
        let checkpoints = serde_json::json!([
            {
                "checkpoint_id": "checkpoint-1",
                "thread_id": "sentinel.phase.linear.sess",
                "step_number": 1,
                "source": { "node": "claim", "source_type": "stream_update" },
                "writes": [{ "node_id": "claim", "channel": "state", "ts": "2026-06-17T00:00:00Z" }],
                "state": {
                    "skill": "linear",
                    "session_id": "sess",
                    "phase_order": ["claim"],
                    "current_phase": 1,
                    "completed_phases": ["claim"]
                }
            }
        ]);

        let json = workflow_state_json(&wf, None, Some(&checkpoints), None, None);

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["latest_checkpoint"]["checkpoint_id"], "checkpoint-1");
        assert_eq!(json["latest_checkpoint"]["source"]["node"], "claim");
        assert_eq!(json["graph_state"]["completed_phases"][0], "claim");
        assert_eq!(json["current_phase"], 1);
        assert_eq!(json["completed_phases"], serde_json::json!(["claim"]));
    }

    #[test]
    fn workflow_api_identifier_records_absent_keys_explicitly() {
        let identifier = workflow_api_read_identifier(
            "error",
            &serde_json::json!({"error": "workflow config load failed"}),
            "abc123",
        );

        assert_eq!(
            identifier,
            "error-skill-present-false:session-id-present-false:response-abc123"
        );
        assert!(!identifier.contains("workflow-api"));
    }
}
