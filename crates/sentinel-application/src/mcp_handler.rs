//! MCP Tool Handler
//!
//! Routes MCP tool calls (sentinel__*) to the appropriate engine/proof functions.
//! This is how Claude interacts with Sentinel directly.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use sentinel_domain::proof::ProofEntry;
use sentinel_domain::state::SessionState;

use crate::proof_engine::ProofEngine;

/// MCP tool call request
#[derive(Debug, Clone, Deserialize)]
pub struct McpToolCall {
    /// Tool name (e.g., "sentinel__submit_evidence")
    pub name: String,

    /// Tool arguments as JSON
    pub arguments: serde_json::Value,
}

/// MCP tool call response
#[derive(Debug, Clone, Serialize)]
pub struct McpToolResult {
    /// Whether the call succeeded
    pub success: bool,

    /// Result content
    pub content: serde_json::Value,

    /// Error message if failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl McpToolResult {
    pub fn ok(content: serde_json::Value) -> Self {
        Self {
            success: true,
            content,
            error: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            success: false,
            content: serde_json::Value::Null,
            error: Some(message.into()),
        }
    }
}

/// MCP handler — routes tool calls to engine functions
pub struct McpHandler {
    state: Arc<RwLock<SessionState>>,
    proof_engine: Arc<ProofEngine>,
}

impl McpHandler {
    pub fn new(state: Arc<RwLock<SessionState>>, proof_engine: Arc<ProofEngine>) -> Self {
        Self {
            state,
            proof_engine,
        }
    }

    /// Handle an MCP tool call
    pub async fn handle(&self, call: McpToolCall) -> McpToolResult {
        match call.name.as_str() {
            "sentinel__get_proof_chain" => self.get_proof_chain(call.arguments).await,
            "sentinel__get_workflow_status" => self.get_workflow_status(call.arguments).await,
            "sentinel__verify_chain" => self.verify_chain(call.arguments).await,
            // ── Step-level (M4.1) ─────────────────────────────────────
            "sentinel__get_step_proof" => self.get_step_proof(call.arguments).await,
            "sentinel__get_step_chain" => self.get_step_chain(call.arguments).await,
            "sentinel__get_active_step" => self.get_active_step(call.arguments).await,
            // ── Step-level write (M4.2) ──────────────────────────────
            "sentinel__submit_step_complete" => self.submit_step_complete(call.arguments).await,
            _ => McpToolResult::err(format!("Unknown tool: {}", call.name)),
        }
    }

    async fn get_proof_chain(&self, args: serde_json::Value) -> McpToolResult {
        let skill = match args.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'skill' argument"),
        };

