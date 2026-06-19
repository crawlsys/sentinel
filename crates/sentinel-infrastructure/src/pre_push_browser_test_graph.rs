//! Graph-backed pre-push browser-test authorization.
//!
//! The application hook computes deterministic facts for `git push`: whether
//! the current repo is browser-test configured, whether the branch diff includes
//! frontend files, and whether the current session has recent browser evidence.
//! This graph authorizes the resulting allow/block decision through durable
//! LangGraph checkpoints before the CLI permits source-control mutation.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::pre_push_browser_test::PrePushBrowserEvaluation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PrePushBrowserDecision {
    #[default]
    Unclassified,
    Allow,
    AllowNoBrowserConfig,
    AllowNoFrontendChanges,
    AllowRecentBrowserTest,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrePushBrowserState {
    pub identifier: String,
    pub tool: Option<String>,
    pub bash_tool: bool,
    pub command_present: bool,
    pub command_sha256: Option<String>,
    pub git_push: bool,
    pub repo_browser_test_configured: bool,
    pub frontend_changes: bool,
    pub session_id_present: bool,
    pub recent_browser_test: bool,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: PrePushBrowserDecision,
}

impl PrePushBrowserState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &PrePushBrowserEvaluation,
    ) -> Self {
        let command_sha256 = evaluation
            .command
            .as_deref()
            .filter(|_| evaluation.command_present)
            .map(command_sha256);
        let blocking_finding_count = u64::from(expected_should_block(
            evaluation.bash_tool,
            evaluation.git_push,
            evaluation.repo_browser_test_configured,
            evaluation.frontend_changes,
            evaluation.recent_browser_test,
        ));
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            bash_tool: evaluation.bash_tool,
            command_present: evaluation.command_present,
            command_sha256,
            git_push: evaluation.git_push,
            repo_browser_test_configured: evaluation.repo_browser_test_configured,
            frontend_changes: evaluation.frontend_changes,
            session_id_present: evaluation.session_id_present,
            recent_browser_test: evaluation.recent_browser_test,
            blocking_finding_count,
            should_block: blocking_finding_count > 0,
            decision: PrePushBrowserDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PrePushBrowserGraphRun {
    pub state: PrePushBrowserState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<PrePushBrowserState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct PrePushBrowserAuthorization {
    decision: PrePushBrowserDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl PrePushBrowserAuthorization {
    #[must_use]
    pub fn decision(&self) -> PrePushBrowserDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl PrePushBrowserGraphRun {
    #[must_use]
    pub fn pre_push_browser_authorization(
        &self,
    ) -> Result<Option<PrePushBrowserAuthorization>, String> {
        if self.state.decision == PrePushBrowserDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "pre_push_browser_test",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(PrePushBrowserAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const ALLOW_NO_BROWSER_CONFIG: &str = "allow_no_browser_config";
const ALLOW_NO_FRONTEND_CHANGES: &str = "allow_no_frontend_changes";
const ALLOW_RECENT_BROWSER_TEST: &str = "allow_recent_browser_test";
const BLOCK: &str = "block";

pub type PrePushBrowserGraph = CompilationResult<PrePushBrowserState>;

#[must_use]
pub fn pre_push_browser_decision_label(decision: PrePushBrowserDecision) -> &'static str {
    match decision {
        PrePushBrowserDecision::Unclassified => "unclassified",
        PrePushBrowserDecision::Allow => "allow",
        PrePushBrowserDecision::AllowNoBrowserConfig => "allow-no-browser-config",
        PrePushBrowserDecision::AllowNoFrontendChanges => "allow-no-frontend-changes",
        PrePushBrowserDecision::AllowRecentBrowserTest => "allow-recent-browser-test",
        PrePushBrowserDecision::Block => "block",
    }
}

#[must_use]
pub fn command_sha256(command: &str) -> String {
    hex::encode(Sha256::digest(command.as_bytes()))
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn optional_hex_digest_present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(hex_digest_present)
}

fn expected_should_block(
    bash_tool: bool,
    git_push: bool,
    repo_browser_test_configured: bool,
    frontend_changes: bool,
    recent_browser_test: bool,
) -> bool {
    bash_tool
        && git_push
        && repo_browser_test_configured
        && frontend_changes
        && !recent_browser_test
}

fn expected_decision(state: &PrePushBrowserState) -> PrePushBrowserDecision {
    if !state.bash_tool || !state.git_push {
        PrePushBrowserDecision::Allow
    } else if !state.repo_browser_test_configured {
        PrePushBrowserDecision::AllowNoBrowserConfig
    } else if !state.frontend_changes {
        PrePushBrowserDecision::AllowNoFrontendChanges
    } else if state.recent_browser_test {
        PrePushBrowserDecision::AllowRecentBrowserTest
    } else {
        PrePushBrowserDecision::Block
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "pre_push_browser_test")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn pre_push_browser_state_schema() -> StateSchema<PrePushBrowserState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "bash_tool",
                "command_present",
                "command_sha256",
                "git_push",
                "repo_browser_test_configured",
                "frontend_changes",
                "session_id_present",
                "recent_browser_test",
                "blocking_finding_count",
                "should_block",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "tool": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "bash_tool": { "type": "boolean" },
                "command_present": { "type": "boolean" },
                "command_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "git_push": { "type": "boolean" },
                "repo_browser_test_configured": { "type": "boolean" },
                "frontend_changes": { "type": "boolean" },
                "session_id_present": { "type": "boolean" },
                "recent_browser_test": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "Allow",
                        "AllowNoBrowserConfig",
                        "AllowNoFrontendChanges",
                        "AllowRecentBrowserTest",
                        "Block"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "pre_push_browser_test",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &PrePushBrowserState| {
            let Some(tool) = state
                .tool
                .as_deref()
                .map(str::trim)
                .filter(|tool| !tool.is_empty())
            else {
                return Err(StateError::ValidationFailed(
                    "LangGraph tool-authority state requires concrete tool identity".to_string(),
                ));
            };
            if state.bash_tool && tool != "Bash" {
                return Err(StateError::ValidationFailed(format!(
                    "pre_push_browser_test bash_tool requires Bash, got {tool}"
                )));
            }
            if !state.command_present {
                if state.command_sha256.is_some()
                    || state.git_push
                    || state.repo_browser_test_configured
                    || state.frontend_changes
                    || state.recent_browser_test
                    || state.blocking_finding_count > 0
                    || state.should_block
                {
                    return Err(StateError::ValidationFailed(
                        "pre_push_browser_test missing-command state cannot carry push facts"
                            .to_string(),
                    ));
                }
            } else if !optional_hex_digest_present(&state.command_sha256) {
                return Err(StateError::ValidationFailed(
                    "pre_push_browser_test command_sha256 must be a 64-character hex digest"
                        .to_string(),
                ));
            }
            if !state.bash_tool
                && (state.git_push
                    || state.repo_browser_test_configured
                    || state.frontend_changes
                    || state.recent_browser_test
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "pre_push_browser_test non-Bash state cannot authorize git pushes"
                        .to_string(),
                ));
            }
            if !state.git_push
                && (state.repo_browser_test_configured
                    || state.frontend_changes
                    || state.recent_browser_test
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "pre_push_browser_test non-push state cannot carry browser-test facts"
                        .to_string(),
                ));
            }
            if !state.repo_browser_test_configured
                && (state.frontend_changes
                    || state.recent_browser_test
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "pre_push_browser_test repos without browser config cannot carry browser-test findings"
                        .to_string(),
                ));
            }
            if state.repo_browser_test_configured
                && !state.frontend_changes
                && (state.recent_browser_test
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "pre_push_browser_test backend-only push cannot carry browser evidence or block"
                        .to_string(),
                ));
            }
            if state.recent_browser_test && !state.session_id_present {
                return Err(StateError::ValidationFailed(
                    "pre_push_browser_test recent browser evidence requires a session id"
                        .to_string(),
                ));
            }
            let expected_should_block = expected_should_block(
                state.bash_tool,
                state.git_push,
                state.repo_browser_test_configured,
                state.frontend_changes,
                state.recent_browser_test,
            );
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "pre_push_browser_test should_block must match browser-test policy: expected \
                     {expected_should_block}, got {}",
                    state.should_block
                )));
            }
            let expected_blocking_finding_count = u64::from(expected_should_block);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "pre_push_browser_test blocking_finding_count must match should_block: \
                     expected {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }
            if state.decision != PrePushBrowserDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "pre_push_browser_test terminal decision must match derived authorization: \
                     terminal decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: PrePushBrowserState) -> Result<PrePushBrowserState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_pre_push_browser_graph() -> Result<PrePushBrowserGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("pre_push_browser_test").await?;
    build_pre_push_browser_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_pre_push_browser_graph_with_ephemeral_sqlite() -> Result<PrePushBrowserGraph, String>
{
    build_pre_push_browser_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_pre_push_browser_graph_with_database_path(
    db_path: &str,
) -> Result<PrePushBrowserGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_pre_push_browser_graph_with_checkpointer(checkpointer).await
}

