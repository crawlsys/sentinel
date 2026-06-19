//! Proof Chain API Endpoints
//!
//! GET /api/proofs                    — list all proof chain sessions
//! GET /`api/proofs/:session_id`        — full proof chain for a session
//! GET /`api/proofs/:session_id/verify` — re-verify chain integrity

use std::io::Write as _;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use sentinel_domain::proof::ProofChain;
use sentinel_domain::workflow::WorkflowState;
use sentinel_infrastructure::proof_api_read_graph::{ProofApiReadGraph, ProofApiReadSurface};

use super::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_proofs))
        .route("/{session_id}", get(get_proof_chain))
        .route("/{session_id}/verify", get(verify_chain))
}

async fn list_proofs(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    // Collect while holding the read-guard, then drop it before returning.
    let chains = match {
        let session = state.session.read().await;
        let mut chains = Vec::new();
        let mut error = None;
        for (skill, chain) in session.proof_chains() {
            let Some(workflow_state) = session.graph_workflow(skill) else {
                error = Some(proof_skill_error_json(
                    skill,
                    "proof chain is unavailable without LangGraph-projected workflow state",
                ));
                break;
            };
            chains.push(proof_summary_json(skill, chain, workflow_state));
        }
        match error {
            Some(error) => Err(error),
            None => Ok(chains),
        }
    } {
        Ok(chains) => chains,
        Err(error) => return proof_json(ProofApiReadSurface::Error, error).await,
    };
    proof_list_json(serde_json::json!({
        "workflow_authority": "langgraph",
        "chains": chains,
    }))
    .await
}

async fn get_proof_chain(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let in_memory = {
        let session = state.session.read().await;
        let mut found = None;
        for (skill, chain) in session.proof_chains() {
            if chain.session_id == session_id {
                let Some(workflow_state) = session.graph_workflow(skill) else {
                    found = Some(Err(proof_skill_error_json(
                        skill,
                        "proof chain is unavailable without LangGraph-projected workflow state",
                    )));
                    break;
                };
                found = Some(proof_chain_json(chain, workflow_state).map_err(proof_error_json));
                break;
            }
        }
        found
    };
    match in_memory {
        Some(Ok(value)) => proof_json(ProofApiReadSurface::Chain, value).await,
        Some(Err(error)) => proof_json(ProofApiReadSurface::Error, error).await,
        None => {
            proof_json(
                ProofApiReadSurface::Error,
                proof_error_json("Chain not found"),
            )
            .await
        }
    }
}

async fn verify_chain(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let resolved = {
        let session = state.session.read().await;
        let mut found = None;
        for (skill, chain) in session.proof_chains() {
            if chain.session_id == session_id {
                let Some(workflow_state) = session.graph_workflow(skill).cloned() else {
                    found = Some(Err(proof_skill_error_json(
                        skill,
                        "proof chain verification requires LangGraph-projected workflow state",
                    )));
                    break;
                };
                found = Some(Ok::<_, serde_json::Value>((chain.clone(), workflow_state)));
                break;
            }
        }
        found
    };
    match resolved {
        Some(Ok((chain, workflow_state))) => {
            let verify_key = match crate::mcp_cmd::load_verify_key_from_env() {
                Ok(key) => key,
                Err(error) => {
                    return proof_json(
                        ProofApiReadSurface::Error,
                        attach_proof_authority(
                            proof_error_json(format!(
                                "proof signature verification unavailable: {error:#}"
                            )),
                            &workflow_state,
                        ),
                    )
                    .await;
                }
            };
            proof_json(
                ProofApiReadSurface::Verify,
                attach_proof_authority(
                    verify_chain_json(&chain, Some(verify_key)),
                    &workflow_state,
                ),
            )
            .await
        }
        Some(Err(error)) => proof_json(ProofApiReadSurface::Error, error).await,
        None => {
            proof_json(
                ProofApiReadSurface::Error,
                proof_error_json("Chain not found"),
            )
            .await
        }
    }
}