        let state = self.state.read().await;
        match state.proof_chains.get(skill) {
            Some(chain) => match serde_json::to_value(chain) {
                Ok(v) => McpToolResult::ok(v),
                Err(e) => McpToolResult::err(format!("Serialization error: {e}")),
            },
            None => McpToolResult::err(format!("No proof chain for skill '{skill}'")),
        }
    }

    async fn get_workflow_status(&self, args: serde_json::Value) -> McpToolResult {
        let skill = match args.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'skill' argument"),
        };

        let state = self.state.read().await;
        match state.workflows.get(skill) {
            Some(wf) => match serde_json::to_value(wf) {
                Ok(v) => McpToolResult::ok(v),
                Err(e) => McpToolResult::err(format!("Serialization error: {e}")),
            },
            None => McpToolResult::err(format!("No workflow state for skill '{skill}'")),
        }
    }

    async fn verify_chain(&self, args: serde_json::Value) -> McpToolResult {
        let skill = match args.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'skill' argument"),
        };

        match self.proof_engine.verify_chain(skill).await {
            Ok(verification) => match serde_json::to_value(&verification) {
                Ok(v) => McpToolResult::ok(v),
                Err(e) => McpToolResult::err(format!("Serialization error: {e}")),
            },
            Err(e) => McpToolResult::err(format!("Verification failed: {e}")),
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Step-level tools (M4.1)
    //
    // These three tools expose the step-level chain that M1.1-M1.5
    // built. Together they give external MCP servers (skills-mcp,
    // agents-mcp, dashboards) a clean read surface against the chain
    // without needing to mirror sentinel's serialization format.
    //
    // - get_step_proof(skill, step_id [, phase_id]) → single StepProof
    // - get_step_chain(skill) → ordered list of step entries with
    //   verification status (the chain restricted to step entries)
    // - get_active_step(skill) → which step is "next" to run
    //   (skill's chain head + the immediate next step from config,
    //   if config is loaded into state)
    // ─────────────────────────────────────────────────────────────────

    /// Return a single [`StepProof`](sentinel_domain::step_proof::StepProof)
    /// matching `(skill, step_id [, phase_id])`. Phase id disambiguates
    /// when the same step_id repeats across phases (e.g. "1" in both
    /// "claim" and "review" phases).
    async fn get_step_proof(&self, args: serde_json::Value) -> McpToolResult {
        let skill = match args.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'skill' argument"),
        };
        let step_id = match args.get("step_id").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'step_id' argument"),
        };
        // phase_id is optional — only required when the chain has step_ids
        // that collide across phases (uncommon but possible).
        let phase_filter = args.get("phase_id").and_then(|v| v.as_str());

        let state = self.state.read().await;
        let chain = match state.proof_chains.get(skill) {
            Some(c) => c,
            None => return McpToolResult::err(format!("No proof chain for skill '{skill}'")),
        };

        // Walk the mixed-entry chain looking for a matching step entry.
        // We search in reverse so the *most recent* matching step wins
        // when an idempotent step_id has been re-recorded — important
        // for replay/resubmission semantics.
        let found = chain.entries.iter().rev().find_map(|e| match e {
            ProofEntry::Step(s) if s.step_id == step_id => match phase_filter {
                Some(p) if s.phase_id != p => None,
                _ => Some(s),
            },
            _ => None,
        });

        match found {
            Some(proof) => match serde_json::to_value(proof) {
                Ok(v) => McpToolResult::ok(v),
                Err(e) => McpToolResult::err(format!("Serialization error: {e}")),
            },
            None => McpToolResult::err(format!(
                "No StepProof for skill '{skill}', step_id '{step_id}'{}",
                phase_filter
                    .map(|p| format!(" (phase '{p}')"))
                    .unwrap_or_default(),
            )),
        }
    }

    /// Return all step entries from the chain for a skill, in order.
    /// Phase entries are filtered out — callers wanting the full
    /// mixed chain should use `sentinel__get_proof_chain`.
    ///
    /// Response shape:
    /// ```json
    /// {
    ///   "skill": "linear",
    ///   "session_id": "...",
    ///   "step_count": 3,
    ///   "head_hash": "...",
    ///   "steps": [ {step_id, phase_id, combined_hash, ...}, ... ]
    /// }
    /// ```
    async fn get_step_chain(&self, args: serde_json::Value) -> McpToolResult {
        let skill = match args.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'skill' argument"),
        };

        let state = self.state.read().await;
        let chain = match state.proof_chains.get(skill) {
            Some(c) => c,
            None => return McpToolResult::err(format!("No proof chain for skill '{skill}'")),
        };

        let steps: Vec<&sentinel_domain::step_proof::StepProof> = chain
            .entries
            .iter()
            .filter_map(|e| match e {
                ProofEntry::Step(s) => Some(s),
                _ => None,
            })
            .collect();

        let payload = serde_json::json!({
            "skill": chain.skill,
            "session_id": chain.session_id,
            "step_count": steps.len(),
            "head_hash": chain.head_hash(),
            "steps": steps,
        });
        McpToolResult::ok(payload)
    }

    /// Return the chain's "active step" for a skill — i.e. the head of
    /// the chain plus a hint at what's expected next.
    ///
    /// Response shape:
    /// ```json
    /// {
    ///   "skill": "linear",
    ///   "head_hash": "...",
    ///   "last_step": { "phase_id": "claim", "step_id": "2", ... } | null,
    ///   "chain_length": 5,
    ///   "phase_count": 1,
    ///   "step_count": 4
    /// }
    /// ```
    async fn get_active_step(&self, args: serde_json::Value) -> McpToolResult {
        let skill = match args.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'skill' argument"),
        };

        let state = self.state.read().await;
        let chain = match state.proof_chains.get(skill) {
            Some(c) => c,
            None => return McpToolResult::err(format!("No proof chain for skill '{skill}'")),
        };

        let phase_count = chain.proofs.len()
            + chain
                .entries
                .iter()
                .filter(|e| matches!(e, ProofEntry::Phase(_)))
                .count();
        let step_count = chain
            .entries
            .iter()
            .filter(|e| matches!(e, ProofEntry::Step(_)))
            .count();

        // last_step = last Step entry in the chain (None if no step
        // entries yet — chain may still be in phase-only mode).
        let last_step = chain.entries.iter().rev().find_map(|e| match e {
            ProofEntry::Step(s) => Some(serde_json::json!({
                "phase_id": s.phase_id,
                "step_id": s.step_id,
                "combined_hash": s.combined_hash,
                "completed_at": s.completed_at,
            })),
            _ => None,
        });

        let payload = serde_json::json!({
            "skill": skill,
            "head_hash": chain.head_hash(),
            "last_step": last_step,
            "chain_length": chain.proofs.len() + chain.entries.len(),
            "phase_count": phase_count,
            "step_count": step_count,
        });
        McpToolResult::ok(payload)
    }

    /// Seal a judged step into the proof chain (M4.2).
    ///
    /// Wraps [`ProofEngine::submit_step_evidence`] so external MCP
    /// servers (skills-mcp, agents-mcp) can advance the chain remotely
    /// without needing direct access to sentinel-application internals.
    ///
    /// Required arguments:
    /// - `skill` (string)
    /// - `phase_id` (string)
    /// - `step_id` (string)
    /// - `step_description` (string) — what "sufficient" means for this step
    /// - `verdict` (object) — JudgeVerdict { sufficient, confidence, reasoning, requested_evidence? }
    ///
    /// Optional arguments (sensible defaults applied when omitted):
    /// - `evidence` (object) — defaults to empty Evidence
    /// - `judge_model` (string: "sonnet" | "opus" | "haiku") — defaults to "sonnet"
    /// - `artifact` (any JSON value) — defaults to null
    /// - `account_context` (string|null) — defaults to null
    /// - `started_at` (RFC3339 string) — defaults to now-1ms
    ///
    /// Returns the sealed StepProof on success, or an error on
    /// insufficient verdict / chain-link mismatch / serialization
    /// failure. Refusing to seal an insufficient verdict is the
    /// engine's job — surface the error here for caller telemetry.
    async fn submit_step_complete(&self, args: serde_json::Value) -> McpToolResult {
        // Required string fields.
        let Some(skill) = args.get("skill").and_then(|v| v.as_str()) else {
            return McpToolResult::err("Missing 'skill' argument");
        };
        let Some(phase_id) = args.get("phase_id").and_then(|v| v.as_str()) else {
            return McpToolResult::err("Missing 'phase_id' argument");
        };
        let Some(step_id) = args.get("step_id").and_then(|v| v.as_str()) else {
            return McpToolResult::err("Missing 'step_id' argument");
        };
        let Some(step_description) = args.get("step_description").and_then(|v| v.as_str()) else {
            return McpToolResult::err("Missing 'step_description' argument");
        };

        // Required: the verdict. Deserialize, sanitize, surface clear
        // errors when the shape is wrong.
        let verdict_raw = match args.get("verdict") {
            Some(v) => v.clone(),
            None => return McpToolResult::err("Missing 'verdict' argument"),
        };
        let verdict: sentinel_domain::judge::JudgeVerdict =
            match serde_json::from_value(verdict_raw) {
                Ok(v) => sentinel_domain::judge::JudgeVerdict::sanitized(v),
                Err(e) => return McpToolResult::err(format!("Invalid 'verdict' shape: {e}")),
            };

        // Optional fields with defaults.
        let evidence: sentinel_domain::evidence::Evidence = match args.get("evidence") {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(e) => e,
                Err(e) => return McpToolResult::err(format!("Invalid 'evidence' shape: {e}")),
            },
            None => sentinel_domain::evidence::Evidence::default(),
        };

        let judge_model = match args.get("judge_model").and_then(|v| v.as_str()) {
            Some("sonnet") | None => sentinel_domain::judge::JudgeModel::Sonnet,
            Some("opus") => sentinel_domain::judge::JudgeModel::Opus,
            Some("haiku") => sentinel_domain::judge::JudgeModel::Haiku,
            Some(other) => {
                return McpToolResult::err(format!(
                    "Unknown judge_model '{other}' — expected sonnet | opus | haiku"
                ));
            }
        };

        let artifact = args
            .get("artifact")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let account_context = args
            .get("account_context")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // started_at — accept RFC3339 string, fall back to now-1ms so
        // started_at < completed_at (set inside the engine = now).
        let started_at = match args.get("started_at").and_then(|v| v.as_str()) {
            Some(ts) => match chrono::DateTime::parse_from_rfc3339(ts) {
                Ok(dt) => dt.with_timezone(&chrono::Utc),
                Err(e) => {
                    return McpToolResult::err(format!(
                        "Invalid 'started_at' (expected RFC3339): {e}"
                    ));
                }
            },
            None => chrono::Utc::now() - chrono::Duration::milliseconds(1),
        };

        match self
            .proof_engine
            .submit_step_evidence(
                skill,
                phase_id,
                step_id,
                step_description,
                evidence,
                verdict,
                judge_model,
                artifact,
                account_context,
                started_at,
            )
            .await
        {
            Ok(proof) => match serde_json::to_value(&proof) {
                Ok(v) => McpToolResult::ok(v),
                Err(e) => McpToolResult::err(format!("Serialization error: {e}")),
            },
            Err(e) => McpToolResult::err(format!("submit_step_complete failed: {e}")),
        }
    }
}

