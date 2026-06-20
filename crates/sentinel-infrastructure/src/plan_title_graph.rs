//! Graph-backed plan title authorization.
//!
//! The application hook computes deterministic facts for `ExitPlanMode`: whether
//! inspectable plan text is present and whether Sentinel can derive a stable,
//! descriptive title. This graph authorizes the resulting allow/block decision
//! through durable LangGraph checkpoints before the CLI permits plan exit.

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::plan_title_gate::PlanTitleEvaluation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PlanTitleDecision {
    #[default]
    Unclassified,
    Allow,
    BlockMissingPlan,
    BlockTitleless,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTitleState {
    pub identifier: String,
    pub tool: Option<String>,
    pub exit_plan_mode: bool,
    pub plan_text_present: bool,
    pub plan_text_sha256: Option<String>,
    pub derivable_title: bool,
    pub title_line_present: bool,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: PlanTitleDecision,
}

impl PlanTitleState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &PlanTitleEvaluation,
    ) -> Self {
        let plan_text_sha256 = evaluation
            .plan_text
            .as_deref()
            .filter(|_| evaluation.plan_text_present)
            .map(sha256);
        let should_block = expected_should_block(
            evaluation.exit_plan_mode,
            evaluation.plan_text_present,
            evaluation.derivable_title,
        );
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            exit_plan_mode: evaluation.exit_plan_mode,
            plan_text_present: evaluation.plan_text_present,
            plan_text_sha256,
            derivable_title: evaluation.derivable_title,
            title_line_present: evaluation.title_line_present,
            blocking_finding_count: u64::from(should_block),
            should_block,
            decision: PlanTitleDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanTitleGraphRun {
    pub state: PlanTitleState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<PlanTitleState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct PlanTitleAuthorization {
    decision: PlanTitleDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl PlanTitleAuthorization {
    #[must_use]
    pub fn decision(&self) -> PlanTitleDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl PlanTitleGraphRun {
    #[must_use]
    pub fn plan_title_authorization(&self) -> Result<Option<PlanTitleAuthorization>, String> {
        if self.state.decision == PlanTitleDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "plan_title",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(PlanTitleAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const BLOCK_MISSING_PLAN: &str = "block_missing_plan";
const BLOCK_TITLELESS: &str = "block_titleless";

pub type PlanTitleGraph = CompilationResult<PlanTitleState>;

#[must_use]
pub fn plan_title_decision_label(decision: PlanTitleDecision) -> &'static str {
    match decision {
        PlanTitleDecision::Unclassified => "unclassified",
        PlanTitleDecision::Allow => "allow",
        PlanTitleDecision::BlockMissingPlan => "block-missing-plan",
        PlanTitleDecision::BlockTitleless => "block-titleless",
    }
}

#[must_use]
pub fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn expected_should_block(
    exit_plan_mode: bool,
    plan_text_present: bool,
    derivable_title: bool,
) -> bool {
    exit_plan_mode && (!plan_text_present || !derivable_title)
}

fn expected_decision(state: &PlanTitleState) -> PlanTitleDecision {
    if !state.exit_plan_mode {
        PlanTitleDecision::Allow
    } else if !state.plan_text_present {
        PlanTitleDecision::BlockMissingPlan
    } else if !state.derivable_title {
        PlanTitleDecision::BlockTitleless
    } else {
        PlanTitleDecision::Allow
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "plan_title")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn optional_hex_digest_present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(hex_digest_present)
}

fn plan_title_state_schema() -> StateSchema<PlanTitleState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "exit_plan_mode",
                "plan_text_present",
                "plan_text_sha256",
                "derivable_title",
                "title_line_present",
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
                "exit_plan_mode": { "type": "boolean" },
                "plan_text_present": { "type": "boolean" },
                "plan_text_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "derivable_title": { "type": "boolean" },
                "title_line_present": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "Allow",
                        "BlockMissingPlan",
                        "BlockTitleless"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "plan_title",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &PlanTitleState| {
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
            if state.exit_plan_mode {
                if tool != "ExitPlanMode" {
                    return Err(StateError::ValidationFailed(format!(
                        "plan_title exit_plan_mode requires ExitPlanMode tool, got {tool}"
                    )));
                }
            } else if state.plan_text_present
                || state.plan_text_sha256.is_some()
                || state.derivable_title
                || state.title_line_present
                || state.blocking_finding_count > 0
                || state.should_block
            {
                return Err(StateError::ValidationFailed(
                    "plan_title non-ExitPlanMode state cannot carry plan-title facts".to_string(),
                ));
            }

            if state.plan_text_present {
                if !optional_hex_digest_present(&state.plan_text_sha256) {
                    return Err(StateError::ValidationFailed(
                        "plan_title plan_text_sha256 must be a 64-character hex digest".to_string(),
                    ));
                }
            } else if state.plan_text_sha256.is_some()
                || state.derivable_title
                || state.title_line_present
            {
                return Err(StateError::ValidationFailed(
                    "plan_title missing plan text cannot carry title facts".to_string(),
                ));
            }

            if state.derivable_title != state.title_line_present {
                return Err(StateError::ValidationFailed(format!(
                    "plan_title derivable_title must match title_line_present: expected {}, got {}",
                    state.title_line_present, state.derivable_title
                )));
            }

            let expected_should_block = expected_should_block(
                state.exit_plan_mode,
                state.plan_text_present,
                state.derivable_title,
            );
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "plan_title should_block must match plan title policy: expected \
                     {expected_should_block}, got {}",
                    state.should_block
                )));
            }

            let expected_blocking_finding_count = u64::from(expected_should_block);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "plan_title blocking_finding_count must match should_block: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }

            if state.decision != PlanTitleDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "plan_title terminal decision must match derived authorization: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }

            Ok(())
        })
}

