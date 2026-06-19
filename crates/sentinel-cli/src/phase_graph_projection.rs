//! Shared phase-graph projection helpers for CLI surfaces.
//!
//! Hooks, MCP tools, and local API endpoints all need to read the same durable
//! LangGraph checkpoints. Keeping the path and projection logic here prevents
//! those surfaces from drifting into separate workflow authorities.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{SkillWorkflow, WorkflowState};
use sentinel_graph::{
    CompiledPhaseGraph, PhaseGraphCheckpointSnapshot, PhaseGraphIntrospection, PhaseGraphState,
    PhaseGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhaseGraphActivation {
    CreatedGateCheckpoint,
    ProjectedExistingCheckpoint,
}

pub(crate) fn load_workflow_configs() -> Result<HashMap<String, SkillWorkflow>> {
    let config_dir = sentinel_infrastructure::config::config_dir();
    Ok(
        sentinel_infrastructure::config::load_workflows(&config_dir)?
            .into_iter()
            .map(|w| (w.skill.clone(), w))
            .collect(),
    )
}

/// Resolve the per-session phase-graph sqlite path under the sentinel state dir.
pub(crate) fn phase_graph_db_path(session_id: &str) -> Result<String> {
    sentinel_infrastructure::state_store::sanitize_session_id(session_id)?;
    let dir = sentinel_infrastructure::state_store::state_dir().join("phase-graphs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir
        .join(format!("{session_id}.db"))
        .to_string_lossy()
        .to_string())
}

fn needs_empty_graph_projection(state: &SessionState, skill: &str) -> bool {
    state.has_graph_workflow(skill) || state.phases_read.contains_key(skill)
}

/// Project one configured workflow from the authoritative durable graph.
///
/// Returns `Ok(None)` when the workflow is not configured or when the graph has
/// no checkpoint for this session yet. Callers that need an "unstarted" view
/// should render that explicitly instead of manufacturing workflow progress.
pub(crate) async fn graph_latest_workflow_state(
    skill: &str,
    session_id: &str,
    workflow_configs: &HashMap<String, SkillWorkflow>,
) -> Result<Option<WorkflowState>> {
    let Some(workflow) = workflow_configs.get(skill) else {
        return Ok(None);
    };

    let db_path = phase_graph_db_path(session_id)?;
    let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(
        validated_latest_phase_graph_state(&graph, skill, session_id)
            .await?
            .map(|state| state.to_workflow_state()),
    )
}

/// Reflect one configured workflow's compiled LangGraph topology.
pub(crate) async fn graph_introspection(
    skill: &str,
    session_id: &str,
    workflow_configs: &HashMap<String, SkillWorkflow>,
) -> Result<Option<PhaseGraphIntrospection>> {
    let Some(workflow) = workflow_configs.get(skill) else {
        return Ok(None);
    };

    let db_path = phase_graph_db_path(session_id)?;
    let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    graph
        .introspect(session_id)
        .map(Some)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Project one configured workflow's durable checkpoint history as JSON.
pub(crate) async fn graph_checkpoint_projection(
    skill: &str,
    session_id: &str,
    workflow_configs: &HashMap<String, SkillWorkflow>,
) -> Result<Option<serde_json::Value>> {
    let Some(workflow) = workflow_configs.get(skill) else {
        return Ok(None);
    };

    let db_path = phase_graph_db_path(session_id)?;
    let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let snapshots = graph
        .phase_snapshots(session_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !snapshots.is_empty() {
        let writes = graph
            .phase_writes_history(session_id, None)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let expected_thread_id = graph
            .thread_id_for_session(session_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        validate_phase_graph_projection(
            skill,
            session_id,
            &expected_thread_id,
            &snapshots,
            &writes,
        )?;
    }
    let entries: Vec<serde_json::Value> = snapshots
        .iter()
        .map(|snapshot| {
            let state = &snapshot.state;
            serde_json::json!({
                "checkpoint_id": snapshot.checkpoint_id,
                "parent_checkpoint_id": snapshot.parent_checkpoint_id,
                "thread_id": snapshot.thread_id,
                "step_number": snapshot.step_number,
                "created_at": snapshot.created_at,
                "source": snapshot.source,
                "writes": snapshot.writes,
                "tags": snapshot.tags,
                "skill": state.skill,
                "session_id": state.session_id,
                "current_phase": state.current_phase,
                "completed_phases": state.completed_phases,
                "current_step": state.current_step,
                "step_states": state.step_states,
                "complete": state.complete,
                "state": state,
            })
        })
        .collect();
    Ok(Some(serde_json::json!(entries)))
}

/// Project one configured workflow's durable LangGraph state timeline as JSON.
///
/// This intentionally reads through `CompiledPhaseGraph::phase_history` and
/// then validates that state-only history against the metadata-rich checkpoint
/// stream. Clients get a compact state timeline without gaining a second
/// workflow authority.
pub(crate) async fn graph_history_projection(
    skill: &str,
    session_id: &str,
    workflow_configs: &HashMap<String, SkillWorkflow>,
) -> Result<Option<serde_json::Value>> {
    let Some(workflow) = workflow_configs.get(skill) else {
        return Ok(None);
    };

    let db_path = phase_graph_db_path(session_id)?;
    let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let snapshots = graph
        .phase_snapshots(session_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if snapshots.is_empty() {
        return Ok(None);
    }
    let writes = graph
        .phase_writes_history(session_id, None)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let expected_thread_id = graph
        .thread_id_for_session(session_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    validate_phase_graph_projection(skill, session_id, &expected_thread_id, &snapshots, &writes)?;

    let history = graph
        .phase_history(session_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if history.len() != snapshots.len()
        || history
            .iter()
            .zip(snapshots.iter())
            .any(|(state, snapshot)| state != &snapshot.state)
    {
        return Err(anyhow::anyhow!(
            "LangGraph phase history for '{skill}' diverged from checkpoint state history"
        ));
    }

    Ok(Some(serde_json::json!(history)))
}

/// Project one configured workflow's checkpoint write history as JSON.
pub(crate) async fn graph_writes_projection(
    skill: &str,
    session_id: &str,
    workflow_configs: &HashMap<String, SkillWorkflow>,
    channel: Option<&str>,
) -> Result<Option<serde_json::Value>> {
    let Some(workflow) = workflow_configs.get(skill) else {
        return Ok(None);
    };

    let db_path = phase_graph_db_path(session_id)?;
    let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let snapshots = graph
        .phase_snapshots(session_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if snapshots.is_empty() {
        return Ok(None);
    }
    let writes = graph
        .phase_writes_history(session_id, channel)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if channel.is_none() || channel == Some("state") {
        let expected_thread_id = graph
            .thread_id_for_session(session_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        validate_phase_graph_projection(
            skill,
            session_id,
            &expected_thread_id,
            &snapshots,
            &writes,
        )?;
    }
    Ok(Some(serde_json::json!(writes)))
}

/// Project every configured workflow from the durable graph.
///
/// Session state is refreshed from LangGraph checkpoints. Configured workflows
/// without a durable checkpoint are omitted, and unconfigured projected workflow
/// entries are dropped instead of being preserved as a second source of truth.
pub(crate) async fn graph_projected_workflows(
    session_id: &str,
    workflow_configs: &HashMap<String, SkillWorkflow>,
) -> Result<HashMap<String, WorkflowState>> {
    let mut projected = HashMap::new();
    if workflow_configs.is_empty() {
        return Ok(projected);
    }

    let db_path = phase_graph_db_path(session_id)?;
    let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    for (skill, workflow) in workflow_configs {
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver.clone())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if let Some(state) = validated_latest_phase_graph_state(&graph, skill, session_id).await? {
            projected.insert(skill.clone(), state.to_workflow_state());
        }
    }
    Ok(projected)
}

async fn validated_latest_phase_graph_state(
    graph: &CompiledPhaseGraph,
    skill: &str,
    session_id: &str,
) -> Result<Option<PhaseGraphState>> {
    let snapshots = graph
        .phase_snapshots(session_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if snapshots.is_empty() {
        return Ok(None);
    }
    let writes = graph
        .phase_writes_history(session_id, None)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let expected_thread_id = graph
        .thread_id_for_session(session_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    validate_phase_graph_projection(skill, session_id, &expected_thread_id, &snapshots, &writes)?;
    Ok(snapshots.last().map(|snapshot| snapshot.state.clone()))
}

fn validate_phase_graph_projection(
    skill: &str,
    session_id: &str,
    expected_thread_id: &str,
    snapshots: &[PhaseGraphCheckpointSnapshot],
    write_history: &[PhaseGraphWriteHistoryEntry],
) -> Result<()> {
    let Some(latest) = snapshots.last() else {
        return Err(anyhow::anyhow!(
            "LangGraph phase projection for '{skill}' omitted checkpoint history"
        ));
    };
    if latest.thread_id != expected_thread_id {
        return Err(anyhow::anyhow!(
            "LangGraph phase projection latest checkpoint thread mismatch for '{skill}': expected '{expected_thread_id}', got '{}'",
            latest.thread_id
        ));
    }
    if let Some(mismatched) = snapshots
        .iter()
        .find(|snapshot| snapshot.thread_id != expected_thread_id)
    {
        return Err(anyhow::anyhow!(
            "LangGraph phase projection for '{skill}' contains checkpoint thread '{}', expected '{expected_thread_id}'",
            mismatched.thread_id
        ));
    }
    for pair in snapshots.windows(2) {
        if pair[0].step_number > pair[1].step_number {
            return Err(anyhow::anyhow!(
                "LangGraph phase projection checkpoint history for '{skill}' is not oldest-first: checkpoint '{}' step {} appears before checkpoint '{}' step {}",
                pair[0].checkpoint_id,
                pair[0].step_number,
                pair[1].checkpoint_id,
                pair[1].step_number
            ));
        }
    }
    if let Some(mismatched) = write_history
        .iter()
        .find(|write| write.thread_id != expected_thread_id)
    {
        return Err(anyhow::anyhow!(
            "LangGraph phase projection write history for '{skill}' contains thread '{}', expected '{expected_thread_id}'",
            mismatched.thread_id
        ));
    }
    for pair in write_history.windows(2) {
        if pair[0].step_number > pair[1].step_number {
            return Err(anyhow::anyhow!(
                "LangGraph phase projection write history for '{skill}' is not oldest-first: checkpoint '{}' step {} appears before checkpoint '{}' step {}",
                pair[0].checkpoint_id,
                pair[0].step_number,
                pair[1].checkpoint_id,
                pair[1].step_number
            ));
        }
    }
    if latest.state.skill != skill {
        return Err(anyhow::anyhow!(
            "LangGraph phase projection latest checkpoint skill mismatch: expected '{skill}', got '{}'",
            latest.state.skill
        ));
    }
    if latest.state.session_id != session_id {
        return Err(anyhow::anyhow!(
            "LangGraph phase projection latest checkpoint session mismatch for '{skill}': expected '{session_id}', got '{}'",
            latest.state.session_id
        ));
    }

    let terminal_json = serde_json::to_value(&latest.state).map_err(|err| {
        anyhow::anyhow!(
            "LangGraph phase projection failed to serialize latest state for '{skill}': {err}"
        )
    })?;
    let latest_state_writes: Vec<_> = latest
        .writes
        .iter()
        .filter(|write| write.channel == "state")
        .collect();
    if latest_state_writes.is_empty() {
        return Err(anyhow::anyhow!(
            "LangGraph phase projection latest checkpoint '{}' omitted state-channel write metadata for '{skill}'",
            latest.checkpoint_id
        ));
    }

    let latest_write = write_history
        .iter()
        .find(|write| {
            write.checkpoint_id == latest.checkpoint_id
                && write.channel == "state"
                && latest_state_writes
                    .iter()
                    .any(|metadata| metadata.node_id == write.node_id && metadata.ts == write.ts)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "LangGraph phase projection write history omitted latest state-channel write for checkpoint '{}' skill '{skill}'",
                latest.checkpoint_id
            )
        })?;
    if &latest_write.value_json != &terminal_json {
        return Err(anyhow::anyhow!(
            "LangGraph phase projection latest state-channel write mismatch for checkpoint '{}' skill '{skill}'",
            latest.checkpoint_id
        ));
    }

    Ok(())
}

/// Project graph-owned workflow state into the hook's loaded session state
/// before any workflow-aware hook evaluates it.
pub(crate) async fn project_phase_graph_workflows(
    state: &mut SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
) -> Result<()> {
    if workflows.is_empty() {
        return Ok(());
    }

    state.retain_configured_graph_workflows(|skill| workflows.contains_key(skill));

    let session_id = state.session_id.clone();
    let db_path = phase_graph_db_path(&session_id)?;
    let db_exists = Path::new(&db_path).exists();
    let should_project_any = db_exists
        || workflows
            .keys()
            .any(|skill| needs_empty_graph_projection(state, skill));
    if !should_project_any {
        return Ok(());
    }

    let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    for (skill, workflow) in workflows {
        if !db_exists && !needs_empty_graph_projection(state, skill) {
            continue;
        }

        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver.clone())
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        match graph
            .phase_snapshots(&session_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
        {
            snapshots if !snapshots.is_empty() => {
                let writes = graph
                    .phase_writes_history(&session_id, None)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let expected_thread_id = graph
                    .thread_id_for_session(&session_id)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                validate_phase_graph_projection(
                    skill,
                    &session_id,
                    &expected_thread_id,
                    &snapshots,
                    &writes,
                )?;
                let graph_state = snapshots
                    .last()
                    .expect("non-empty snapshots must have a latest entry")
                    .state
                    .clone();
                state.set_graph_projected_workflow(skill.clone(), graph_state.to_workflow_state());
            }
            _ if needs_empty_graph_projection(state, skill) => {
                state.remove_graph_projected_workflow(skill);
            }
            _ => {}
        }
    }

    Ok(())
}

/// Activate an explicitly invoked workflow by running the durable LangGraph
/// phase graph to its next interrupt checkpoint, then projecting the validated
/// checkpoint back into the hook session state.
pub(crate) async fn activate_phase_graph_workflow(
    state: &mut SessionState,
    skill: &str,
    workflow: &SkillWorkflow,
) -> Result<PhaseGraphActivation> {
    if workflow.skill != skill {
        return Err(anyhow::anyhow!(
            "LangGraph activation skill mismatch: requested '{skill}', workflow is '{}'",
            workflow.skill
        ));
    }

    let session_id = state.session_id.clone();
    let db_path = phase_graph_db_path(&session_id)?;
    let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if let Some(existing) = validated_latest_phase_graph_state(&graph, skill, &session_id).await? {
        state.set_active_skill_marker(skill);
        state.set_graph_projected_workflow(skill.to_string(), existing.to_workflow_state());
        return Ok(PhaseGraphActivation::ProjectedExistingCheckpoint);
    }

    let report = graph
        .run_until_gate_report(skill, &session_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let snapshots = graph
        .phase_snapshots(&session_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let writes = graph
        .phase_writes_history(&session_id, None)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let expected_thread_id = graph
        .thread_id_for_session(&session_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    validate_phase_graph_projection(skill, &session_id, &expected_thread_id, &snapshots, &writes)?;
    let latest = snapshots
        .last()
        .ok_or_else(|| anyhow::anyhow!("LangGraph activation omitted checkpoint history"))?;
    if latest.state != report.state {
        return Err(anyhow::anyhow!(
            "LangGraph activation latest checkpoint state did not match stream result for '{skill}'"
        ));
    }

    state.set_active_skill_marker(skill);
    state.set_graph_projected_workflow(skill.to_string(), report.state.to_workflow_state());
    Ok(PhaseGraphActivation::CreatedGateCheckpoint)
}

#[cfg(test)]
mod tests {
    use std::sync::{LockResult, MutexGuard};

    use super::*;
    use sentinel_domain::judge::JudgeModel;
    use sentinel_domain::workflow::WorkflowPhase;
    use sentinel_graph::{
        PhaseGraphCheckpointSnapshot, PhaseGraphCheckpointWrite, PhaseGraphState,
        PhaseGraphWriteHistoryEntry,
    };

    struct EnvLock;

    static ENV_LOCK: EnvLock = EnvLock;

    impl EnvLock {
        fn lock(&self) -> LockResult<MutexGuard<'static, ()>> {
            Ok(crate::test_env::lock())
        }
    }

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

    fn phase_state(skill: &str, session_id: &str, completed: &[&str]) -> PhaseGraphState {
        let mut state = PhaseGraphState::new(skill, session_id, vec!["claim".to_string()]);
        state.completed_phases = completed.iter().map(|phase| (*phase).to_string()).collect();
        state.complete = completed.contains(&"claim");
        state
    }

    fn expected_thread_id(skill: &str, session_id: &str) -> String {
        format!("sentinel.phase.{skill}.{session_id}")
    }

    fn checkpoint(
        skill: &str,
        session_id: &str,
        checkpoint_id: &str,
        step_number: u64,
        state: PhaseGraphState,
        node_id: &str,
    ) -> PhaseGraphCheckpointSnapshot {
        PhaseGraphCheckpointSnapshot {
            checkpoint_id: checkpoint_id.to_string(),
            parent_checkpoint_id: None,
            thread_id: format!("sentinel.phase.{skill}.{session_id}"),
            step_number,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            tags: Default::default(),
            source: None,
            writes: vec![PhaseGraphCheckpointWrite {
                node_id: node_id.to_string(),
                channel: "state".to_string(),
                ts: "2026-01-01T00:00:00Z".to_string(),
            }],
            state,
        }
    }

    fn write_entry(
        checkpoint_id: &str,
        step_number: u64,
        node_id: &str,
        value_json: serde_json::Value,
    ) -> PhaseGraphWriteHistoryEntry {
        let skill = value_json
            .get("skill")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("linear");
        let session_id = value_json
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("session");
        PhaseGraphWriteHistoryEntry {
            thread_id: format!("sentinel.phase.{skill}.{session_id}"),
            checkpoint_id: checkpoint_id.to_string(),
            step_number,
            channel: "state".to_string(),
            node_id: node_id.to_string(),
            ts: "2026-01-01T00:00:00Z".to_string(),
            value_len: 1,
            value_sha256: "0".repeat(64),
            value_json,
        }
    }

    #[test]
    fn validate_phase_graph_projection_accepts_matching_latest_checkpoint_and_write() {
        let skill = "linear";
        let session_id = "projection-valid";
        let older_state = phase_state(skill, session_id, &[]);
        let latest_state = phase_state(skill, session_id, &["claim"]);
        let latest_json = serde_json::to_value(&latest_state).expect("json");
        let snapshots = vec![
            checkpoint(skill, session_id, "checkpoint-1", 1, older_state, "claim"),
            checkpoint(skill, session_id, "checkpoint-2", 2, latest_state, "claim"),
        ];
        let writes = vec![
            write_entry(
                "checkpoint-1",
                1,
                "claim",
                serde_json::to_value(&snapshots[0].state).expect("json"),
            ),
            write_entry("checkpoint-2", 2, "claim", latest_json),
        ];

        let expected_thread_id = expected_thread_id(skill, session_id);
        validate_phase_graph_projection(
            skill,
            session_id,
            &expected_thread_id,
            &snapshots,
            &writes,
        )
        .expect("matching latest checkpoint and write must project");
    }

    #[test]
    fn validate_phase_graph_projection_rejects_forged_latest_write() {
        let skill = "linear";
        let session_id = "projection-forged-write";
        let latest_state = phase_state(skill, session_id, &["claim"]);
        let snapshots = vec![checkpoint(
            skill,
            session_id,
            "checkpoint-2",
            2,
            latest_state,
            "claim",
        )];
        let writes = vec![write_entry(
            "checkpoint-2",
            2,
            "claim",
            serde_json::to_value(phase_state(skill, session_id, &[])).expect("json"),
        )];

        let expected_thread_id = expected_thread_id(skill, session_id);
        let err = validate_phase_graph_projection(
            skill,
            session_id,
            &expected_thread_id,
            &snapshots,
            &writes,
        )
        .expect_err("forged latest state write must fail");
        assert!(
            err.to_string()
                .contains("latest state-channel write mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_phase_graph_projection_rejects_missing_latest_write_metadata() {
        let skill = "linear";
        let session_id = "projection-missing-metadata";
        let latest_state = phase_state(skill, session_id, &["claim"]);
        let mut latest = checkpoint(
            skill,
            session_id,
            "checkpoint-2",
            2,
            latest_state.clone(),
            "claim",
        );
        latest.writes.clear();
        let snapshots = vec![latest];
        let writes = vec![write_entry(
            "checkpoint-2",
            2,
            "claim",
            serde_json::to_value(latest_state).expect("json"),
        )];

        let expected_thread_id = expected_thread_id(skill, session_id);
        let err = validate_phase_graph_projection(
            skill,
            session_id,
            &expected_thread_id,
            &snapshots,
            &writes,
        )
        .expect_err("missing latest state write metadata must fail");
        assert!(
            err.to_string()
                .contains("omitted state-channel write metadata"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_phase_graph_projection_rejects_mismatched_write_thread() {
        let skill = "linear";
        let session_id = "projection-mismatched-write-thread";
        let latest_state = phase_state(skill, session_id, &["claim"]);
        let snapshots = vec![checkpoint(
            skill,
            session_id,
            "checkpoint-2",
            2,
            latest_state.clone(),
            "claim",
        )];
        let mut writes = vec![write_entry(
            "checkpoint-2",
            2,
            "claim",
            serde_json::to_value(latest_state).expect("json"),
        )];
        writes[0].thread_id = "sentinel.phase.linear.other-session".to_string();

        let expected_thread_id = expected_thread_id(skill, session_id);
        let err = validate_phase_graph_projection(
            skill,
            session_id,
            &expected_thread_id,
            &snapshots,
            &writes,
        )
        .expect_err("mismatched write thread must fail");
        assert!(
            err.to_string().contains("write history") && err.to_string().contains("other-session"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_phase_graph_projection_rejects_out_of_order_write_history() {
        let skill = "linear";
        let session_id = "projection-out-of-order-write";
        let older_state = phase_state(skill, session_id, &[]);
        let latest_state = phase_state(skill, session_id, &["claim"]);
        let snapshots = vec![
            checkpoint(
                skill,
                session_id,
                "checkpoint-1",
                1,
                older_state.clone(),
                "claim",
            ),
            checkpoint(
                skill,
                session_id,
                "checkpoint-2",
                2,
                latest_state.clone(),
                "claim",
            ),
        ];
        let writes = vec![
            write_entry(
                "checkpoint-2",
                2,
                "claim",
                serde_json::to_value(&latest_state).expect("json"),
            ),
            write_entry(
                "checkpoint-1",
                1,
                "claim",
                serde_json::to_value(&older_state).expect("json"),
            ),
        ];

        let expected_thread_id = expected_thread_id(skill, session_id);
        let err = validate_phase_graph_projection(
            skill,
            session_id,
            &expected_thread_id,
            &snapshots,
            &writes,
        )
        .expect_err("out-of-order write history must fail");
        assert!(
            err.to_string().contains("write history")
                && err.to_string().contains("not oldest-first"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_phase_graph_projection_rejects_out_of_order_checkpoint_history() {
        let skill = "linear";
        let session_id = "projection-out-of-order";
        let older_state = phase_state(skill, session_id, &[]);
        let latest_state = phase_state(skill, session_id, &["claim"]);
        let snapshots = vec![
            checkpoint(
                skill,
                session_id,
                "checkpoint-2",
                2,
                latest_state.clone(),
                "claim",
            ),
            checkpoint(
                skill,
                session_id,
                "checkpoint-1",
                1,
                older_state.clone(),
                "claim",
            ),
        ];
        let writes = vec![
            write_entry(
                "checkpoint-2",
                2,
                "claim",
                serde_json::to_value(latest_state).expect("json"),
            ),
            write_entry(
                "checkpoint-1",
                1,
                "claim",
                serde_json::to_value(older_state).expect("json"),
            ),
        ];

        let expected_thread_id = expected_thread_id(skill, session_id);
        let err = validate_phase_graph_projection(
            skill,
            session_id,
            &expected_thread_id,
            &snapshots,
            &writes,
        )
        .expect_err("out-of-order checkpoint history must fail");
        assert!(
            err.to_string().contains("not oldest-first"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn workflow_config_parse_error_is_not_treated_as_empty_config() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());

        let config_dir = sentinel_infrastructure::config::config_dir();
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(config_dir.join("workflows.toml"), "not valid toml =")
            .expect("write config");

        let err = load_workflow_configs().expect_err("invalid config must be a hard error");
        assert!(err.to_string().contains("Failed to parse workflows.toml"));
    }

    #[test]
    fn missing_workflow_config_is_not_treated_as_empty_config() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());

        let err = load_workflow_configs().expect_err("missing config must be a hard error");
        assert!(
            err.to_string().contains("Failed to read")
                && err.to_string().contains("workflows.toml"),
            "missing authoritative workflow config must fail closed: {err:#}"
        );
    }

    #[test]
    fn phase_graph_db_path_uses_authoritative_state_dir_and_validates_session_id() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());

        let db_path = phase_graph_db_path("safe-session").expect("db path");
        let expected = sentinel_infrastructure::state_store::state_dir()
            .join("phase-graphs")
            .join("safe-session.db");
        assert_eq!(std::path::PathBuf::from(db_path), expected);

        let err = phase_graph_db_path("../escape").expect_err("path traversal must be rejected");
        assert!(
            err.to_string().contains("path traversal"),
            "unexpected error: {err}"
        );
    }

    fn workflow(skill: &str) -> SkillWorkflow {
        SkillWorkflow {
            skill: skill.to_string(),
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

    #[tokio::test(flavor = "current_thread")]
    async fn graph_projected_workflows_omits_configured_workflow_without_checkpoint() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let workflows = HashMap::from([("linear".to_string(), workflow("linear"))]);

        let projected = graph_projected_workflows("projection-empty", &workflows)
            .await
            .expect("projection");

        assert!(
            projected.is_empty(),
            "configured workflows without durable checkpoints must not be synthesized"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn graph_projected_workflows_reads_only_durable_checkpoint_state() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let session_id = "projection-checkpoint";
        let workflow = workflow("linear");
        let workflows = HashMap::from([("linear".to_string(), workflow.clone())]);

        let db_path = phase_graph_db_path(session_id).expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(&workflow, saver)
            .expect("compile");
        graph
            .run_until_gate("linear", session_id)
            .await
            .expect("initial gate checkpoint");
        graph
            .apply_verdict("linear", session_id, "claim", true)
            .await
            .expect("checkpointed verdict");

        let projected = graph_projected_workflows(session_id, &workflows)
            .await
            .expect("projection");
        let state = projected.get("linear").expect("projected workflow");
        assert_eq!(state.completed_phases, vec!["claim".to_string()]);
        assert!(state.complete);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn graph_history_projection_reads_validated_phase_history() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let session_id = "projection-history";
        let workflow = workflow("linear");
        let workflows = HashMap::from([("linear".to_string(), workflow.clone())]);

        let db_path = phase_graph_db_path(session_id).expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(&workflow, saver)
            .expect("compile");
        graph
            .run_until_gate("linear", session_id)
            .await
            .expect("initial gate checkpoint");
        graph
            .apply_verdict("linear", session_id, "claim", true)
            .await
            .expect("checkpointed verdict");

        let history = graph_history_projection("linear", session_id, &workflows)
            .await
            .expect("history projection")
            .expect("durable history");
        let entries = history.as_array().expect("history array");

        assert!(
            entries.len() >= 2,
            "history must include the gate state and the completed terminal state"
        );
        assert_eq!(entries[0]["skill"], "linear");
        assert_eq!(entries[0]["completed_phases"], serde_json::json!([]));
        let latest = entries.last().expect("latest history entry");
        assert_eq!(latest["completed_phases"], serde_json::json!(["claim"]));
        assert_eq!(latest["complete"], true);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn activate_phase_graph_workflow_creates_gate_checkpoint_projection() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let workflow = workflow("linear");
        let mut state = SessionState::new("activate-gate");

        let activation = activate_phase_graph_workflow(&mut state, "linear", &workflow)
            .await
            .expect("activate graph");

        assert_eq!(activation, PhaseGraphActivation::CreatedGateCheckpoint);
        assert_eq!(state.active_skill.as_deref(), Some("linear"));
        let projected = state.graph_workflow("linear").expect("graph projection");
        assert_eq!(projected.skill, "linear");
        assert_eq!(projected.current_phase, Some(0));
        assert!(projected.completed_phases.is_empty());

        let db_path = phase_graph_db_path("activate-gate").expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(&workflow, saver)
            .expect("compile");
        let snapshots = graph
            .phase_snapshots("activate-gate")
            .await
            .expect("snapshots");
        assert!(
            !snapshots.is_empty(),
            "activation must persist LangGraph checkpoint history"
        );
        assert_eq!(
            snapshots.last().expect("latest checkpoint").state.skill,
            "linear"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn activate_phase_graph_workflow_reprojects_existing_checkpoint() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let workflow = workflow("linear");
        let mut state = SessionState::new("activate-existing");

        let first = activate_phase_graph_workflow(&mut state, "linear", &workflow)
            .await
            .expect("initial activation");
        assert_eq!(first, PhaseGraphActivation::CreatedGateCheckpoint);

        let db_path = phase_graph_db_path("activate-existing").expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(&workflow, saver)
            .expect("compile");
        let initial_checkpoint_count = graph
            .phase_snapshots("activate-existing")
            .await
            .expect("initial snapshots")
            .len();

        state.remove_graph_projected_workflow("linear");
        state.active_skill = None;

        let second = activate_phase_graph_workflow(&mut state, "linear", &workflow)
            .await
            .expect("project existing activation");

        assert_eq!(second, PhaseGraphActivation::ProjectedExistingCheckpoint);
        assert_eq!(state.active_skill.as_deref(), Some("linear"));
        assert!(state.has_graph_workflow("linear"));

        let snapshots = graph
            .phase_snapshots("activate-existing")
            .await
            .expect("snapshots");
        assert_eq!(
            snapshots.len(),
            initial_checkpoint_count,
            "reprojecting an existing checkpoint must not run another gate"
        );
    }
}
