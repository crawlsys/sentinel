//! Graph-backed git hygiene authorization.
//!
//! The application hook computes deterministic git hygiene facts for Edit/Write
//! tool calls: protected branch status, worktree/merge exceptions, and dirty
//! file-count limits. This graph authorizes the resulting allow/deny decision
//! through durable LangGraph checkpoints before the CLI permits a source edit.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::git_hygiene::GitHygieneEvaluation;
use sentinel_domain::constants::MAX_UNCOMMITTED_FILES;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GitHygieneDecision {
    #[default]
    Unclassified,
    Allow,
    DenyProtectedBranch,
    DenyUncommittedFileLimit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHygieneState {
    pub identifier: String,
    pub tool: Option<String>,
    pub cwd_sha256: String,
    pub edit_write_tool: bool,
    pub file_path_present: bool,
    pub file_path_sha256: Option<String>,
    pub path_inside_repo: bool,
    pub session_env_path: bool,
    pub hook_applies: bool,
    pub effective_repo_present: bool,
    pub effective_repo_sha256: Option<String>,
    pub branch_known: bool,
    pub branch: Option<String>,
    pub protected_branch: bool,
    pub worktree: bool,
    pub merge_in_progress: bool,
    pub protected_branch_block: bool,
    pub has_uncommitted_changes_known: bool,
    pub has_uncommitted_changes: bool,
    pub changed_files_known: bool,
    pub changed_file_count: u64,
    pub changed_files_sha256: Option<String>,
    pub uncommitted_file_limit_exceeded: bool,
    pub blocking_finding_count: u64,
    pub should_deny: bool,
    pub decision: GitHygieneDecision,
}

impl GitHygieneState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &GitHygieneEvaluation,
    ) -> Self {
        let file_path_sha256 = evaluation
            .file_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty() && evaluation.file_path_present)
            .map(sha256);
        let effective_repo_sha256 = evaluation.effective_repo.as_deref().map(sha256);
        let changed_files_sha256 = if evaluation.changed_files_known {
            Some(sha256(
                &serde_json::to_string(&evaluation.changed_files)
                    .expect("Vec<String> serialization cannot fail"),
            ))
        } else {
            None
        };
        let blocking_finding_count = u64::from(evaluation.protected_branch_block)
            + u64::from(evaluation.uncommitted_file_limit_exceeded);
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            cwd_sha256: sha256(&evaluation.cwd),
            edit_write_tool: evaluation.edit_write_tool,
            file_path_present: evaluation.file_path_present,
            file_path_sha256,
            path_inside_repo: evaluation.path_inside_repo,
            session_env_path: evaluation.session_env_path,
            hook_applies: evaluation.hook_applies,
            effective_repo_present: evaluation.effective_repo.is_some(),
            effective_repo_sha256,
            branch_known: evaluation.branch_known,
            branch: evaluation.branch.clone(),
            protected_branch: evaluation.protected_branch,
            worktree: evaluation.worktree,
            merge_in_progress: evaluation.merge_in_progress,
            protected_branch_block: evaluation.protected_branch_block,
            has_uncommitted_changes_known: evaluation.has_uncommitted_changes_known,
            has_uncommitted_changes: evaluation.has_uncommitted_changes,
            changed_files_known: evaluation.changed_files_known,
            changed_file_count: evaluation.changed_file_count as u64,
            changed_files_sha256,
            uncommitted_file_limit_exceeded: evaluation.uncommitted_file_limit_exceeded,
            blocking_finding_count,
            should_deny: evaluation.should_deny,
            decision: GitHygieneDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GitHygieneGraphRun {
    pub state: GitHygieneState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<GitHygieneState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct GitHygieneAuthorization {
    decision: GitHygieneDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl GitHygieneAuthorization {
    #[must_use]
    pub fn decision(&self) -> GitHygieneDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl GitHygieneGraphRun {
    #[must_use]
    pub fn git_hygiene_authorization(&self) -> Result<Option<GitHygieneAuthorization>, String> {
        if self.state.decision == GitHygieneDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "git_hygiene",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(GitHygieneAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const DENY_PROTECTED_BRANCH: &str = "deny_protected_branch";
const DENY_UNCOMMITTED_FILE_LIMIT: &str = "deny_uncommitted_file_limit";

pub type GitHygieneGraph = CompilationResult<GitHygieneState>;

#[must_use]
pub fn git_hygiene_decision_label(decision: GitHygieneDecision) -> &'static str {
    match decision {
        GitHygieneDecision::Unclassified => "unclassified",
        GitHygieneDecision::Allow => "allow",
        GitHygieneDecision::DenyProtectedBranch => "deny-protected-branch",
        GitHygieneDecision::DenyUncommittedFileLimit => "deny-uncommitted-file-limit",
    }
}

#[must_use]
pub fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn optional_hex_digest_present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(hex_digest_present)
}

fn expected_protected_branch_block(state: &GitHygieneState) -> bool {
    state.hook_applies
        && state.branch_known
        && state.protected_branch
        && !state.worktree
        && !state.merge_in_progress
}

fn expected_uncommitted_file_limit_exceeded(state: &GitHygieneState) -> bool {
    state.hook_applies
        && !state.protected_branch_block
        && state.has_uncommitted_changes_known
        && state.has_uncommitted_changes
        && state.changed_files_known
        && state.changed_file_count > MAX_UNCOMMITTED_FILES as u64
}

fn expected_decision(state: &GitHygieneState) -> GitHygieneDecision {
    if state.protected_branch_block {
        GitHygieneDecision::DenyProtectedBranch
    } else if state.uncommitted_file_limit_exceeded {
        GitHygieneDecision::DenyUncommittedFileLimit
    } else {
        GitHygieneDecision::Allow
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "git_hygiene")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn git_hygiene_state_schema() -> StateSchema<GitHygieneState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "cwd_sha256",
                "edit_write_tool",
                "file_path_present",
                "file_path_sha256",
                "path_inside_repo",
                "session_env_path",
                "hook_applies",
                "effective_repo_present",
                "effective_repo_sha256",
                "branch_known",
                "branch",
                "protected_branch",
                "worktree",
                "merge_in_progress",
                "protected_branch_block",
                "has_uncommitted_changes_known",
                "has_uncommitted_changes",
                "changed_files_known",
                "changed_file_count",
                "changed_files_sha256",
                "uncommitted_file_limit_exceeded",
                "blocking_finding_count",
                "should_deny",
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
                "cwd_sha256": { "type": "string" },
                "edit_write_tool": { "type": "boolean" },
                "file_path_present": { "type": "boolean" },
                "file_path_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "path_inside_repo": { "type": "boolean" },
                "session_env_path": { "type": "boolean" },
                "hook_applies": { "type": "boolean" },
                "effective_repo_present": { "type": "boolean" },
                "effective_repo_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "branch_known": { "type": "boolean" },
                "branch": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "protected_branch": { "type": "boolean" },
                "worktree": { "type": "boolean" },
                "merge_in_progress": { "type": "boolean" },
                "protected_branch_block": { "type": "boolean" },
                "has_uncommitted_changes_known": { "type": "boolean" },
                "has_uncommitted_changes": { "type": "boolean" },
                "changed_files_known": { "type": "boolean" },
                "changed_file_count": { "type": "integer", "minimum": 0 },
                "changed_files_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "uncommitted_file_limit_exceeded": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_deny": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "Allow",
                        "DenyProtectedBranch",
                        "DenyUncommittedFileLimit"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "git_hygiene",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &GitHygieneState| {
            if state
                .tool
                .as_deref()
                .is_none_or(|tool| tool.trim().is_empty())
            {
                return Err(StateError::ValidationFailed(
                    "LangGraph tool-authority state requires concrete tool identity".to_string(),
                ));
            }
            if !hex_digest_present(&state.cwd_sha256) {
                return Err(StateError::ValidationFailed(
                    "git_hygiene cwd_sha256 must be a 64-character hex digest".to_string(),
                ));
            }

            if state.file_path_present {
                if !optional_hex_digest_present(&state.file_path_sha256) {
                    return Err(StateError::ValidationFailed(
                        "git_hygiene file_path_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
            } else if state.file_path_sha256.is_some() {
                return Err(StateError::ValidationFailed(
                    "git_hygiene missing file path cannot carry a file hash".to_string(),
                ));
            }

            if state.effective_repo_present {
                if !optional_hex_digest_present(&state.effective_repo_sha256) {
                    return Err(StateError::ValidationFailed(
                        "git_hygiene effective_repo_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
            } else if state.effective_repo_sha256.is_some() {
                return Err(StateError::ValidationFailed(
                    "git_hygiene missing effective repo cannot carry a repo hash".to_string(),
                ));
            }

            if state.hook_applies {
                if !state.edit_write_tool || !state.path_inside_repo || state.session_env_path {
                    return Err(StateError::ValidationFailed(
                        "git_hygiene applicable hook requires Edit/Write inside repo outside session-env"
                            .to_string(),
                    ));
                }
                if !state.effective_repo_present {
                    return Err(StateError::ValidationFailed(
                        "git_hygiene applicable hook requires an effective repo".to_string(),
                    ));
                }
            } else {
                if state.effective_repo_present
                    || state.branch_known
                    || state.branch.is_some()
                    || state.protected_branch
                    || state.worktree
                    || state.merge_in_progress
                    || state.protected_branch_block
                    || state.has_uncommitted_changes_known
                    || state.has_uncommitted_changes
                    || state.changed_files_known
                    || state.changed_file_count > 0
                    || state.changed_files_sha256.is_some()
                    || state.uncommitted_file_limit_exceeded
                    || state.blocking_finding_count > 0
                    || state.should_deny
                {
                    return Err(StateError::ValidationFailed(
                        "git_hygiene non-applicable state cannot carry authority findings"
                            .to_string(),
                    ));
                }
            }

            if !state.edit_write_tool && state.hook_applies {
                return Err(StateError::ValidationFailed(
                    "git_hygiene non-Edit/Write tools cannot be applicable".to_string(),
                ));
            }

            if state.session_env_path && (!state.file_path_present || !state.path_inside_repo) {
                return Err(StateError::ValidationFailed(
                    "git_hygiene session-env exemption requires an inside-repo file path".to_string(),
                ));
            }

            if state.branch_known {
                let Some(branch) = state.branch.as_deref().filter(|branch| !branch.is_empty()) else {
                    return Err(StateError::ValidationFailed(
                        "git_hygiene known branch requires branch text".to_string(),
                    ));
                };
                let branch_is_protected = matches!(branch, "main" | "master");
                if state.protected_branch != branch_is_protected {
                    return Err(StateError::ValidationFailed(format!(
                        "git_hygiene protected_branch must match branch name: expected \
                         {branch_is_protected}, got {}",
                        state.protected_branch
                    )));
                }
            } else if state.branch.is_some()
                || state.protected_branch
                || state.protected_branch_block
            {
                return Err(StateError::ValidationFailed(
                    "git_hygiene unknown branch cannot carry protected-branch facts".to_string(),
                ));
            }

            let expected_protected_branch_block = expected_protected_branch_block(state);
            if state.protected_branch_block != expected_protected_branch_block {
                return Err(StateError::ValidationFailed(format!(
                    "git_hygiene protected_branch_block must match protected branch policy: expected \
                     {expected_protected_branch_block}, got {}",
                    state.protected_branch_block
                )));
            }

            if state.protected_branch_block
                && (state.has_uncommitted_changes_known
                    || state.has_uncommitted_changes
                    || state.changed_files_known
                    || state.changed_file_count > 0
                    || state.changed_files_sha256.is_some()
                    || state.uncommitted_file_limit_exceeded)
            {
                return Err(StateError::ValidationFailed(
                    "git_hygiene protected branch denial must not carry dirty-worktree facts"
                        .to_string(),
                ));
            }

            if !state.has_uncommitted_changes_known {
                if state.has_uncommitted_changes
                    || state.changed_files_known
                    || state.changed_file_count > 0
                    || state.changed_files_sha256.is_some()
                    || state.uncommitted_file_limit_exceeded
                {
                    return Err(StateError::ValidationFailed(
                        "git_hygiene unknown dirty status cannot carry changed-file facts".to_string(),
                    ));
                }
            } else if !state.has_uncommitted_changes {
                if state.changed_files_known
                    || state.changed_file_count > 0
                    || state.changed_files_sha256.is_some()
                    || state.uncommitted_file_limit_exceeded
                {
                    return Err(StateError::ValidationFailed(
                        "git_hygiene clean dirty-status cannot carry changed-file facts".to_string(),
                    ));
                }
            } else if state.changed_files_known {
                if !optional_hex_digest_present(&state.changed_files_sha256) {
                    return Err(StateError::ValidationFailed(
                        "git_hygiene changed_files_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
            } else if state.changed_file_count > 0
                || state.changed_files_sha256.is_some()
                || state.uncommitted_file_limit_exceeded
            {
                return Err(StateError::ValidationFailed(
                    "git_hygiene unknown changed files cannot carry changed-file count/hash"
                        .to_string(),
                ));
            }

            let expected_uncommitted_file_limit_exceeded =
                expected_uncommitted_file_limit_exceeded(state);
            if state.uncommitted_file_limit_exceeded != expected_uncommitted_file_limit_exceeded {
                return Err(StateError::ValidationFailed(format!(
                    "git_hygiene uncommitted_file_limit_exceeded must match file-count policy: \
                     expected {expected_uncommitted_file_limit_exceeded}, got {}",
                    state.uncommitted_file_limit_exceeded
                )));
            }

            if state.protected_branch_block && state.uncommitted_file_limit_exceeded {
                return Err(StateError::ValidationFailed(
                    "git_hygiene cannot deny for protected branch and file-count limit together"
                        .to_string(),
                ));
            }

            let expected_blocking_finding_count = u64::from(state.protected_branch_block)
                + u64::from(state.uncommitted_file_limit_exceeded);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "git_hygiene blocking_finding_count must match blocking findings: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }

            let expected_should_deny =
                state.protected_branch_block || state.uncommitted_file_limit_exceeded;
            if state.should_deny != expected_should_deny {
                return Err(StateError::ValidationFailed(format!(
                    "git_hygiene should_deny must match blocking findings: expected \
                     {expected_should_deny}, got {}",
                    state.should_deny
                )));
            }

            if state.decision != GitHygieneDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "git_hygiene terminal decision must match derived authorization: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }

            Ok(())
        })
}

async fn classify_node(state: GitHygieneState) -> Result<GitHygieneState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_git_hygiene_graph() -> Result<GitHygieneGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("git_hygiene").await?;
    build_git_hygiene_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_git_hygiene_graph_with_ephemeral_sqlite() -> Result<GitHygieneGraph, String> {
    build_git_hygiene_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_git_hygiene_graph_with_database_path(
    db_path: &str,
) -> Result<GitHygieneGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_git_hygiene_graph_with_checkpointer(checkpointer).await
}

async fn build_git_hygiene_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<GitHygieneGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = git_hygiene_state_schema();
    let builder = StateGraphBuilder::<GitHygieneState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: GitHygieneState| async move {
                emit_decision_node_event("git_hygiene", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ALLOW,
            |s: GitHygieneState| async move {
                emit_decision_node_event("git_hygiene", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = GitHygieneDecision::Allow;
                Ok::<_, NodeError>(next)
            },
            node_config(ALLOW, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            DENY_PROTECTED_BRANCH,
            |s: GitHygieneState| async move {
                emit_decision_node_event("git_hygiene", DENY_PROTECTED_BRANCH, &s.identifier)?;
                let mut next = s;
                next.decision = GitHygieneDecision::DenyProtectedBranch;
                Ok::<_, NodeError>(next)
            },
            node_config(
                DENY_PROTECTED_BRANCH,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            DENY_UNCOMMITTED_FILE_LIMIT,
            |s: GitHygieneState| async move {
                emit_decision_node_event(
                    "git_hygiene",
                    DENY_UNCOMMITTED_FILE_LIMIT,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = GitHygieneDecision::DenyUncommittedFileLimit;
                Ok::<_, NodeError>(next)
            },
            node_config(
                DENY_UNCOMMITTED_FILE_LIMIT,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &GitHygieneState| match expected_decision(s) {
            GitHygieneDecision::Allow => ALLOW.into(),
            GitHygieneDecision::DenyProtectedBranch => DENY_PROTECTED_BRANCH.into(),
            GitHygieneDecision::DenyUncommittedFileLimit => DENY_UNCOMMITTED_FILE_LIMIT.into(),
            GitHygieneDecision::Unclassified => ALLOW.into(),
        })
        .add_edge(ALLOW, END)
        .add_edge(DENY_PROTECTED_BRANCH, END)
        .add_edge(DENY_UNCOMMITTED_FILE_LIMIT, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_git_hygiene_decision_report(
    compiled: &GitHygieneGraph,
    state: GitHygieneState,
) -> Result<GitHygieneGraphRun, String> {
    let thread_id =
        crate::decision_graph_store::run_thread_id("git_hygiene", &state.identifier, &state)?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "git_hygiene", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "git_hygiene",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(GitHygieneGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: git_hygiene_graph_topology(compiled)?,
    })
}

pub fn git_hygiene_graph_topology(
    compiled: &GitHygieneGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("git_hygiene", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::git_hygiene::{
        GitHygieneDecision as AppDecision, GitHygieneEvaluation,
    };

    fn clean_evaluation() -> GitHygieneEvaluation {
        GitHygieneEvaluation {
            tool: Some("Edit".to_string()),
            cwd: "/repo".to_string(),
            file_path: Some("/repo/src/lib.rs".to_string()),
            edit_write_tool: true,
            file_path_present: true,
            path_inside_repo: true,
            session_env_path: false,
            hook_applies: true,
            effective_repo: Some("/repo".to_string()),
            branch_known: true,
            branch: Some("feat/git-hygiene".to_string()),
            protected_branch: false,
            worktree: false,
            merge_in_progress: false,
            protected_branch_block: false,
            has_uncommitted_changes_known: true,
            has_uncommitted_changes: false,
            changed_files_known: false,
            changed_files: Vec::new(),
            changed_file_count: 0,
            uncommitted_file_limit_exceeded: false,
            should_deny: false,
            decision: AppDecision::Allow,
        }
    }

    fn protected_branch_evaluation() -> GitHygieneEvaluation {
        GitHygieneEvaluation {
            branch: Some("main".to_string()),
            protected_branch: true,
            protected_branch_block: true,
            has_uncommitted_changes_known: false,
            should_deny: true,
            decision: AppDecision::DenyProtectedBranch,
            ..clean_evaluation()
        }
    }

    fn dirty_limit_evaluation() -> GitHygieneEvaluation {
        let changed_files = (0..=MAX_UNCOMMITTED_FILES)
            .map(|idx| format!("src/file-{idx}.rs"))
            .collect::<Vec<_>>();
        GitHygieneEvaluation {
            has_uncommitted_changes_known: true,
            has_uncommitted_changes: true,
            changed_files_known: true,
            changed_file_count: changed_files.len(),
            changed_files,
            uncommitted_file_limit_exceeded: true,
            should_deny: true,
            decision: AppDecision::DenyUncommittedFileLimit,
            ..clean_evaluation()
        }
    }

    #[tokio::test]
    async fn graph_authorizes_protected_branch_deny() {
        let graph = build_git_hygiene_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = GitHygieneState::from_evaluation(
            "git-hygiene-protected",
            &protected_branch_evaluation(),
        );
        let run = run_git_hygiene_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, GitHygieneDecision::DenyProtectedBranch);
        assert!(run
            .git_hygiene_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_uncommitted_file_limit_deny() {
        let graph = build_git_hygiene_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state =
            GitHygieneState::from_evaluation("git-hygiene-dirty", &dirty_limit_evaluation());
        let run = run_git_hygiene_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(
            run.state.decision,
            GitHygieneDecision::DenyUncommittedFileLimit
        );
    }

    #[tokio::test]
    async fn graph_authorizes_clean_allow() {
        let graph = build_git_hygiene_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = GitHygieneState::from_evaluation("git-hygiene-clean", &clean_evaluation());
        assert_eq!(state.tool.as_deref(), Some("Edit"));
        assert!(state
            .file_path_sha256
            .as_deref()
            .is_some_and(hex_digest_present));
        assert!(state
            .effective_repo_sha256
            .as_deref()
            .is_some_and(hex_digest_present));
        assert_eq!(state.branch.as_deref(), Some("feat/git-hygiene"));
        let run = run_git_hygiene_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, GitHygieneDecision::Allow);
    }

    #[tokio::test]
    async fn graph_authorizes_absent_file_path_without_digest() {
        let graph = build_git_hygiene_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let evaluation = GitHygieneEvaluation {
            file_path: None,
            file_path_present: false,
            ..clean_evaluation()
        };
        let state = GitHygieneState::from_evaluation("git-hygiene-no-file-path", &evaluation);
        assert!(state.file_path_sha256.is_none());
        let run = run_git_hygiene_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, GitHygieneDecision::Allow);
        assert!(run.state.file_path_sha256.is_none());
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_dirty_allow() {
        let graph = build_git_hygiene_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state =
            GitHygieneState::from_evaluation("git-hygiene-forged", &dirty_limit_evaluation());
        state.uncommitted_file_limit_exceeded = false;
        state.blocking_finding_count = 0;
        state.should_deny = false;
        let err = run_git_hygiene_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("uncommitted_file_limit_exceeded"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_branch_fact_mismatch() {
        let graph = build_git_hygiene_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = GitHygieneState::from_evaluation(
            "git-hygiene-branch-mismatch",
            &protected_branch_evaluation(),
        );
        state.protected_branch = false;
        state.protected_branch_block = false;
        state.blocking_finding_count = 0;
        state.should_deny = false;
        let err = run_git_hygiene_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("protected_branch"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_file_path_hash_evidence() {
        let graph = build_git_hygiene_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state =
            GitHygieneState::from_evaluation("git-hygiene-missing-file-hash", &clean_evaluation());
        state.file_path_sha256 = None;
        let err = run_git_hygiene_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("file_path_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_changed_files_hash_evidence() {
        let graph = build_git_hygiene_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = GitHygieneState::from_evaluation(
            "git-hygiene-missing-changed-hash",
            &dirty_limit_evaluation(),
        );
        state.changed_files_sha256 = None;
        let err = run_git_hygiene_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("changed_files_sha256"), "{err}");
    }
}
