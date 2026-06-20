//! Graph-backed Linear comment authorization for live enforcement notes.

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LinearCommentDecision {
    Post,
    #[default]
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearCommentState {
    pub identifier: String,
    pub issue_id: String,
    pub category: String,
    pub body: String,
    pub checkpoint_refs: Vec<String>,
    pub decision: LinearCommentDecision,
}

impl LinearCommentState {
    #[must_use]
    pub fn new(
        identifier: impl Into<String>,
        issue_id: impl Into<String>,
        category: impl Into<String>,
        body: impl Into<String>,
        checkpoint_refs: Vec<String>,
    ) -> Self {
        Self {
            identifier: identifier.into(),
            issue_id: issue_id.into(),
            category: category.into(),
            body: body.into(),
            checkpoint_refs,
            decision: LinearCommentDecision::Skip,
        }
    }

    fn can_post(&self) -> bool {
        !self.identifier.trim().is_empty()
            && !self.issue_id.trim().is_empty()
            && !self.category.trim().is_empty()
            && !self.body.trim().is_empty()
            && !self.checkpoint_refs.is_empty()
            && self.body.contains("LangGraph checkpoints:")
            && self
                .checkpoint_refs
                .iter()
                .all(|checkpoint| !checkpoint.trim().is_empty() && self.body.contains(checkpoint))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LinearCommentRun {
    pub state: LinearCommentState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<LinearCommentState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct LinearCommentApplyResult {
    pub run: LinearCommentRun,
    pub posted: bool,
}

const CLASSIFY: &str = "classify";
const POST: &str = "post_comment";
const SKIP: &str = "skip";

pub type LinearCommentGraph = CompilationResult<LinearCommentState>;

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "linear_comment")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
}

fn linear_comment_state_schema() -> StateSchema<LinearCommentState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "issue_id",
                "category",
                "body",
                "checkpoint_refs",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "issue_id": { "type": "string" },
                "category": { "type": "string" },
                "body": { "type": "string" },
                "checkpoint_refs": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "decision": { "type": "string", "enum": ["Post", "Skip"] }
            },
            "x-sentinel": {
                "graph": "linear_comment",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &LinearCommentState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "linear comment identifier must not be empty".to_string(),
                ));
            }
            if state.decision == LinearCommentDecision::Post && !state.can_post() {
                return Err(StateError::ValidationFailed(
                    "linear comment Post requires issue id, category, body, and referenced LangGraph checkpoints"
                        .to_string(),
                ));
            }
            Ok(())
        })
}

pub async fn build_linear_comment_graph() -> Result<LinearCommentGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("linear_comment").await?;
    build_linear_comment_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_linear_comment_graph_with_ephemeral_sqlite() -> Result<LinearCommentGraph, String> {
    build_linear_comment_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_linear_comment_graph_with_database_path(
    database_path: &str,
) -> Result<LinearCommentGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_linear_comment_graph_with_checkpointer(checkpointer).await
}

