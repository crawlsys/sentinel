//! Graph-backed BA3 requirements traceability authorization.
//!
//! The BA3 hook computes structural findings from a BA output's
//! `requirement_refs`; this graph authorizes the hook decision through durable
//! LangGraph checkpoints so production BA publish gates do not rely on an
//! uncheckpointed in-process branch.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::hooks::requirements_traceability_gate::ValidationMode;
use sentinel_domain::ba::{RequirementCheck, RequirementFinding};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BaRequirementsDecision {
    #[default]
    Unclassified,
    Allow,
    ObserveOnlyWouldBlock,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaRequirementsState {
    pub identifier: String,
    pub mode: String,
    pub reference_count: u64,
    pub finding_count: u64,
    pub default_block_finding_count: u64,
    pub hash_finding_count: u64,
    pub should_block: bool,
    pub decision: BaRequirementsDecision,
}

impl BaRequirementsState {
    #[must_use]
    pub fn from_check(
        identifier: impl Into<String>,
        check: &RequirementCheck,
        mode: ValidationMode,
    ) -> Self {
        let default_block_finding_count = check
            .findings
            .iter()
            .filter(|finding| finding.is_block())
            .count() as u64;
        let hash_finding_count = check
            .findings
            .iter()
            .filter(|finding| matches!(finding, RequirementFinding::Hash { .. }))
            .count() as u64;
        let should_block = expected_should_block(
            validation_mode_label(mode),
            default_block_finding_count,
            hash_finding_count,
        );
        Self {
            identifier: identifier.into(),
            mode: validation_mode_label(mode).to_string(),
            reference_count: check.references.len() as u64,
            finding_count: check.findings.len() as u64,
            default_block_finding_count,
            hash_finding_count,
            should_block,
            decision: BaRequirementsDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BaRequirementsRun {
    pub state: BaRequirementsState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<BaRequirementsState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct BaRequirementsAuthorization {
    decision: BaRequirementsDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl BaRequirementsAuthorization {
    #[must_use]
    pub fn decision(&self) -> BaRequirementsDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl BaRequirementsRun {
    #[must_use]
    pub fn ba_requirements_authorization(
        &self,
    ) -> Result<Option<BaRequirementsAuthorization>, String> {
        if self.state.decision == BaRequirementsDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "ba_requirements",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(BaRequirementsAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const OBSERVE_ONLY_WOULD_BLOCK: &str = "observe_only_would_block";
const BLOCK: &str = "block";

pub type BaRequirementsGraph = CompilationResult<BaRequirementsState>;

#[must_use]
pub const fn validation_mode_label(mode: ValidationMode) -> &'static str {
    match mode {
        ValidationMode::ObserveOnly => "observe_only",
        ValidationMode::DefaultBlocking => "default_blocking",
        ValidationMode::StrictBlocking => "strict_blocking",
    }
}

#[must_use]
pub fn ba_requirements_decision_label(decision: BaRequirementsDecision) -> &'static str {
    match decision {
        BaRequirementsDecision::Unclassified => "unclassified",
        BaRequirementsDecision::Allow => "allow",
        BaRequirementsDecision::ObserveOnlyWouldBlock => "observe-only-would-block",
        BaRequirementsDecision::Block => "block",
    }
}

fn expected_should_block(
    mode: &str,
    default_block_finding_count: u64,
    hash_finding_count: u64,
) -> bool {
    match mode {
        "observe_only" => false,
        "default_blocking" => default_block_finding_count > 0,
        "strict_blocking" => default_block_finding_count > 0 || hash_finding_count > 0,
        _ => false,
    }
}

fn expected_decision(state: &BaRequirementsState) -> BaRequirementsDecision {
    if state.should_block {
        BaRequirementsDecision::Block
    } else if state.mode == "observe_only" && state.default_block_finding_count > 0 {
        BaRequirementsDecision::ObserveOnlyWouldBlock
    } else {
        BaRequirementsDecision::Allow
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "ba_requirements")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn ba_requirements_state_schema() -> StateSchema<BaRequirementsState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "mode",
                "reference_count",
                "finding_count",
                "default_block_finding_count",
                "hash_finding_count",
                "should_block",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "mode": {
                    "type": "string",
                    "enum": ["observe_only", "default_blocking", "strict_blocking"]
                },
                "reference_count": { "type": "integer", "minimum": 0 },
                "finding_count": { "type": "integer", "minimum": 0 },
                "default_block_finding_count": { "type": "integer", "minimum": 0 },
                "hash_finding_count": { "type": "integer", "minimum": 0 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Allow", "ObserveOnlyWouldBlock", "Block"]
                }
            },
            "x-sentinel": {
                "graph": "ba_requirements",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &BaRequirementsState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "ba_requirements identifier must not be empty".to_string(),
                ));
            }
            if !matches!(
                state.mode.as_str(),
                "observe_only" | "default_blocking" | "strict_blocking"
            ) {
                return Err(StateError::ValidationFailed(
                    "ba_requirements mode must be known".to_string(),
                ));
            }
            if state.default_block_finding_count > state.finding_count {
                return Err(StateError::ValidationFailed(
                    "ba_requirements default_block_finding_count cannot exceed finding_count"
                        .to_string(),
                ));
            }
            if state.hash_finding_count > state.finding_count {
                return Err(StateError::ValidationFailed(
                    "ba_requirements hash_finding_count cannot exceed finding_count".to_string(),
                ));
            }
            let expected_block = expected_should_block(
                &state.mode,
                state.default_block_finding_count,
                state.hash_finding_count,
            );
            if state.should_block != expected_block {
                return Err(StateError::ValidationFailed(
                    "ba_requirements should_block must match mode and finding counts".to_string(),
                ));
            }
            if state.decision != BaRequirementsDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "ba_requirements terminal decision must match mode and finding counts"
                        .to_string(),
                ));
            }
            Ok(())
        })
}