async fn proof_list_json(
    response: serde_json::Value,
) -> Result<Json<serde_json::Value>, StatusCode> {
    attach_proof_list_graph_audit(response)
        .await
        .map(Json)
        .map_err(|error| {
            tracing::error!(
                error = %error,
                "proof API read graph audit failed; refusing unaudited response"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn proof_json(
    surface: ProofApiReadSurface,
    response: serde_json::Value,
) -> Result<Json<serde_json::Value>, StatusCode> {
    attach_proof_api_read_graph_audit(surface, response)
        .await
        .map(Json)
        .map_err(|error| {
            tracing::error!(
                surface = sentinel_infrastructure::proof_api_read_graph::proof_api_read_surface_label(surface),
                error = %error,
                "proof API read graph audit failed; refusing unaudited response"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn attach_proof_list_graph_audit(
    mut response: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let graph = sentinel_infrastructure::proof_api_read_graph::build_proof_api_read_graph()
        .await
        .map_err(|e| format!("build proof API read graph: {e}"))?;
    let chains = response
        .get_mut("chains")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| "proof API list response must contain chains array".to_string())?;
    for chain in chains {
        let raw = std::mem::take(chain);
        *chain =
            attach_proof_api_read_graph_audit_with_graph(&graph, ProofApiReadSurface::Summary, raw)
                .await?;
    }
    attach_proof_api_read_graph_audit_with_graph(&graph, ProofApiReadSurface::List, response).await
}

async fn attach_proof_api_read_graph_audit(
    surface: ProofApiReadSurface,
    response: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let graph = sentinel_infrastructure::proof_api_read_graph::build_proof_api_read_graph()
        .await
        .map_err(|e| format!("build proof API read graph: {e}"))?;
    attach_proof_api_read_graph_audit_with_graph(&graph, surface, response).await
}

async fn attach_proof_api_read_graph_audit_with_graph(
    graph: &ProofApiReadGraph,
    surface: ProofApiReadSurface,
    mut response: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let graph_audit = run_proof_api_read_graph_audit(graph, surface, &response).await?;
    response
        .as_object_mut()
        .ok_or_else(|| {
            "proof API read graph audit can only attach to object responses".to_string()
        })?
        .insert("graph_audit".to_string(), graph_audit);
    Ok(response)
}

async fn run_proof_api_read_graph_audit(
    graph: &ProofApiReadGraph,
    surface: ProofApiReadSurface,
    response: &serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let response_hash = sentinel_infrastructure::proof_api_read_graph::sha256_json(response);
    let surface_label =
        sentinel_infrastructure::proof_api_read_graph::proof_api_read_surface_label(surface);
    let identifier = proof_api_read_identifier(surface_label, response, &response_hash);
    let state = sentinel_infrastructure::proof_api_read_graph::ProofApiReadState::from_response(
        surface, identifier, response,
    );
    let run = sentinel_infrastructure::proof_api_read_graph::run_proof_api_read_decision_report(
        graph, state,
    )
    .await
    .map_err(|e| format!("run proof API read graph: {e}"))?;
    let authorization = run
        .proof_api_read_authorization()
        .map_err(|e| format!("proof API read graph authorization failed: {e}"))?
        .ok_or_else(|| "proof API read graph produced no terminal checkpoint".to_string())?;
    let graph_runs = sentinel_infrastructure::paths::sentinel_root()
        .join("metrics")
        .join("proof-api-read.graph-runs.jsonl");
    if let Some(parent) = graph_runs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "create proof API read graph audit dir {}: {e}",
                parent.display()
            )
        })?;
    }
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "proof_api_read",
        "surface": surface_label,
        "response_sha256": response_hash,
        "decision": sentinel_infrastructure::proof_api_read_graph::
            proof_api_read_decision_label(authorization.decision()),
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
                "open proof API read graph audit {}: {e}",
                graph_runs.display()
            )
        })?;
    serde_json::to_writer(&mut file, &row).map_err(|e| {
        format!(
            "write proof API read graph audit {}: {e}",
            graph_runs.display()
        )
    })?;
    file.write_all(b"\n").map_err(|e| {
        format!(
            "terminate proof API read graph audit {}: {e}",
            graph_runs.display()
        )
    })?;
    Ok(serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "proof_api_read",
        "surface": surface_label,
        "graph_runs_path": graph_runs,
        "response_sha256": row["response_sha256"].clone(),
        "decision": row["decision"].clone(),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": row["thread_id"].clone(),
        "run": row["run"].clone(),
    }))
}