async fn build_linear_comment_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<LinearCommentGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = linear_comment_state_schema();
    let builder = StateGraphBuilder::<LinearCommentState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_node_defaults(crate::decision_graph_introspection::decision_node_defaults())
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: LinearCommentState| async move {
                emit_decision_node_event("linear_comment", CLASSIFY, &s.identifier)?;
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
            POST,
            |s: LinearCommentState| async move {
                emit_decision_node_event("linear_comment", POST, &s.identifier)?;
                let mut next = s;
                next.decision = LinearCommentDecision::Post;
                Ok::<_, NodeError>(next)
            },
            node_config(
                POST,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            SKIP,
            |s: LinearCommentState| async move {
                emit_decision_node_event("linear_comment", SKIP, &s.identifier)?;
                let mut next = s;
                next.decision = LinearCommentDecision::Skip;
                Ok::<_, NodeError>(next)
            },
            node_config(
                SKIP,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &LinearCommentState| {
            if s.can_post() {
                POST.into()
            } else {
                SKIP.into()
            }
        })
        .add_edge(POST, END)
        .add_edge(SKIP, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_linear_comment_decision_report(
    compiled: &LinearCommentGraph,
    state: LinearCommentState,
) -> Result<LinearCommentRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "linear_comment",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "linear_comment", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "linear_comment",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(LinearCommentRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: linear_comment_graph_topology(compiled)?,
    })
}

pub fn linear_comment_graph_topology(
    compiled: &LinearCommentGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("linear_comment", compiled)
}

pub async fn post_graph_authorized_comment(
    client: &reqwest::Client,
    token: &str,
    compiled: &LinearCommentGraph,
    state: LinearCommentState,
) -> Result<LinearCommentApplyResult, String> {
    let run = run_linear_comment_decision_report(compiled, state).await?;
    if run.state.decision != LinearCommentDecision::Post {
        return Ok(LinearCommentApplyResult { run, posted: false });
    }

    let comment_checkpoint_ref = terminal_decision_checkpoint_result(
        "linear_comment",
        &run.thread_id,
        &run.state,
        &run.checkpoints,
        &run.write_history,
    )
    .map(|checkpoint| checkpoint.checkpoint_ref())?;
    let body = body_with_comment_checkpoint(&run.state.body, &comment_checkpoint_ref);
    let posted =
        crate::remediation::post_comment(client, token, &run.state.issue_id, &body).await?;
    Ok(LinearCommentApplyResult { run, posted })
}

fn body_with_comment_checkpoint(body: &str, comment_checkpoint_ref: &str) -> String {
    if body.contains("LangGraph checkpoints:") {
        format!("{body}, comment `{comment_checkpoint_ref}`")
    } else {
        format!("{body}\n\nLangGraph checkpoints: comment `{comment_checkpoint_ref}`")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> LinearCommentState {
        LinearCommentState::new(
            "FPCRM-1",
            "uuid-1",
            "sla_breached",
            "## SLA breached\n\nAct now.\n\nLangGraph checkpoints: enforcement `e1`",
            vec!["e1".into()],
        )
    }

    #[tokio::test]
    async fn graph_authorizes_checkpointed_comment() {
        let graph = build_linear_comment_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let run = run_linear_comment_decision_report(&graph, state())
            .await
            .expect("runs");
        assert_eq!(run.state.decision, LinearCommentDecision::Post);
        assert!(!run.checkpoints.is_empty(), "run must expose checkpoints");
        assert_eq!(
            run.checkpoints.first().expect("latest").thread_id,
            run.thread_id
        );
        assert_eq!(
            run.checkpoints.first().expect("latest").state.decision,
            LinearCommentDecision::Post
        );
        assert!(!run.stream.is_empty(), "run must expose stream parts");
        assert!(
            run.stream
                .iter()
                .any(|part| part.event_type == "ExecutionComplete"),
            "stream must expose LangGraph execution completion"
        );
        assert!(
            run.stream.iter().any(|part| part.payload_kind == "values"),
            "stream must expose LangGraph values payloads"
        );
        assert!(
            run.stream
                .iter()
                .any(|part| part.payload_kind == "checkpoints"),
            "stream must expose LangGraph checkpoint payloads"
        );
        assert!(
            run.stream.iter().any(|part| {
                part.payload_kind == "custom"
                    && part.payload_json["type"] == "sentinel.decision_node"
                    && part.payload_json["graph"] == "linear_comment"
            }),
            "stream must expose Sentinel custom decision-node payloads"
        );
        assert!(
            run.write_history
                .iter()
                .any(|write| write.channel == "state"),
            "run must expose state channel write history"
        );
        assert!(
            run.write_history
                .iter()
                .all(|write| write.value_len > 0 && write.value_sha256.len() == 64),
            "write history must expose value length and sha256"
        );
        assert!(
            run.write_history
                .iter()
                .filter(|write| write.channel == "state")
                .any(|write| write.value_json["decision"] == "Post"),
            "state write history must decode the terminal decision JSON"
        );
        let post_checkpoint = terminal_decision_checkpoint_result(
            "linear_comment",
            &run.thread_id,
            &run.state,
            &run.checkpoints,
            &run.write_history,
        )
        .expect("Post run must expose decision-node checkpoint");
        assert_eq!(post_checkpoint.source_node.as_deref(), Some(POST));
        assert_eq!(
            post_checkpoint
                .writes
                .iter()
                .find(|write| write.channel == "state")
                .expect("post checkpoint state write")
                .node_id
                .as_str(),
            POST
        );
        assert_eq!(run.topology.graph, "linear_comment");
        assert!(run.topology.durable_checkpointer);
        assert_eq!(run.topology.checkpointer_backend, "sqlite");
        assert!(
            run.topology
                .checkpointer_scope
                .starts_with("database_path:"),
            "topology must expose sanitized checkpoint scope"
        );
        assert_eq!(
            run.topology.schemas.state.as_ref().expect("state schema")["x-sentinel"]["graph"],
            "linear_comment"
        );
        assert!(run.topology.schemas.input.is_some());
        assert!(run.topology.schemas.output.is_some());
        assert!(
            run.topology
                .edges
                .iter()
                .any(|edge| edge.kind == "conditional"),
            "topology must expose post/skip conditional routing"
        );
    }

    #[tokio::test]
    async fn graph_skips_uncheckpointed_or_incomplete_comment() {
        let graph = build_linear_comment_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        for state in [
            LinearCommentState::new("FPCRM-1", "uuid-1", "missing_refs", "body", vec![]),
            LinearCommentState::new(
                "FPCRM-1",
                "uuid-1",
                "missing_note",
                "body",
                vec!["e1".into()],
            ),
            LinearCommentState::new(
                "FPCRM-1",
                "uuid-1",
                "missing_ref_in_body",
                "LangGraph checkpoints: enforcement `other`",
                vec!["e1".into()],
            ),
            LinearCommentState::new(
                "FPCRM-1",
                "",
                "missing_issue",
                "LangGraph checkpoints: enforcement `e1`",
                vec!["e1".into()],
            ),
            LinearCommentState::new("FPCRM-1", "uuid-1", "missing_body", "", vec!["e1".into()]),
        ] {
            let run = run_linear_comment_decision_report(&graph, state)
                .await
                .expect("runs");
            assert_eq!(run.state.decision, LinearCommentDecision::Skip);
        }
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_post_authorization() {
        let graph = build_linear_comment_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut forged =
            LinearCommentState::new("FPCRM-1", "uuid-1", "missing_refs", "body", vec![]);
        forged.decision = LinearCommentDecision::Post;

        let err = run_linear_comment_decision_report(&graph, forged)
            .await
            .expect_err("forged Post state must fail LangGraph schema validation");
        assert!(
            err.contains("linear comment Post requires"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn appends_comment_checkpoint_to_existing_audit_note() {
        assert_eq!(
            body_with_comment_checkpoint("x\n\nLangGraph checkpoints: enforcement `e1`", "c1#k1"),
            "x\n\nLangGraph checkpoints: enforcement `e1`, comment `c1#k1`"
        );
    }
}