async fn build_pre_push_browser_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<PrePushBrowserGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = pre_push_browser_state_schema();
    let builder = StateGraphBuilder::<PrePushBrowserState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: PrePushBrowserState| async move {
                emit_decision_node_event("pre_push_browser_test", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ALLOW,
            |s: PrePushBrowserState| async move {
                emit_decision_node_event("pre_push_browser_test", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = PrePushBrowserDecision::Allow;
                Ok::<_, NodeError>(next)
            },
            node_config(ALLOW, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ALLOW_NO_BROWSER_CONFIG,
            |s: PrePushBrowserState| async move {
                emit_decision_node_event(
                    "pre_push_browser_test",
                    ALLOW_NO_BROWSER_CONFIG,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = PrePushBrowserDecision::AllowNoBrowserConfig;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_NO_BROWSER_CONFIG,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            ALLOW_NO_FRONTEND_CHANGES,
            |s: PrePushBrowserState| async move {
                emit_decision_node_event(
                    "pre_push_browser_test",
                    ALLOW_NO_FRONTEND_CHANGES,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = PrePushBrowserDecision::AllowNoFrontendChanges;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_NO_FRONTEND_CHANGES,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            ALLOW_RECENT_BROWSER_TEST,
            |s: PrePushBrowserState| async move {
                emit_decision_node_event(
                    "pre_push_browser_test",
                    ALLOW_RECENT_BROWSER_TEST,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = PrePushBrowserDecision::AllowRecentBrowserTest;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_RECENT_BROWSER_TEST,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            BLOCK,
            |s: PrePushBrowserState| async move {
                emit_decision_node_event("pre_push_browser_test", BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = PrePushBrowserDecision::Block;
                Ok::<_, NodeError>(next)
            },
            node_config(BLOCK, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(
            CLASSIFY,
            |s: &PrePushBrowserState| match expected_decision(s) {
                PrePushBrowserDecision::Allow => ALLOW.into(),
                PrePushBrowserDecision::AllowNoBrowserConfig => ALLOW_NO_BROWSER_CONFIG.into(),
                PrePushBrowserDecision::AllowNoFrontendChanges => ALLOW_NO_FRONTEND_CHANGES.into(),
                PrePushBrowserDecision::AllowRecentBrowserTest => ALLOW_RECENT_BROWSER_TEST.into(),
                PrePushBrowserDecision::Block => BLOCK.into(),
                PrePushBrowserDecision::Unclassified => ALLOW.into(),
            },
        )
        .add_edge(ALLOW, END)
        .add_edge(ALLOW_NO_BROWSER_CONFIG, END)
        .add_edge(ALLOW_NO_FRONTEND_CHANGES, END)
        .add_edge(ALLOW_RECENT_BROWSER_TEST, END)
        .add_edge(BLOCK, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_pre_push_browser_decision_report(
    compiled: &PrePushBrowserGraph,
    state: PrePushBrowserState,
) -> Result<PrePushBrowserGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id(
        "pre_push_browser_test",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed = stream_decision_run(
        compiled,
        &thread_id,
        "pre_push_browser_test",
        &identifier,
        state,
    )
    .await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "pre_push_browser_test",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(PrePushBrowserGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: pre_push_browser_graph_topology(compiled)?,
    })
}

pub fn pre_push_browser_graph_topology(
    compiled: &PrePushBrowserGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("pre_push_browser_test", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::pre_push_browser_test::{
        PrePushBrowserDecision as AppDecision, PrePushBrowserEvaluation,
    };

    fn evaluation() -> PrePushBrowserEvaluation {
        PrePushBrowserEvaluation {
            tool: Some("Bash".to_string()),
            command: Some("git push origin main".to_string()),
            bash_tool: true,
            command_present: true,
            git_push: true,
            repo_browser_test_configured: true,
            frontend_changes: true,
            session_id_present: true,
            recent_browser_test: false,
            should_block: true,
            decision: AppDecision::Block,
        }
    }

    fn missing_command_evaluation() -> PrePushBrowserEvaluation {
        PrePushBrowserEvaluation {
            tool: Some("Bash".to_string()),
            command: None,
            bash_tool: true,
            command_present: false,
            git_push: false,
            repo_browser_test_configured: false,
            frontend_changes: false,
            session_id_present: true,
            recent_browser_test: false,
            should_block: false,
            decision: AppDecision::Allow,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_browser_test_block() {
        let graph = build_pre_push_browser_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = PrePushBrowserState::from_evaluation("browser-push-block", &evaluation());
        assert_eq!(state.tool.as_deref(), Some("Bash"));
        assert!(optional_hex_digest_present(&state.command_sha256));
        let run = run_pre_push_browser_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, PrePushBrowserDecision::Block);
        assert!(run
            .pre_push_browser_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_missing_command_with_absent_command_hash() {
        let graph = build_pre_push_browser_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = PrePushBrowserState::from_evaluation(
            "browser-push-missing-command",
            &missing_command_evaluation(),
        );
        assert_eq!(state.command_sha256, None);
        let run = run_pre_push_browser_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, PrePushBrowserDecision::Allow);
    }

    #[tokio::test]
    async fn graph_authorizes_recent_browser_test_allow() {
        let graph = build_pre_push_browser_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation();
        eval.recent_browser_test = true;
        eval.should_block = false;
        eval.decision = AppDecision::AllowRecentBrowserTest;
        let state = PrePushBrowserState::from_evaluation("browser-push-recent", &eval);
        let run = run_pre_push_browser_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(
            run.state.decision,
            PrePushBrowserDecision::AllowRecentBrowserTest
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_recent_test_without_session() {
        let graph = build_pre_push_browser_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation();
        eval.recent_browser_test = true;
        eval.session_id_present = false;
        eval.should_block = false;
        eval.decision = AppDecision::AllowRecentBrowserTest;
        let state = PrePushBrowserState::from_evaluation("browser-push-forged-recent", &eval);
        let err = run_pre_push_browser_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(
            err.contains("recent browser evidence requires a session id"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_allow_without_browser_test() {
        let graph = build_pre_push_browser_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state =
            PrePushBrowserState::from_evaluation("browser-push-forged-allow", &evaluation());
        state.should_block = false;
        state.blocking_finding_count = 0;
        let err = run_pre_push_browser_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_block"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_tool_identity() {
        let graph = build_pre_push_browser_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation();
        eval.tool = None;
        let state = PrePushBrowserState::from_evaluation("browser-push-missing-tool", &eval);
        let err = run_pre_push_browser_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("tool identity"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_command_without_command_digest() {
        let graph = build_pre_push_browser_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state =
            PrePushBrowserState::from_evaluation("browser-push-missing-digest", &evaluation());
        state.command_sha256 = None;
        let err = run_pre_push_browser_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("command_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_command_with_extra_command_digest() {
        let graph = build_pre_push_browser_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = PrePushBrowserState::from_evaluation(
            "browser-push-extra-digest",
            &missing_command_evaluation(),
        );
        state.command_sha256 = Some(command_sha256("git push origin main"));
        let err = run_pre_push_browser_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("missing-command"), "{err}");
    }
}