fn proof_api_read_identifier(
    surface_label: &str,
    response: &serde_json::Value,
    response_hash: &str,
) -> String {
    let id = if let Some(session_id) = response
        .get("session_id")
        .and_then(serde_json::Value::as_str)
    {
        format!("session-id-present-true:{session_id}")
    } else if let Some(skill) = response.get("skill").and_then(serde_json::Value::as_str) {
        format!("session-id-present-false:skill-present-true:{skill}")
    } else {
        "session-id-present-false:skill-present-false".to_string()
    };
    format!("{surface_label}-{id}:response-{response_hash}")
}

fn proof_error_json(error: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "workflow_authority": "langgraph",
        "error": error.into(),
    })
}

fn proof_skill_error_json(skill: &str, error: impl Into<String>) -> serde_json::Value {
    let mut value = proof_error_json(error);
    value
        .as_object_mut()
        .expect("proof error JSON is an object")
        .insert("skill".to_string(), serde_json::json!(skill));
    value
}

fn proof_summary_json(
    skill: &str,
    chain: &ProofChain,
    workflow_state: &WorkflowState,
) -> serde_json::Value {
    serde_json::json!({
        "workflow_authority": "langgraph",
        "skill": skill,
        "session_id": chain.session_id,
        "phases": chain.phase_count(),
        "complete": chain.complete,
        "chain_valid": chain.chain_valid,
        "graph_workflow": workflow_state,
    })
}

fn proof_chain_json(
    chain: &ProofChain,
    workflow_state: &WorkflowState,
) -> Result<serde_json::Value, String> {
    let value = serde_json::to_value(chain)
        .map_err(|e| format!("proof chain serialization failed: {e}"))?;
    Ok(attach_proof_authority(value, workflow_state))
}

fn attach_proof_authority(
    mut value: serde_json::Value,
    workflow_state: &WorkflowState,
) -> serde_json::Value {
    let obj = value
        .as_object_mut()
        .expect("proof authority payload must be a JSON object");
    obj.insert(
        "workflow_authority".to_string(),
        serde_json::json!("langgraph"),
    );
    obj.insert(
        "graph_workflow".to_string(),
        serde_json::json!(workflow_state),
    );
    value
}