#[cfg(test)]
mod step_tools_tests {
    //! Tests for M4.1 step-level MCP tools. Drives the handler end-to-end
    //! against a real ProofEngine + state, asserts response shapes match
    //! what external MCP callers will rely on.

    use super::*;
    use crate::judge_service::JudgeService;
    use anyhow::Result;
    use chrono::Utc;
    use sentinel_domain::evidence::Evidence;
    use sentinel_domain::judge::{JudgeModel, JudgeVerdict};

    struct StubJudge;
    #[async_trait::async_trait]
    impl JudgeService for StubJudge {
        async fn evaluate(
            &self,
            _skill: &str,
            _phase_id: &str,
            _phase_objectives: &str,
            _evidence: &Evidence,
            _model: JudgeModel,
        ) -> Result<JudgeVerdict> {
            unreachable!("step tools never call evaluate()")
        }
    }

    async fn handler_with_chain() -> McpHandler {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = Arc::new(ProofEngine::new(state.clone(), Arc::new(StubJudge)));
        let handler = McpHandler::new(state, engine.clone());

        // Seed chain: 2 step proofs in phase "claim".
        engine
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Sonnet,
                serde_json::json!({"ticket": "FPCRM-1"}),
                Some("firefly-pro".into()),
                Utc::now(),
            )
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        engine
            .submit_step_evidence(
                "linear",
                "claim",
                "2",
                "create branch",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Sonnet,
                serde_json::json!({"branch": "fpcrm-1-fix"}),
                Some("firefly-pro".into()),
                Utc::now(),
            )
            .await
            .unwrap();

