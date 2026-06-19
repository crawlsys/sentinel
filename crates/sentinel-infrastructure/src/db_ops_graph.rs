//! Graph-backed database operation authorization.
//!
//! The application hook computes deterministic facts for Bash database
//! commands: command presence, migration/destructive classification, production
//! targeting, and the derived allow/block result. This graph authorizes those
//! facts through durable LangGraph checkpoints so production database decisions
//! cannot proceed from an uncheckpointed branch.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::db_ops_gate::DbOpsEvaluation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DbOpsDecision {
    #[default]
    Unclassified,
    Allow,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbOpsState {
    pub identifier: String,
    pub tool: Option<String>,
    pub bash_tool: bool,
    pub command_present: bool,
    pub command_sha256: Option<String>,
    pub migration: bool,
    pub destructive: bool,
    pub database_operation: bool,
    pub production_target: bool,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: DbOpsDecision,
}

impl DbOpsState {
    #[must_use]
    pub fn from_evaluation(identifier: impl Into<String>, evaluation: &DbOpsEvaluation) -> Self {
        let command_sha256 = evaluation
            .command
            .as_deref()
            .filter(|_| evaluation.command_present)
            .map(command_sha256);
        let blocking_finding_count = u64::from(expected_should_block(
            evaluation.bash_tool,
            evaluation.database_operation,
            evaluation.production_target,
        ));
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            bash_tool: evaluation.bash_tool,
            command_present: evaluation.command_present,
            command_sha256,
            migration: evaluation.migration,
            destructive: evaluation.destructive,
            database_operation: evaluation.database_operation,
            production_target: evaluation.production_target,
            blocking_finding_count,
            should_block: blocking_finding_count > 0,
            decision: DbOpsDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DbOpsGraphRun {
    pub state: DbOpsState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<DbOpsState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct DbOpsAuthorization {
    decision: DbOpsDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl DbOpsAuthorization {
    #[must_use]
    pub fn decision(&self) -> DbOpsDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl DbOpsGraphRun {
    #[must_use]
    pub fn db_ops_authorization(&self) -> Result<Option<DbOpsAuthorization>, String> {
        if self.state.decision == DbOpsDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "db_ops",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(DbOpsAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const BLOCK: &str = "block";

pub type DbOpsGraph = CompilationResult<DbOpsState>;

#[must_use]
pub fn db_ops_decision_label(decision: DbOpsDecision) -> &'static str {
    match decision {
        DbOpsDecision::Unclassified => "unclassified",
        DbOpsDecision::Allow => "allow",
        DbOpsDecision::Block => "block",
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
    database_operation: bool,
    production_target: bool,
) -> bool {
    bash_tool && database_operation && production_target
}

fn expected_decision(state: &DbOpsState) -> DbOpsDecision {
    if state.should_block {
        DbOpsDecision::Block
    } else {
        DbOpsDecision::Allow
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "db_ops")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn db_ops_state_schema() -> StateSchema<DbOpsState> {
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
                "migration",
                "destructive",
                "database_operation",
                "production_target",
                "blocking_finding_count",
                "should_block",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "tool": {
                    "anyOf": [
                        { "type": "string", "minLength": 1 },
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
                "migration": { "type": "boolean" },
                "destructive": { "type": "boolean" },
                "database_operation": { "type": "boolean" },
                "production_target": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Allow", "Block"]
                }
            },
            "x-sentinel": {
                "graph": "db_ops",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &DbOpsState| {
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
                    "db_ops bash_tool requires Bash, got {tool}"
                )));
            }
            if !state.command_present {
                if state.command_sha256.is_some()
                    || state.migration
                    || state.destructive
                    || state.database_operation
                    || state.production_target
                    || state.blocking_finding_count > 0
                    || state.should_block
                {
                    return Err(StateError::ValidationFailed(
                        "db_ops missing-command state cannot carry database authorization facts"
                            .to_string(),
                    ));
                }
            } else if !optional_hex_digest_present(&state.command_sha256) {
                return Err(StateError::ValidationFailed(
                    "db_ops command_sha256 must be a 64-character hex digest".to_string(),
                ));
            }
            if !state.bash_tool
                && (state.migration
                    || state.destructive
                    || state.database_operation
                    || state.production_target
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "db_ops non-Bash state cannot authorize database operations".to_string(),
                ));
            }
            let expected_database_operation = state.migration || state.destructive;
            if state.database_operation != expected_database_operation {
                return Err(StateError::ValidationFailed(format!(
                    "db_ops database_operation must match migration/destructive facts: expected \
                     {expected_database_operation}, got {}",
                    state.database_operation
                )));
            }
            if state.production_target && !state.database_operation {
                return Err(StateError::ValidationFailed(
                    "db_ops production_target requires a database operation".to_string(),
                ));
            }
            let expected_should_block = expected_should_block(
                state.bash_tool,
                state.database_operation,
                state.production_target,
            );
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "db_ops should_block must match Bash/database/production policy: expected \
                     {expected_should_block}, got {}",
                    state.should_block
                )));
            }
            let expected_blocking_finding_count = u64::from(expected_should_block);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "db_ops blocking_finding_count must match should_block: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }
            if state.decision != DbOpsDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "db_ops terminal decision must match derived authorization: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: DbOpsState) -> Result<DbOpsState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_db_ops_graph() -> Result<DbOpsGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("db_ops").await?;
    build_db_ops_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_db_ops_graph_with_ephemeral_sqlite() -> Result<DbOpsGraph, String> {
    build_db_ops_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_db_ops_graph_with_database_path(db_path: &str) -> Result<DbOpsGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_db_ops_graph_with_checkpointer(checkpointer).await
}

async fn build_db_ops_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<DbOpsGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = db_ops_state_schema();
    let builder = StateGraphBuilder::<DbOpsState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: DbOpsState| async move {
                emit_decision_node_event("db_ops", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ALLOW,
            |s: DbOpsState| async move {
                emit_decision_node_event("db_ops", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = DbOpsDecision::Allow;
                Ok::<_, NodeError>(next)
            },
            node_config(ALLOW, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            BLOCK,
            |s: DbOpsState| async move {
                emit_decision_node_event("db_ops", BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = DbOpsDecision::Block;
                Ok::<_, NodeError>(next)
            },
            node_config(BLOCK, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &DbOpsState| match expected_decision(s) {
            DbOpsDecision::Allow => ALLOW.into(),
            DbOpsDecision::Block => BLOCK.into(),
            DbOpsDecision::Unclassified => ALLOW.into(),
        })
        .add_edge(ALLOW, END)
        .add_edge(BLOCK, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_db_ops_decision_report(
    compiled: &DbOpsGraph,
    state: DbOpsState,
) -> Result<DbOpsGraphRun, String> {
    let thread_id =
        crate::decision_graph_store::run_thread_id("db_ops", &state.identifier, &state)?;
    let identifier = state.identifier.clone();
    let streamed = stream_decision_run(compiled, &thread_id, "db_ops", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "db_ops",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(DbOpsGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: db_ops_graph_topology(compiled)?,
    })
}

pub fn db_ops_graph_topology(compiled: &DbOpsGraph) -> Result<DecisionGraphTopology, String> {
    topology("db_ops", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::events::HookInput;

    fn evaluation(command: &str) -> DbOpsEvaluation {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({ "command": command })),
            ..Default::default()
        };
        sentinel_application::hooks::db_ops_gate::evaluate(&input)
    }

    fn missing_command_evaluation() -> DbOpsEvaluation {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({})),
            ..Default::default()
        };
        sentinel_application::hooks::db_ops_gate::evaluate(&input)
    }

    #[tokio::test]
    async fn graph_authorizes_prod_db_block() {
        let graph = build_db_ops_graph_with_ephemeral_sqlite().await.unwrap();
        let state = DbOpsState::from_evaluation(
            "prod-db-block",
            &evaluation("psql production -c 'DROP TABLE users;'"),
        );
        let run = run_db_ops_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, DbOpsDecision::Block);
        assert!(run
            .db_ops_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_nonprod_db_allow() {
        let graph = build_db_ops_graph_with_ephemeral_sqlite().await.unwrap();
        let state =
            DbOpsState::from_evaluation("local-db-allow", &evaluation("prisma migrate dev"));
        assert!(state.command_sha256.is_some());
        let run = run_db_ops_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, DbOpsDecision::Allow);
        assert_eq!(run.state.blocking_finding_count, 0);
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_command_without_command_digest() {
        let graph = build_db_ops_graph_with_ephemeral_sqlite().await.unwrap();
        let mut eval = evaluation("sqlx migrate run --env prod");
        eval.command = None;
        eval.command_present = true;
        let state = DbOpsState::from_evaluation("db-ops-missing-command-digest", &eval);

        let err = run_db_ops_decision_report(&graph, state).await.unwrap_err();

        assert!(err.contains("command_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_command_with_extra_command_digest() {
        let graph = build_db_ops_graph_with_ephemeral_sqlite().await.unwrap();
        let mut state = DbOpsState::from_evaluation(
            "db-ops-extra-command-digest",
            &missing_command_evaluation(),
        );
        state.command_sha256 = Some(command_sha256("ghost command"));

        let err = run_db_ops_decision_report(&graph, state).await.unwrap_err();

        assert!(err.contains("missing-command"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_prod_allow() {
        let graph = build_db_ops_graph_with_ephemeral_sqlite().await.unwrap();
        let mut state = DbOpsState::from_evaluation(
            "forged-prod-allow",
            &evaluation("sqlx migrate run --env prod"),
        );
        state.should_block = false;
        state.blocking_finding_count = 0;
        let err = run_db_ops_decision_report(&graph, state).await.unwrap_err();
        assert!(err.contains("should_block"), "{err}");
    }
}