async fn classify_node(state: PlanTitleState) -> Result<PlanTitleState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_plan_title_graph() -> Result<PlanTitleGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("plan_title").await?;
    build_plan_title_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_plan_title_graph_with_ephemeral_sqlite() -> Result<PlanTitleGraph, String> {
    build_plan_title_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_plan_title_graph_with_database_path(
    db_path: &str,
) -> Result<PlanTitleGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_plan_title_graph_with_checkpointer(checkpointer).await
}

async fn build_plan_title_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<PlanTitleGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = plan_title_state_schema();
    let builder = StateGraphBuilder::<PlanTitleState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_node_defaults(crate::decision_graph_introspection::decision_node_defaults())
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: PlanTitleState| async move {
                emit_decision_node_event("plan_title", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(
                CLASSIFY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            ALLOW,
            |s: PlanTitleState| async move {
                emit_decision_node_event("plan_title", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = PlanTitleDecision::Allow;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            BLOCK_MISSING_PLAN,
            |s: PlanTitleState| async move {
                emit_decision_node_event("plan_title", BLOCK_MISSING_PLAN, &s.identifier)?;
                let mut next = s;
                next.decision = PlanTitleDecision::BlockMissingPlan;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK_MISSING_PLAN,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            BLOCK_TITLELESS,
            |s: PlanTitleState| async move {
                emit_decision_node_event("plan_title", BLOCK_TITLELESS, &s.identifier)?;
                let mut next = s;
                next.decision = PlanTitleDecision::BlockTitleless;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK_TITLELESS,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &PlanTitleState| match expected_decision(s) {
            PlanTitleDecision::Allow => ALLOW.into(),
            PlanTitleDecision::BlockMissingPlan => BLOCK_MISSING_PLAN.into(),
            PlanTitleDecision::BlockTitleless => BLOCK_TITLELESS.into(),
            PlanTitleDecision::Unclassified => ALLOW.into(),
        })
        .add_edge(ALLOW, END)
        .add_edge(BLOCK_MISSING_PLAN, END)
        .add_edge(BLOCK_TITLELESS, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_plan_title_decision_report(
    compiled: &PlanTitleGraph,
    state: PlanTitleState,
) -> Result<PlanTitleGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "plan_title",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "plan_title", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "plan_title",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(PlanTitleGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: plan_title_graph_topology(compiled)?,
    })
}

pub fn plan_title_graph_topology(
    compiled: &PlanTitleGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("plan_title", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::plan_title_gate::{
        PlanTitleDecision as AppDecision, PlanTitleEvaluation,
    };

    fn evaluation(plan_text: Option<&str>, decision: AppDecision) -> PlanTitleEvaluation {
        let plan_text = plan_text.map(str::to_string);
        let plan_text_present = plan_text.is_some();
        let derivable_title = matches!(decision, AppDecision::Allow);
        PlanTitleEvaluation {
            tool: Some("ExitPlanMode".to_string()),
            exit_plan_mode: true,
            plan_text,
            plan_text_present,
            derivable_title,
            title_line_present: derivable_title,
            should_block: !matches!(decision, AppDecision::Allow),
            decision,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_titled_plan_allow() {
        let graph = build_plan_title_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = PlanTitleState::from_evaluation(
            "plan-title-allow",
            &evaluation(Some("## Plan: Add auth\nbody"), AppDecision::Allow),
        );
        assert_eq!(state.tool.as_deref(), Some("ExitPlanMode"));
        assert!(optional_hex_digest_present(&state.plan_text_sha256));
        let run = run_plan_title_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, PlanTitleDecision::Allow);
        assert!(run
            .plan_title_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_missing_plan_block() {
        let graph = build_plan_title_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = PlanTitleState::from_evaluation(
            "plan-title-missing",
            &evaluation(None, AppDecision::BlockMissingPlan),
        );
        assert_eq!(state.plan_text_sha256, None);
        let run = run_plan_title_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, PlanTitleDecision::BlockMissingPlan);
    }

    #[tokio::test]
    async fn graph_authorizes_titleless_plan_block() {
        let graph = build_plan_title_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = PlanTitleState::from_evaluation(
            "plan-title-titleless",
            &evaluation(Some("   \n\n"), AppDecision::BlockTitleless),
        );
        let run = run_plan_title_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, PlanTitleDecision::BlockTitleless);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_allow_for_titleless_plan() {
        let graph = build_plan_title_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = PlanTitleState::from_evaluation(
            "plan-title-forged",
            &evaluation(Some("   \n\n"), AppDecision::BlockTitleless),
        );
        state.should_block = false;
        state.blocking_finding_count = 0;
        let err = run_plan_title_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_block"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_title_without_plan_text() {
        let graph = build_plan_title_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = PlanTitleState::from_evaluation(
            "plan-title-contradictory",
            &evaluation(None, AppDecision::BlockMissingPlan),
        );
        state.derivable_title = true;
        state.title_line_present = true;
        let err = run_plan_title_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("missing plan text"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_tool_identity() {
        let graph = build_plan_title_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut evaluation = evaluation(Some("## Plan: Add auth"), AppDecision::Allow);
        evaluation.tool = None;
        let state = PlanTitleState::from_evaluation("plan-title-missing-tool", &evaluation);
        let err = run_plan_title_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("tool identity"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_plan_without_plan_digest() {
        let graph = build_plan_title_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = PlanTitleState::from_evaluation(
            "plan-title-missing-digest",
            &evaluation(Some("## Plan: Add auth"), AppDecision::Allow),
        );
        state.plan_text_sha256 = None;
        let err = run_plan_title_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("plan_text_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_plan_with_extra_plan_digest() {
        let graph = build_plan_title_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = PlanTitleState::from_evaluation(
            "plan-title-extra-digest",
            &evaluation(None, AppDecision::BlockMissingPlan),
        );
        state.plan_text_sha256 = Some(sha256("## Plan: Add auth"));
        let err = run_plan_title_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("missing plan text"), "{err}");
    }
}