        handler
    }

    #[tokio::test]
    async fn unknown_step_tool_name_errors_with_clear_message() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__nonexistent".into(),
                arguments: serde_json::json!({}),
            })
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown tool"));
    }

    #[tokio::test]
    async fn get_step_proof_returns_matching_step() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_step_proof".into(),
                arguments: serde_json::json!({"skill": "linear", "step_id": "1"}),
            })
            .await;
        assert!(result.success, "error: {:?}", result.error);
        let proof = result.content;
        assert_eq!(proof.get("step_id").and_then(|v| v.as_str()), Some("1"));
        assert_eq!(proof.get("skill").and_then(|v| v.as_str()), Some("linear"));
        assert_eq!(proof.get("phase_id").and_then(|v| v.as_str()), Some("claim"));
        assert!(proof.get("combined_hash").is_some());
    }

    #[tokio::test]
    async fn get_step_proof_404s_for_missing_step() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_step_proof".into(),
                arguments: serde_json::json!({"skill": "linear", "step_id": "99"}),
            })
            .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("step_id '99'"));
    }

    #[tokio::test]
    async fn get_step_proof_filters_by_phase_when_supplied() {
        let handler = handler_with_chain().await;
        // step_id "1" exists in phase "claim". Asking for it under
        // a phase that doesn't contain it must 404.
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_step_proof".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "step_id": "1",
                    "phase_id": "review", // wrong phase
                }),
            })
            .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("phase 'review'"));
    }

    #[tokio::test]
    async fn get_step_proof_requires_skill_and_step_id() {
        let handler = handler_with_chain().await;
        for bad_args in [
            serde_json::json!({}),                   // missing both
            serde_json::json!({"skill": "linear"}), // missing step_id
            serde_json::json!({"step_id": "1"}),    // missing skill
        ] {
            let result = handler
                .handle(McpToolCall {
                    name: "sentinel__get_step_proof".into(),
                    arguments: bad_args,
                })
                .await;
            assert!(!result.success);
        }
    }

    #[tokio::test]
    async fn get_step_chain_returns_all_steps_in_order() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_step_chain".into(),
                arguments: serde_json::json!({"skill": "linear"}),
            })
            .await;
        assert!(result.success);
        let payload = result.content;
        assert_eq!(payload.get("skill").and_then(|v| v.as_str()), Some("linear"));
        assert_eq!(payload.get("step_count").and_then(|v| v.as_u64()), Some(2));
        let steps = payload.get("steps").and_then(|v| v.as_array()).unwrap();
        assert_eq!(steps.len(), 2);
        // Order check — step "1" before step "2" in the array.
        assert_eq!(steps[0].get("step_id").and_then(|v| v.as_str()), Some("1"));
        assert_eq!(steps[1].get("step_id").and_then(|v| v.as_str()), Some("2"));
        // head_hash matches the last step's combined_hash.
        let last_combined = steps[1].get("combined_hash").and_then(|v| v.as_str()).unwrap();
        assert_eq!(payload.get("head_hash").and_then(|v| v.as_str()), Some(last_combined));
    }

    #[tokio::test]
    async fn get_step_chain_404s_for_unknown_skill() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_step_chain".into(),
                arguments: serde_json::json!({"skill": "nonexistent"}),
            })
            .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("nonexistent"));
    }

    #[tokio::test]
    async fn get_active_step_reports_last_step_and_counts() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_active_step".into(),
                arguments: serde_json::json!({"skill": "linear"}),
            })
            .await;
        assert!(result.success);
        let payload = result.content;
        assert_eq!(payload.get("skill").and_then(|v| v.as_str()), Some("linear"));
        assert_eq!(payload.get("step_count").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(payload.get("phase_count").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(payload.get("chain_length").and_then(|v| v.as_u64()), Some(2));
        let last = payload.get("last_step").unwrap();
        assert_eq!(last.get("step_id").and_then(|v| v.as_str()), Some("2"));
        assert_eq!(last.get("phase_id").and_then(|v| v.as_str()), Some("claim"));
        assert!(last.get("combined_hash").is_some());
    }

    // ─────────────────────────────────────────────────────────────────
    // M4.2: submit_step_complete tests
    // ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn submit_step_complete_seals_step_with_minimal_args() {
        // Smallest legal call: skill + phase_id + step_id + step_description + verdict.
        // Everything else takes defaults.
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = Arc::new(ProofEngine::new(state.clone(), Arc::new(StubJudge)));
        let handler = McpHandler::new(state, engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "fetch ticket",
                    "verdict": {
                        "sufficient": true,
                        "confidence": 0.93,
                        "reasoning": "evidence present",
                    },
                }),
            })
            .await;

        assert!(result.success, "error: {:?}", result.error);
        let proof = result.content;
        assert_eq!(proof.get("skill").and_then(|v| v.as_str()), Some("linear"));
        assert_eq!(proof.get("step_id").and_then(|v| v.as_str()), Some("1"));
        assert_eq!(proof.get("phase_id").and_then(|v| v.as_str()), Some("claim"));
        assert!(proof.get("combined_hash").is_some());
        // Default judge_model is sonnet.
        assert_eq!(
            proof.get("judge_model").and_then(|v| v.as_str()),
            Some("openai/gpt-5.3"),
        );
    }

    #[tokio::test]
    async fn submit_step_complete_propagates_artifact_and_account_context() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = Arc::new(ProofEngine::new(state.clone(), Arc::new(StubJudge)));
        let handler = McpHandler::new(state, engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "open PR",
                    "verdict": {"sufficient": true, "confidence": 0.95, "reasoning": "ok"},
                    "artifact": {"pr_url": "https://github.com/foo/bar/pull/9", "pr_number": 9},
                    "account_context": "firefly-pro",
                    "judge_model": "opus",
                }),
            })
            .await;

        assert!(result.success);
        let proof = result.content;
        assert_eq!(
            proof.get("account_context").and_then(|v| v.as_str()),
            Some("firefly-pro"),
        );
        assert_eq!(
            proof.get("artifact").and_then(|v| v.get("pr_url")).and_then(|v| v.as_str()),
            Some("https://github.com/foo/bar/pull/9"),
        );
        assert_eq!(
            proof.get("judge_model").and_then(|v| v.as_str()),
            Some("anthropic/opus-4.6"),
        );
    }

    #[tokio::test]
    async fn submit_step_complete_rejects_insufficient_verdict() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = Arc::new(ProofEngine::new(state.clone(), Arc::new(StubJudge)));
        let handler = McpHandler::new(state.clone(), engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "fetch ticket",
                    "verdict": {
                        "sufficient": false,
                        "confidence": 0.7,
                        "reasoning": "missing FPCRM ref in PR body",
                        "requested_evidence": ["Ref FPCRM-XXX in PR body"],
                    },
                }),
            })
            .await;

        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("insufficient"), "error mentions insufficient: {err}");
        // No chain mutation on failure.
        let s = state.read().await;
        assert!(!s.proof_chains.contains_key("linear"));
    }

    #[tokio::test]
    async fn submit_step_complete_validates_required_args() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = Arc::new(ProofEngine::new(state.clone(), Arc::new(StubJudge)));
        let handler = McpHandler::new(state, engine);

        // Each entry below is missing exactly one required field.
        let cases = [
            (serde_json::json!({}), "skill"),
            (
                serde_json::json!({"skill": "linear"}),
                "phase_id",
            ),
            (
                serde_json::json!({"skill": "linear", "phase_id": "claim"}),
                "step_id",
            ),
            (
                serde_json::json!({
                    "skill": "linear", "phase_id": "claim", "step_id": "1"
                }),
                "step_description",
            ),
            (
                serde_json::json!({
                    "skill": "linear", "phase_id": "claim", "step_id": "1",
                    "step_description": "fetch",
                }),
                "verdict",
            ),
        ];

        for (args, missing) in cases {
            let result = handler
                .handle(McpToolCall {
                    name: "sentinel__submit_step_complete".into(),
                    arguments: args,
                })
                .await;
            assert!(!result.success, "expected failure when missing {missing}");
            assert!(
                result.error.as_deref().unwrap().contains(missing),
                "error must name the missing arg '{missing}', got: {:?}",
                result.error,
            );
        }
    }

    #[tokio::test]
    async fn submit_step_complete_rejects_unknown_judge_model() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = Arc::new(ProofEngine::new(state.clone(), Arc::new(StubJudge)));
        let handler = McpHandler::new(state, engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "fetch",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "judge_model": "bogus-model-name",
                }),
            })
            .await;

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("bogus-model-name"));
    }

    #[tokio::test]
    async fn submit_step_complete_chains_to_existing_proof() {
        // Two sequential submits via the MCP tool — second's previous_hash
        // must equal the first's combined_hash.
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = Arc::new(ProofEngine::new(state.clone(), Arc::new(StubJudge)));
        let handler = McpHandler::new(state.clone(), engine);

        let r1 = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "fetch ticket",
                    "verdict": {"sufficient": true, "confidence": 0.95, "reasoning": "ok"},
                }),
            })
            .await;
        assert!(r1.success);
        let combined_1 = r1
            .content
            .get("combined_hash")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();

        // Brief pause so step 2's started_at > step 1's completed_at.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let r2 = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "2",
                    "step_description": "create branch",
                    "verdict": {"sufficient": true, "confidence": 0.95, "reasoning": "ok"},
                }),
            })
            .await;
        assert!(r2.success);
        let prev_2 = r2
            .content
            .get("previous_hash")
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(
            prev_2, combined_1,
            "step 2 previous_hash must equal step 1 combined_hash via head_hash() resolution",
        );
    }

    #[tokio::test]
    async fn get_active_step_returns_null_last_step_when_chain_is_phase_only() {
        // Empty entries vec, no step proofs — last_step should be null.
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = Arc::new(ProofEngine::new(state.clone(), Arc::new(StubJudge)));
        let handler = McpHandler::new(state.clone(), engine);

        // Insert an empty chain manually (no proofs, no entries).
        {
            let mut s = state.write().await;
            s.proof_chains.insert(
                "phaseonly".to_string(),
                sentinel_domain::proof::ProofChain::new("phaseonly", "test-session"),
            );
        }

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_active_step".into(),
                arguments: serde_json::json!({"skill": "phaseonly"}),
            })
            .await;
        assert!(result.success);
        let payload = result.content;
        assert!(payload.get("last_step").unwrap().is_null());
        assert_eq!(payload.get("step_count").and_then(|v| v.as_u64()), Some(0));
    }
}
