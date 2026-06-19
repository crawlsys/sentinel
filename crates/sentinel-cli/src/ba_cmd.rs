//! `sentinel ba` — BA-orchestrator CLI surface.
//!
//! Phase 3 ships the `draft` subcommand.
//!
//! It takes a stakeholder brief plus audience, calls the
//! orchestrator's `draft()` use case, and prints the resulting
//! [`BaRecommendation`] as JSON. The orchestrator emits exactly
//! the envelope sentinel's BA1 / BA3 / A13 gates verify
//! downstream, so a `sentinel ba draft` invocation is the
//! end-to-end demo: operator types a brief, gets a structured,
//! gate-ready recommendation.
//!
//! ## What this does
//!
//! 1. Build an [`OpenRouterLlm`] from env (`OPENROUTER_API_KEY`).
//! 2. Construct a [`BaDraftRequest`] from CLI args.
//! 3. Call [`ba_orchestrator::draft`].
//! 4. Run the recommendation through the durable BA draft LangGraph.
//! 5. Print the recommendation and graph audit evidence.
//!
//! ## What this does NOT do
//!
//! - It does not run sentinel's hooks. Hooks fire when the
//!   recommendation is *used* downstream (serialized into a tool's
//!   `extra` payload). The CLI's job stops at emitting the
//!   envelope. The draft itself is still authorized by the BA draft
//!   LangGraph before the CLI/MCP surface returns it.

use anyhow::{Context, Result};
use serde::Serialize;

use sentinel_application::ba_orchestrator;
use sentinel_domain::ba::{BaDraftRequest, BaRecommendation, StakeholderAudience};
use sentinel_domain::ports::LlmPort;
use sentinel_infrastructure::openrouter_llm::OpenRouterLlm;