pub async fn build_ba_requirements_graph() -> Result<BaRequirementsGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("ba_requirements").await?;
    build_ba_requirements_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_ba_requirements_graph_with_ephemeral_sqlite() -> Result<BaRequirementsGraph, String>
{
    build_ba_requirements_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_ba_requirements_graph_with_database_path(
    database_path: &str,
) -> Result<BaRequirementsGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_ba_requirements_graph_with_checkpointer(checkpointer).await
}

async fn build_ba_requirements_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<BaRequirementsGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = ba_requirements_state_schema();
    let builder = StateGraphBuilder::<BaRequirementsState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: BaRequirementsState| async move {
                emit_decision_node_event("ba_requirements", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
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
            |s: BaRequirementsState| async move {
                emit_decision_node_event("ba_requirements", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = BaRequirementsDecision::Allow;
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
            OBSERVE_ONLY_WOULD_BLOCK,
            |s: BaRequirementsState| async move {
                emit_decision_node_event(
                    "ba_requirements",
                    OBSERVE_ONLY_WOULD_BLOCK,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = BaRequirementsDecision::ObserveOnlyWouldBlock;
                Ok::<_, NodeError>(next)
            },
            node_config(
                OBSERVE_ONLY_WOULD_BLOCK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            BLOCK,
            |s: BaRequirementsState| async move {
                emit_decision_node_event("ba_requirements", BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = BaRequirementsDecision::Block;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(
            CLASSIFY,
            |s: &BaRequirementsState| match expected_decision(s) {
                BaRequirementsDecision::Allow => ALLOW.into(),
                BaRequirementsDecision::ObserveOnlyWouldBlock => OBSERVE_ONLY_WOULD_BLOCK.into(),
                BaRequirementsDecision::Block => BLOCK.into(),
                BaRequirementsDecision::Unclassified => ALLOW.into(),
            },
        )
        .add_edge(ALLOW, END)
        .add_edge(OBSERVE_ONLY_WOULD_BLOCK, END)
        .add_edge(BLOCK, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_ba_requirements_decision_report(
    compiled: &BaRequirementsGraph,
    state: BaRequirementsState,
) -> Result<BaRequirementsRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "ba_requirements",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "ba_requirements", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "ba_requirements",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(BaRequirementsRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: ba_requirements_graph_topology(compiled)?,
    })
}

pub fn ba_requirements_graph_topology(
    compiled: &BaRequirementsGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("ba_requirements", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::ba::RequirementRef;

    fn req() -> RequirementRef {
        RequirementRef {
            orchestration_id: "case-1".into(),
            matrix_row_id: "R-1".into(),
            content_hash: "hash".into(),
            statement: "Need a justified recommendation".into(),
        }
    }

    #[tokio::test]
    async fn graph_authorizes_default_blocking_coverage_failure() {
        let check =
            RequirementCheck::passing(Vec::new()).with_finding(RequirementFinding::Coverage {
                recommendation_summary: "publish exec deck".into(),
            });
        let graph = build_ba_requirements_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let run = run_ba_requirements_decision_report(
            &graph,
            BaRequirementsState::from_check("ba3-case", &check, ValidationMode::DefaultBlocking),
        )
        .await
        .unwrap();
        assert_eq!(run.state.decision, BaRequirementsDecision::Block);
        assert_eq!(run.state.should_block, true);
        assert!(run.ba_requirements_authorization().unwrap().is_some());
        assert_eq!(run.topology.graph, "ba_requirements");
        assert!(run.topology.durable_checkpointer);
    }

    #[tokio::test]
    async fn graph_authorizes_observe_only_would_block() {
        let check =
            RequirementCheck::passing(Vec::new()).with_finding(RequirementFinding::Coverage {
                recommendation_summary: "publish exec deck".into(),
            });
        let graph = build_ba_requirements_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let run = run_ba_requirements_decision_report(
            &graph,
            BaRequirementsState::from_check("ba3-case", &check, ValidationMode::ObserveOnly),
        )
        .await
        .unwrap();
        assert_eq!(
            run.state.decision,
            BaRequirementsDecision::ObserveOnlyWouldBlock
        );
        assert_eq!(run.state.should_block, false);
    }

    #[tokio::test]
    async fn graph_warns_hash_in_default_but_blocks_in_strict() {
        let check = RequirementCheck::passing(vec![req()]).with_finding(RequirementFinding::Hash {
            orchestration_id: "case-1".into(),
            matrix_row_id: "R-1".into(),
            cited_hash: "old".into(),
            actual_hash: "new".into(),
        });
        let graph = build_ba_requirements_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let default_run = run_ba_requirements_decision_report(
            &graph,
            BaRequirementsState::from_check("ba3-default", &check, ValidationMode::DefaultBlocking),
        )
        .await
        .unwrap();
        assert_eq!(default_run.state.decision, BaRequirementsDecision::Allow);
        assert_eq!(default_run.state.should_block, false);

        let strict_run = run_ba_requirements_decision_report(
            &graph,
            BaRequirementsState::from_check("ba3-strict", &check, ValidationMode::StrictBlocking),
        )
        .await
        .unwrap();
        assert_eq!(strict_run.state.decision, BaRequirementsDecision::Block);
        assert_eq!(strict_run.state.should_block, true);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_should_block() {
        let graph = build_ba_requirements_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let err = run_ba_requirements_decision_report(
            &graph,
            BaRequirementsState {
                identifier: "ba3-forged".into(),
                mode: "default_blocking".into(),
                reference_count: 0,
                finding_count: 1,
                default_block_finding_count: 1,
                hash_finding_count: 0,
                should_block: false,
                decision: BaRequirementsDecision::Unclassified,
            },
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("should_block"),
            "unexpected validation error: {err}"
        );
    }
}
