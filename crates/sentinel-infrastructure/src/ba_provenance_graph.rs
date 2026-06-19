//! Graph-backed BA1 citation provenance authorization.
//!
//! The BA1 hook validates cited artifacts against Sentinel's connector
//! provenance store. This graph authorizes the evaluated allow/block decision
//! through durable LangGraph checkpoints so BA publish gates cannot complete
//! from an uncheckpointed provenance branch.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::hooks::provenance_validate::ValidationMode;
use sentinel_domain::ba::{ProvenanceCheck, ProvenanceFinding};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BaProvenanceDecision {
    #[default]
    Unclassified,
    Allow,
    ObserveOnlyWouldBlock,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaProvenanceState {
    pub identifier: String,
    pub mode: String,
    pub citation_count: u64,
    pub finding_count: u64,
    pub always_block_finding_count: u64,
    pub strict_only_finding_count: u64,
    pub provenance_class_finding_count: u64,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: BaProvenanceDecision,
}

impl BaProvenanceState {
    #[must_use]
    pub fn from_checks(
        identifier: impl Into<String>,
        checks: &[ProvenanceCheck],
        mode: ValidationMode,
    ) -> Self {
        let mut finding_count = 0u64;
        let mut always_block_finding_count = 0u64;
        let mut strict_only_finding_count = 0u64;
        let mut provenance_class_finding_count = 0u64;
        for finding in checks.iter().flat_map(|check| &check.findings) {
            finding_count += 1;
            match finding {
                ProvenanceFinding::Existence { .. }
                | ProvenanceFinding::StoreUnavailable { .. }
                | ProvenanceFinding::StoreMalformed { .. } => {
                    always_block_finding_count += 1;
                }
                ProvenanceFinding::Freshness { .. } | ProvenanceFinding::WithinSession { .. } => {
                    strict_only_finding_count += 1;
                }
                ProvenanceFinding::ProvenanceClass { .. } => {
                    provenance_class_finding_count += 1;
                }
            }
        }
        let blocking_finding_count = expected_blocking_finding_count(
            validation_mode_label(mode),
            always_block_finding_count,
            strict_only_finding_count,
        );
        Self {
            identifier: identifier.into(),
            mode: validation_mode_label(mode).to_string(),
            citation_count: checks.len() as u64,
            finding_count,
            always_block_finding_count,
            strict_only_finding_count,
            provenance_class_finding_count,
            blocking_finding_count,
            should_block: expected_should_block(
                validation_mode_label(mode),
                blocking_finding_count,
            ),
            decision: BaProvenanceDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BaProvenanceRun {
    pub state: BaProvenanceState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<BaProvenanceState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct BaProvenanceAuthorization {
    decision: BaProvenanceDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl BaProvenanceAuthorization {
    #[must_use]
    pub fn decision(&self) -> BaProvenanceDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl BaProvenanceRun {
    #[must_use]
    pub fn ba_provenance_authorization(&self) -> Result<Option<BaProvenanceAuthorization>, String> {
        if self.state.decision == BaProvenanceDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "ba_provenance",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(BaProvenanceAuthorization {
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

pub type BaProvenanceGraph = CompilationResult<BaProvenanceState>;

#[must_use]
pub const fn validation_mode_label(mode: ValidationMode) -> &'static str {
    match mode {
        ValidationMode::ObserveOnly => "observe_only",
        ValidationMode::DefaultBlocking => "default_blocking",
        ValidationMode::StrictBlocking => "strict_blocking",
    }
}

#[must_use]
pub fn ba_provenance_decision_label(decision: BaProvenanceDecision) -> &'static str {
    match decision {
        BaProvenanceDecision::Unclassified => "unclassified",
        BaProvenanceDecision::Allow => "allow",
        BaProvenanceDecision::ObserveOnlyWouldBlock => "observe-only-would-block",
        BaProvenanceDecision::Block => "block",
    }
}

fn expected_blocking_finding_count(
    mode: &str,
    always_block_finding_count: u64,
    strict_only_finding_count: u64,
) -> u64 {
    match mode {
        "strict_blocking" => always_block_finding_count + strict_only_finding_count,
        _ => always_block_finding_count,
    }
}

fn expected_should_block(mode: &str, blocking_finding_count: u64) -> bool {
    mode != "observe_only" && blocking_finding_count > 0
}

fn expected_decision(state: &BaProvenanceState) -> BaProvenanceDecision {
    if state.should_block {
        BaProvenanceDecision::Block
    } else if state.mode == "observe_only" && state.blocking_finding_count > 0 {
        BaProvenanceDecision::ObserveOnlyWouldBlock
    } else {
        BaProvenanceDecision::Allow
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "ba_provenance")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn ba_provenance_state_schema() -> StateSchema<BaProvenanceState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "mode",
                "citation_count",
                "finding_count",
                "always_block_finding_count",
                "strict_only_finding_count",
                "provenance_class_finding_count",
                "blocking_finding_count",
                "should_block",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "mode": {
                    "type": "string",
                    "enum": ["observe_only", "default_blocking", "strict_blocking"]
                },
                "citation_count": { "type": "integer", "minimum": 0 },
                "finding_count": { "type": "integer", "minimum": 0 },
                "always_block_finding_count": { "type": "integer", "minimum": 0 },
                "strict_only_finding_count": { "type": "integer", "minimum": 0 },
                "provenance_class_finding_count": { "type": "integer", "minimum": 0 },
                "blocking_finding_count": { "type": "integer", "minimum": 0 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Allow", "ObserveOnlyWouldBlock", "Block"]
                }
            },
            "x-sentinel": {
                "graph": "ba_provenance",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &BaProvenanceState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "ba_provenance identifier must not be empty".to_string(),
                ));
            }
            if !matches!(
                state.mode.as_str(),
                "observe_only" | "default_blocking" | "strict_blocking"
            ) {
                return Err(StateError::ValidationFailed(
                    "ba_provenance mode must be known".to_string(),
                ));
            }
            let categorized = state.always_block_finding_count
                + state.strict_only_finding_count
                + state.provenance_class_finding_count;
            if categorized > state.finding_count {
                return Err(StateError::ValidationFailed(
                    "ba_provenance categorized findings cannot exceed finding_count".to_string(),
                ));
            }
            let expected_blocking = expected_blocking_finding_count(
                &state.mode,
                state.always_block_finding_count,
                state.strict_only_finding_count,
            );
            if state.blocking_finding_count != expected_blocking {
                return Err(StateError::ValidationFailed(
                    "ba_provenance blocking_finding_count must match mode and finding counts"
                        .to_string(),
                ));
            }
            let expected_block = expected_should_block(&state.mode, state.blocking_finding_count);
            if state.should_block != expected_block {
                return Err(StateError::ValidationFailed(
                    "ba_provenance should_block must match mode and blocking findings".to_string(),
                ));
            }
            if state.decision != BaProvenanceDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "ba_provenance terminal decision must match mode and finding counts"
                        .to_string(),
                ));
            }
            Ok(())
        })
}

pub async fn build_ba_provenance_graph() -> Result<BaProvenanceGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("ba_provenance").await?;
    build_ba_provenance_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_ba_provenance_graph_with_ephemeral_sqlite() -> Result<BaProvenanceGraph, String> {
    build_ba_provenance_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_ba_provenance_graph_with_database_path(
    database_path: &str,
) -> Result<BaProvenanceGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_ba_provenance_graph_with_checkpointer(checkpointer).await
}

async fn build_ba_provenance_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<BaProvenanceGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = ba_provenance_state_schema();
    let builder = StateGraphBuilder::<BaProvenanceState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: BaProvenanceState| async move {
                emit_decision_node_event("ba_provenance", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ALLOW,
            |s: BaProvenanceState| async move {
                emit_decision_node_event("ba_provenance", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = BaProvenanceDecision::Allow;
                Ok::<_, NodeError>(next)
            },
            node_config(ALLOW, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            OBSERVE_ONLY_WOULD_BLOCK,
            |s: BaProvenanceState| async move {
                emit_decision_node_event("ba_provenance", OBSERVE_ONLY_WOULD_BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = BaProvenanceDecision::ObserveOnlyWouldBlock;
                Ok::<_, NodeError>(next)
            },
            node_config(
                OBSERVE_ONLY_WOULD_BLOCK,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            BLOCK,
            |s: BaProvenanceState| async move {
                emit_decision_node_event("ba_provenance", BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = BaProvenanceDecision::Block;
                Ok::<_, NodeError>(next)
            },
            node_config(BLOCK, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &BaProvenanceState| {
            match expected_decision(s) {
                BaProvenanceDecision::Allow => ALLOW.into(),
                BaProvenanceDecision::ObserveOnlyWouldBlock => OBSERVE_ONLY_WOULD_BLOCK.into(),
                BaProvenanceDecision::Block => BLOCK.into(),
                BaProvenanceDecision::Unclassified => ALLOW.into(),
            }
        })
        .add_edge(ALLOW, END)
        .add_edge(OBSERVE_ONLY_WOULD_BLOCK, END)
        .add_edge(BLOCK, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_ba_provenance_decision_report(
    compiled: &BaProvenanceGraph,
    state: BaProvenanceState,
) -> Result<BaProvenanceRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "ba_provenance",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "ba_provenance", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "ba_provenance",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(BaProvenanceRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: ba_provenance_graph_topology(compiled)?,
    })
}

pub fn ba_provenance_graph_topology(
    compiled: &BaProvenanceGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("ba_provenance", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sentinel_domain::ba::{ArtifactReference, ProvenanceClass};

    fn citation() -> ArtifactReference {
        ArtifactReference {
            artifact_id: "artifact-1".into(),
            content_hash: "hash-v1".into(),
            provenance_class: ProvenanceClass::SystemOfRecord,
            retrieved_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn graph_authorizes_default_blocking_existence_failure() {
        let check =
            ProvenanceCheck::passing(citation()).with_finding(ProvenanceFinding::Existence {
                artifact_id: "artifact-1".into(),
            });
        let graph = build_ba_provenance_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let run = run_ba_provenance_decision_report(
            &graph,
            BaProvenanceState::from_checks("ba1-case", &[check], ValidationMode::DefaultBlocking),
        )
        .await
        .unwrap();
        assert_eq!(run.state.decision, BaProvenanceDecision::Block);
        assert!(run.state.should_block);
        assert!(run.ba_provenance_authorization().unwrap().is_some());
        assert_eq!(run.topology.graph, "ba_provenance");
        assert!(run.topology.durable_checkpointer);
    }

    #[tokio::test]
    async fn graph_authorizes_observe_only_would_block() {
        let check =
            ProvenanceCheck::passing(citation()).with_finding(ProvenanceFinding::Existence {
                artifact_id: "artifact-1".into(),
            });
        let graph = build_ba_provenance_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let run = run_ba_provenance_decision_report(
            &graph,
            BaProvenanceState::from_checks("ba1-case", &[check], ValidationMode::ObserveOnly),
        )
        .await
        .unwrap();
        assert_eq!(
            run.state.decision,
            BaProvenanceDecision::ObserveOnlyWouldBlock
        );
        assert!(!run.state.should_block);
    }

    #[tokio::test]
    async fn graph_warns_freshness_in_default_but_blocks_in_strict() {
        let check =
            ProvenanceCheck::passing(citation()).with_finding(ProvenanceFinding::Freshness {
                artifact_id: "artifact-1".into(),
                cited_hash: "old".into(),
                actual_hash: "new".into(),
                is_blocking: false,
            });
        let graph = build_ba_provenance_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let default_run = run_ba_provenance_decision_report(
            &graph,
            BaProvenanceState::from_checks(
                "ba1-default",
                &[check.clone()],
                ValidationMode::DefaultBlocking,
            ),
        )
        .await
        .unwrap();
        assert_eq!(default_run.state.decision, BaProvenanceDecision::Allow);
        assert!(!default_run.state.should_block);

        let strict_run = run_ba_provenance_decision_report(
            &graph,
            BaProvenanceState::from_checks("ba1-strict", &[check], ValidationMode::StrictBlocking),
        )
        .await
        .unwrap();
        assert_eq!(strict_run.state.decision, BaProvenanceDecision::Block);
        assert!(strict_run.state.should_block);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_strict_allow() {
        let graph = build_ba_provenance_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let err = run_ba_provenance_decision_report(
            &graph,
            BaProvenanceState {
                identifier: "ba1-forged".into(),
                mode: "strict_blocking".into(),
                citation_count: 1,
                finding_count: 1,
                always_block_finding_count: 0,
                strict_only_finding_count: 1,
                provenance_class_finding_count: 0,
                blocking_finding_count: 0,
                should_block: false,
                decision: BaProvenanceDecision::Unclassified,
            },
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("blocking_finding_count"),
            "unexpected validation error: {err}"
        );
    }
}