/// Arguments for `sentinel ba draft`.
pub struct DraftArgs {
    pub brief: String,
    pub audience: String,
    pub constraints: Vec<String>,
    pub agent_id: String,
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct BaDraftRunResult {
    pub workflow_authority: &'static str,
    pub recommendation: BaRecommendation,
    pub graph_audit: crate::ba_draft_graph::BaDraftGraphAudit,
}

/// `sentinel ba draft` — production entry point. Builds an
/// [`OpenRouterLlm`] from env and delegates to [`draft_with`].
pub async fn draft(args: DraftArgs) -> Result<()> {
    let llm =
        OpenRouterLlm::from_env().context("failed to build OpenRouter LLM from environment")?;
    draft_with(args, &llm).await
}

/// Test seam — accepts a pre-built [`LlmPort`] so tests can inject
/// a test double without env vars.
pub async fn draft_with<L>(args: DraftArgs, llm: &L) -> Result<()>
where
    L: LlmPort + ?Sized,
{
    let json = args.json;
    let result = draft_result_with(args, llm).await?;
    if json {
        render_json(&result);
    } else {
        render_summary(&result);
    }
    Ok(())
}

pub(crate) async fn draft_result_with<L>(args: DraftArgs, llm: &L) -> Result<BaDraftRunResult>
where
    L: LlmPort + ?Sized,
{
    let audience = parse_audience(&args.audience)?;
    let request = BaDraftRequest {
        brief: args.brief,
        stakeholder_audience: audience,
        constraints: args.constraints,
    };

    let recommendation = ba_orchestrator::draft(&request, llm, &args.agent_id, chrono::Utc::now)
        .await
        .map_err(|e| anyhow::anyhow!("orchestrator failed: {e}"))?;

    let graph_jsonl = sentinel_infrastructure::paths::sentinel_root()
        .join("metrics")
        .join("ba-draft")
        .join(format!(
            "{}.graph-runs.jsonl",
            recommendation.recommendation_id.as_str()
        ));
    let graph_audit =
        crate::ba_draft_graph::run_ba_draft_graph_audit(&recommendation, &graph_jsonl).await?;
    Ok(BaDraftRunResult {
        workflow_authority: "langgraph",
        recommendation,
        graph_audit,
    })
}

fn parse_audience(s: &str) -> Result<StakeholderAudience> {
    match s.to_lowercase().as_str() {
        "exec" => Ok(StakeholderAudience::Exec),
        "board" => Ok(StakeholderAudience::Board),
        "customer" => Ok(StakeholderAudience::Customer),
        "internal_team" | "internal-team" | "internal" => Ok(StakeholderAudience::InternalTeam),
        other => anyhow::bail!(
            "unknown audience {other:?}; expected one of: \
             exec, board, customer, internal_team"
        ),
    }
}

fn render_json(result: &BaDraftRunResult) {
    let out = serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".to_string());
    println!("{out}");
}

fn render_summary(result: &BaDraftRunResult) {
    let rec = &result.recommendation;
    println!(
        "Recommendation {} ({})",
        rec.recommendation_id.as_str(),
        rec.stakeholder_audience.key()
    );
    println!("  generated:  {}", rec.generated_at);
    println!("  agent:      {}", rec.agent_id);
    println!(
        "  citations:  {} ({} distinct)",
        rec.citations.len(),
        rec.distinct_citation_count()
    );
    println!("  requirement_refs: {}", rec.requirement_refs.len());
    println!(
        "  structurally ready: {}",
        rec.is_structurally_ready_for_publication()
    );
    println!(
        "  graph:     {} ({})",
        result.graph_audit.decision,
        result
            .graph_audit
            .authorization_checkpoint
            .as_deref()
            .unwrap_or("<missing checkpoint>")
    );
    println!(
        "  graph log: {}",
        result.graph_audit.graph_runs_path.display()
    );
    println!();
    println!("BRIEF:");
    println!("{}", rec.brief);
    println!();
    println!("BODY:");
    println!("{}", rec.body);
    if !rec.citations.is_empty() {
        println!();
        println!("CITATIONS:");
        for (i, c) in rec.citations.iter().enumerate() {
            println!(
                "  {}. {} ({:?}, retrieved {})",
                i + 1,
                c.artifact_id,
                c.provenance_class,
                c.retrieved_at,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use sentinel_domain::ports::{LlmPort, LlmRequest};
    use std::path::Path;

    struct StubLlm {
        response: String,
    }

    #[async_trait]
    impl LlmPort for StubLlm {
        async fn complete(
            &self,
            _request: LlmRequest,
        ) -> Result<String, sentinel_domain::port_errors::LlmError> {
            Ok(self.response.clone())
        }
    }

    struct SentinelEnvGuard {
        previous_home: Option<std::ffi::OsString>,
        previous_backend: Option<std::ffi::OsString>,
    }

    impl SentinelEnvGuard {
        fn set(path: &Path) -> Self {
            let previous_home = std::env::var_os("SENTINEL_HOME");
            let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
            std::env::set_var("SENTINEL_HOME", path);
            std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
            Self {
                previous_home,
                previous_backend,
            }
        }
    }

    impl Drop for SentinelEnvGuard {
        fn drop(&mut self) {
            match self.previous_home.take() {
                Some(value) => std::env::set_var("SENTINEL_HOME", value),
                None => std::env::remove_var("SENTINEL_HOME"),
            }
            match self.previous_backend.take() {
                Some(value) => std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", value),
                None => std::env::remove_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER"),
            }
        }
    }

    fn good_response() -> String {
        r#"{
            "body": "Recommend horizontal scaling.",
            "citations": [{
                "artifact_id": "linear://issue/FPCRM-42",
                "content_hash": "abc",
                "provenance_class": "SystemOfRecord",
                "retrieved_at": "2026-05-19T10:00:00Z"
            }],
            "requirement_refs": [{
                "orchestration_id": "orch-1",
                "matrix_row_id": "row-1",
                "content_hash": "h1",
                "statement": "stakeholder wants growth"
            }],
            "spec_challenge": {
                "work_id": "w-1",
                "agent_id": "ba",
                "challenged_spec": {"hash": "h", "source": "brief"},
                "reversibility_class": "Catastrophic",
                "assumptions": {"items": [{"statement": "x", "confidence": "Medium", "blast_if_wrong": "Irreversible"}], "explicit_assertion_of_none": null},
                "gaps": {"items": [{"topic": "x", "how_resolved": "OperatorClarified", "inference_source": null}], "explicit_assertion_of_none": null},
                "ambiguities": {"items": [{"spec_excerpt": "x", "interpretations": ["a","b"], "chosen": "a", "rationale": "r"}], "explicit_assertion_of_none": null},
                "alternatives_considered": {"items": [{"description": "x", "why_rejected": "y"}], "explicit_assertion_of_none": null},
                "constraints_not_satisfied": {"items": [], "explicit_assertion_of_none": "all met"},
                "created_at": "2026-05-19T10:00:00Z"
            }
        }"#.to_string()
    }

    #[tokio::test]
    async fn draft_with_well_formed_args_succeeds() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = SentinelEnvGuard::set(tmp.path());
        let llm = StubLlm {
            response: good_response(),
        };
        let args = DraftArgs {
            brief: "scale the platform".to_string(),
            audience: "exec".to_string(),
            constraints: vec![],
            agent_id: "ba".to_string(),
            json: true,
        };
        let result = draft_with(args, &llm).await;
        assert!(result.is_ok(), "got {result:?}");
    }

    #[tokio::test]
    async fn draft_summary_mode_succeeds() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = SentinelEnvGuard::set(tmp.path());
        let llm = StubLlm {
            response: good_response(),
        };
        let args = DraftArgs {
            brief: "scale".to_string(),
            audience: "board".to_string(),
            constraints: vec!["no PII".to_string()],
            agent_id: "ba".to_string(),
            json: false,
        };
        let result = draft_with(args, &llm).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn draft_result_returns_langgraph_audit() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = SentinelEnvGuard::set(tmp.path());
        let llm = StubLlm {
            response: good_response(),
        };
        let args = DraftArgs {
            brief: "scale".to_string(),
            audience: "exec".to_string(),
            constraints: vec![],
            agent_id: "ba".to_string(),
            json: true,
        };

        let result = draft_result_with(args, &llm).await.expect("draft result");

        assert_eq!(result.workflow_authority, "langgraph");
        assert_eq!(result.graph_audit.workflow_authority, "langgraph");
        assert_eq!(result.graph_audit.graph, "ba_draft");
        assert_eq!(result.graph_audit.decision, "high-risk-ready");
        assert!(result
            .graph_audit
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(result.graph_audit.run["topology"]["graph"], "ba_draft");
        assert!(std::fs::read_to_string(&result.graph_audit.graph_runs_path)
            .expect("graph jsonl")
            .contains("\"workflow_authority\":\"langgraph\""));
    }

    #[tokio::test]
    async fn draft_rejects_unknown_audience() {
        let llm = StubLlm {
            response: good_response(),
        };
        let args = DraftArgs {
            brief: "scale".to_string(),
            audience: "not-an-audience".to_string(),
            constraints: vec![],
            agent_id: "ba".to_string(),
            json: true,
        };
        let err = draft_with(args, &llm).await.unwrap_err();
        assert!(err.to_string().contains("unknown audience"));
    }

    #[tokio::test]
    async fn draft_rejects_blank_brief_through_orchestrator() {
        let llm = StubLlm {
            response: good_response(),
        };
        let args = DraftArgs {
            brief: "   ".to_string(),
            audience: "exec".to_string(),
            constraints: vec![],
            agent_id: "ba".to_string(),
            json: true,
        };
        let err = draft_with(args, &llm).await.unwrap_err();
        assert!(err.to_string().contains("orchestrator failed"));
    }

    #[test]
    fn parse_audience_accepts_canonical_keys() {
        assert_eq!(parse_audience("exec").unwrap(), StakeholderAudience::Exec);
        assert_eq!(parse_audience("board").unwrap(), StakeholderAudience::Board);
        assert_eq!(
            parse_audience("customer").unwrap(),
            StakeholderAudience::Customer
        );
        assert_eq!(
            parse_audience("internal_team").unwrap(),
            StakeholderAudience::InternalTeam
        );
    }

    #[test]
    fn parse_audience_is_case_insensitive() {
        assert_eq!(parse_audience("EXEC").unwrap(), StakeholderAudience::Exec);
        assert_eq!(parse_audience("Board").unwrap(), StakeholderAudience::Board);
    }

    #[test]
    fn parse_audience_accepts_internal_aliases() {
        assert_eq!(
            parse_audience("internal-team").unwrap(),
            StakeholderAudience::InternalTeam
        );
        assert_eq!(
            parse_audience("internal").unwrap(),
            StakeholderAudience::InternalTeam
        );
    }

    #[test]
    fn parse_audience_rejects_garbage() {
        let err = parse_audience("vibes").unwrap_err();
        assert!(err.to_string().contains("vibes"));
        assert!(err.to_string().contains("exec"));
    }
}