fn verify_chain_json(
    chain: &ProofChain,
    verify_key: Option<ed25519_dalek::VerifyingKey>,
) -> serde_json::Value {
    let mut verification = chain.verify();
    let mut signature_json = serde_json::json!({
        "checked": false,
        "required": true,
        "verified": 0,
        "unsigned": 0,
        "failures": [],
    });

    if let Some(key) = verify_key {
        let report = chain.verify_signatures(&key);
        if !report.is_ok() {
            verification.valid = false;
            for entry_id in &report.failures {
                verification.errors.push(format!(
                    "signature verification failed for entry {entry_id}"
                ));
            }
        }
        signature_json = serde_json::json!({
            "checked": true,
            "required": true,
            "verified": report.verified,
            "unsigned": report.unsigned,
            "failures": report.failures,
        });
    } else {
        verification.valid = false;
        verification
            .errors
            .push("SENTINEL_VERIFY_KEY is required for proof signature verification".to_string());
    }

    let mut value =
        serde_json::to_value(verification).expect("ChainVerification serialization must succeed");
    value
        .as_object_mut()
        .expect("ChainVerification serializes as object")
        .insert("signatures".to_string(), signature_json);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use sentinel_domain::judge::JudgeVerdict;
    use sentinel_domain::proof::GENESIS_HASH;
    use sentinel_domain::state::SessionState;
    use sentinel_domain::step_proof::StepProof;
    use sentinel_domain::workflow::WorkflowState;
    use std::sync::Arc;
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
        previous_decision_backend: Option<std::ffi::OsString>,
        previous_decision_pg_url: Option<std::ffi::OsString>,
        previous_decision_pg_schema: Option<std::ffi::OsString>,
    }

    impl CheckpointerEnvGuard {
        fn force_sqlite() -> Self {
            let guard = Self {
                previous_decision_backend: std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER"),
                previous_decision_pg_url: std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_URL"),
                previous_decision_pg_schema: std::env::var_os(
                    "SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA",
                ),
            };
            std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
            std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
            std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");
            guard
        }

        fn unsupported_decision_backend() -> Self {
            let guard = Self {
                previous_decision_backend: std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER"),
                previous_decision_pg_url: std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_URL"),
                previous_decision_pg_schema: std::env::var_os(
                    "SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA",
                ),
            };
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

    fn step_chain(signing_key: Option<&SigningKey>) -> ProofChain {
        let evidence = sentinel_domain::evidence::Evidence::default();
        let evidence_hash = StepProof::compute_evidence_hash(&evidence);
        let artifact = serde_json::Value::Null;
        let artifact_hash = StepProof::compute_artifact_hash(&artifact);
        let combined_hash = StepProof::compute_combined_hash(
            "1",
            "claim",
            "linear",
            &evidence_hash,
            &artifact_hash,
            GENESIS_HASH,
            true,
        );
        let now = chrono::Utc::now();
        let mut step = StepProof {
            step_id: "1".into(),
            phase_id: "claim".into(),
            skill: "linear".into(),
            session_id: "sess".into(),
            evidence,
            evidence_hash,
            artifact,
            artifact_hash,
            account_context: None,
            previous_hash: GENESIS_HASH.into(),
            combined_hash,
            judge_model: "sonnet".into(),
            judge_verdict: JudgeVerdict::pass(0.95, "ok"),
            signature: None,
            trace_context: None,
            started_at: now,
            completed_at: now,
            duration_ms: 1,
        };
        if let Some(key) = signing_key {
            step.sign_with(key);
        }
        let mut chain = ProofChain::new("linear", "sess");
        chain.add_step_proof(step).expect("step proof chains");
        chain
    }

    fn unsigned_step_chain() -> ProofChain {
        step_chain(None)
    }

    fn signed_step_chain(signing_key: &SigningKey) -> ProofChain {
        step_chain(Some(signing_key))
    }

    fn app_state_with_chain(project_graph_workflow: bool) -> AppState {
        app_state_with_proof_chain(project_graph_workflow, unsigned_step_chain())
    }

    fn app_state_with_proof_chain(project_graph_workflow: bool, chain: ProofChain) -> AppState {
        let mut state = SessionState::new("sess");
        state.restore_proof_chain("linear", chain);
        if project_graph_workflow {
            state.set_graph_projected_workflow("linear", WorkflowState::new("linear", "sess"));
        }
        AppState {
            session: Arc::new(RwLock::new(state)),
        }
    }

    #[test]
    fn api_verify_fails_closed_when_signatures_required_without_verify_key() {
        let json = verify_chain_json(&unsigned_step_chain(), None);

        assert_eq!(json["valid"], false);
        assert_eq!(json["signatures"]["required"], true);
        assert_eq!(json["signatures"]["checked"], false);
        assert!(
            json["errors"]
                .as_array()
                .expect("errors array")
                .iter()
                .any(|error| error
                    .as_str()
                    .is_some_and(|text| text.contains("SENTINEL_VERIFY_KEY"))),
            "verification error should identify missing verify key: {json}"
        );
    }

    #[tokio::test]
    async fn api_list_proofs_requires_langgraph_workflow_projection() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();

        let Json(json) = list_proofs(State(app_state_with_chain(false)))
            .await
            .expect("proof API read graph audit");

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["graph"], "proof_api_read");
        assert_eq!(json["graph_audit"]["surface"], "error");
        assert!(json["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(json["skill"], "linear");
        assert!(
            json["error"]
                .as_str()
                .is_some_and(|err| err.contains("LangGraph-projected workflow state")),
            "graphless proof list should fail closed: {json}"
        );
    }

    #[tokio::test]
    async fn api_list_proofs_fails_closed_when_read_graph_audit_cannot_run() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::unsupported_decision_backend();

        let err = list_proofs(State(app_state_with_chain(true)))
            .await
            .expect_err("proof API must refuse unaudited responses");

        assert_eq!(err, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn api_get_proof_chain_requires_langgraph_workflow_projection() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();

        let Json(json) =
            get_proof_chain(State(app_state_with_chain(false)), Path("sess".to_string()))
                .await
                .expect("proof API read graph audit");

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["graph"], "proof_api_read");
        assert_eq!(json["graph_audit"]["surface"], "error");
        assert!(json["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(json["skill"], "linear");
        assert!(
            json["error"]
                .as_str()
                .is_some_and(|err| err.contains("LangGraph-projected workflow state")),
            "graphless proof chain read should fail closed: {json}"
        );
    }

    #[tokio::test]
    async fn api_get_proof_chain_includes_langgraph_workflow_projection() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();

        let Json(json) =
            get_proof_chain(State(app_state_with_chain(true)), Path("sess".to_string()))
                .await
                .expect("proof API read graph audit");

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["graph"], "proof_api_read");
        assert_eq!(json["graph_audit"]["surface"], "chain");
        assert!(json["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(json["skill"], "linear");
        assert_eq!(json["graph_workflow"]["skill"], "linear");
        assert_eq!(json["graph_workflow"]["session_id"], "sess");
        assert_eq!(json["entries"].as_array().expect("entries").len(), 1);
    }

    #[tokio::test]
    async fn api_list_proofs_includes_graph_audit_for_summaries_and_envelope() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();

        let Json(json) = list_proofs(State(app_state_with_chain(true)))
            .await
            .expect("proof API read graph audit");

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["graph"], "proof_api_read");
        assert_eq!(json["graph_audit"]["surface"], "list");
        assert!(json["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(json["chains"][0]["workflow_authority"], "langgraph");
        assert_eq!(json["chains"][0]["graph_audit"]["graph"], "proof_api_read");
        assert_eq!(json["chains"][0]["graph_audit"]["surface"], "summary");
        assert!(json["chains"][0]["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        let graph_rows = std::fs::read_to_string(
            tmp.path()
                .join(".claude")
                .join("sentinel")
                .join("metrics")
                .join("proof-api-read.graph-runs.jsonl"),
        )
        .expect("proof API read graph audit rows");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"proof_api_read\""));
        assert!(graph_rows.contains("\"surface\":\"summary\""));
        assert!(graph_rows.contains("\"surface\":\"list\""));
        assert!(graph_rows.contains("\"authorization_checkpoint\""));
    }

    #[tokio::test]
    async fn api_verify_chain_includes_langgraph_workflow_projection() {
        let _env = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = EnvGuard::set_sentinel_home(tmp.path());
        let _checkpointer_env = CheckpointerEnvGuard::force_sqlite();
        let prev_verify = std::env::var_os("SENTINEL_VERIFY_KEY");
        let signing_key = SigningKey::from_bytes(&[11_u8; 32]);
        std::env::set_var(
            "SENTINEL_VERIFY_KEY",
            hex::encode(signing_key.verifying_key().as_bytes()),
        );

        let Json(json) = verify_chain(
            State(app_state_with_proof_chain(
                true,
                signed_step_chain(&signing_key),
            )),
            Path("sess".to_string()),
        )
        .await
        .expect("proof API read graph audit");

        match prev_verify {
            Some(value) => std::env::set_var("SENTINEL_VERIFY_KEY", value),
            None => std::env::remove_var("SENTINEL_VERIFY_KEY"),
        }

        assert_eq!(json["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(json["graph_audit"]["graph"], "proof_api_read");
        assert_eq!(json["graph_audit"]["surface"], "verify");
        assert!(json["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(json["graph_workflow"]["skill"], "linear");
        assert_eq!(json["signatures"]["checked"], true);
        assert_eq!(json["signatures"]["required"], true);
        assert_eq!(json["signatures"]["verified"], 1);
        assert_eq!(json["signatures"]["unsigned"], 0);
    }

    #[test]
    fn proof_api_identifier_records_absent_keys_explicitly() {
        let identifier =
            proof_api_read_identifier("error", &serde_json::json!({"error": "boom"}), "abc123");

        assert_eq!(
            identifier,
            "error-session-id-present-false:skill-present-false:response-abc123"
        );
        assert!(!identifier.contains("proof-api"));
    }
}
