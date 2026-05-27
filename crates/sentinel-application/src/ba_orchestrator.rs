//! BA-orchestrator use case (Phase 2).
//!
//! Given a [`BaDraftRequest`], produce a [`BaRecommendation`] —
//! the artifact sentinel's BA1 / BA3 / A13 gates verify
//! downstream. The orchestrator owns one task: turning a brief
//! into the structured envelope.
//!
//! ## How it works (MVP)
//!
//! One LLM call. The system prompt instructs the model to emit a
//! single JSON document with the recommendation body, citations,
//! requirement refs, and a complete A13 [`SpecChallenge`]. The
//! response is parsed strictly — missing or malformed fields
//! surface as [`BaOrchestratorError::Malformed`] rather than
//! silently degraded outputs.
//!
//! ## What the orchestrator does NOT do
//!
//! - It does NOT pull from connectors. The MVP relies on the model
//!   to emit plausible-looking citations; sentinel's BA1 gate
//!   verifies them against the audit chain. When connectors are
//!   wired in a future phase, real `ArtifactReference`s flow
//!   through here.
//! - It does NOT run the gates. Gates fire in `hook_cmd.rs` on
//!   downstream `PreToolUse` events when the recommendation is
//!   serialized into a tool's `extra` payload.
//! - It does NOT persist the recommendation. Persistence is a
//!   downstream concern (proof chain, run store, etc.).
//!
//! ## Reversibility-class mapping
//!
//! The audience drives the reversibility class on the emitted
//! [`SpecChallenge`]:
//!
//! | Audience | Class |
//! |---|---|
//! | `Exec`, `Board` | `Catastrophic` (will be scored by A13) |
//! | `Customer` | `Irreversible` |
//! | `InternalTeam` | `ReversibleWithEffort` |
//!
//! Operators can override per-call by post-processing the
//! returned [`BaRecommendation`].

use chrono::{DateTime, Utc};
use serde::Deserialize;

use sentinel_domain::ba::{
    ArtifactReference, BaDraftRequest, BaRecommendation, RecommendationId, RequirementRef,
    StakeholderAudience,
};
use sentinel_domain::ports::{LlmModel, LlmPort, LlmRequest};
use sentinel_domain::reversibility::ReversibilityClass;
use sentinel_domain::spec_challenge::SpecChallenge;

/// Errors the orchestrator can surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaOrchestratorError {
    /// Operator-supplied request didn't pass validation
    /// ([`BaDraftRequest::is_well_formed`] returned false).
    InvalidRequest(String),
    /// LLM call failed — network, rate limit, malformed upstream.
    Backend(String),
    /// LLM returned text that didn't parse as the expected JSON
    /// envelope. Includes a preview of the head of the response so
    /// operators can triage prompt-vs-model failures.
    Malformed(String),
}

impl std::fmt::Display for BaOrchestratorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(msg) => write!(f, "ba-orchestrator invalid request: {msg}"),
            Self::Backend(msg) => write!(f, "ba-orchestrator backend error: {msg}"),
            Self::Malformed(msg) => write!(f, "ba-orchestrator response malformed: {msg}"),
        }
    }
}

impl std::error::Error for BaOrchestratorError {}

/// Default per-call max tokens. The orchestrator's response
/// contains the recommendation body + citations + `requirement_refs`
/// + a structured `spec_challenge`; substantive responses run 2-4 KB.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Default model. Opus is the sensible BA-vertical default: dense
/// reasoning required to produce substantive `alternatives_considered`
/// + honest `constraints_not_satisfied`. Operators downshift to
///   Sonnet for cheaper non-Catastrophic work via [`draft_with_model`].
pub const DEFAULT_MODEL: LlmModel = LlmModel::Opus;

/// Produce a [`BaRecommendation`] from a request. Uses
/// [`DEFAULT_MODEL`] and [`DEFAULT_MAX_TOKENS`].
pub async fn draft<L>(
    request: &BaDraftRequest,
    llm: &L,
    agent_id: &str,
    clock: impl Fn() -> DateTime<Utc>,
) -> Result<BaRecommendation, BaOrchestratorError>
where
    L: LlmPort + ?Sized,
{
    draft_with_model(
        request,
        llm,
        agent_id,
        clock,
        DEFAULT_MODEL,
        DEFAULT_MAX_TOKENS,
    )
    .await
}

/// [`draft`] with an explicit model + max-token override.
pub async fn draft_with_model<L>(
    request: &BaDraftRequest,
    llm: &L,
    agent_id: &str,
    clock: impl Fn() -> DateTime<Utc>,
    model: LlmModel,
    max_tokens: u32,
) -> Result<BaRecommendation, BaOrchestratorError>
where
    L: LlmPort + ?Sized,
{
    if !request.is_well_formed() {
        return Err(BaOrchestratorError::InvalidRequest(
            "brief is empty or whitespace-only".to_string(),
        ));
    }

    let prompt = build_prompt(request);
    let llm_request = LlmRequest {
        model,
        prompt,
        max_tokens,
    };

    let response_text = llm
        .complete(llm_request)
        .await
        .map_err(|e| BaOrchestratorError::Backend(format!("{e:#}")))?;

    let raw = parse_raw(&response_text)?;
    let generated_at = clock();
    let recommendation_id = RecommendationId::new(format!("rec-{}", generated_at.timestamp()))
        .map_err(|e| BaOrchestratorError::Malformed(format!("synthesized id rejected: {e}")))?;

    Ok(BaRecommendation {
        recommendation_id,
        brief: request.brief.clone(),
        stakeholder_audience: request.stakeholder_audience,
        body: raw.body,
        citations: raw.citations,
        requirement_refs: raw.requirement_refs,
        spec_challenge: raw.spec_challenge,
        generated_at,
        agent_id: agent_id.to_string(),
    })
}

/// Map audience → reversibility class for the embedded spec
/// challenge. Public so operators iterating on the convention can
/// override by post-processing the result.
#[must_use]
pub const fn reversibility_for_audience(audience: StakeholderAudience) -> ReversibilityClass {
    match audience {
        StakeholderAudience::Exec | StakeholderAudience::Board => ReversibilityClass::Catastrophic,
        StakeholderAudience::Customer => ReversibilityClass::Irreversible,
        StakeholderAudience::InternalTeam => ReversibilityClass::ReversibleWithEffort,
    }
}

fn build_prompt(request: &BaDraftRequest) -> String {
    let constraints = if request.constraints.is_empty() {
        "(none stated)".to_string()
    } else {
        request
            .constraints
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}. {c}", i + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let expected_class = reversibility_for_audience(request.stakeholder_audience);

    format!(
        "You are a senior Business Analyst. Produce a recommendation \
         for the stated audience. Before drafting, run an A13 \
         spec-challenge: articulate your assumptions, gaps, \
         ambiguities, alternatives_considered, and \
         constraints_not_satisfied.\n\n\
         STAKEHOLDER BRIEF:\n{brief}\n\n\
         AUDIENCE: {audience}\n\n\
         OPERATOR CONSTRAINTS:\n{constraints}\n\n\
         REVERSIBILITY CLASS: {class:?}\n\n\
         Return EXACTLY this JSON shape and NOTHING else (no markdown, \
         no prose before or after — the response is parsed verbatim):\n\n\
         {{\n  \
         \"body\": \"<the recommendation body, plain prose, markdown allowed>\",\n  \
         \"citations\": [\n    {{\n      \
         \"artifact_id\": \"<connector://source-id>\",\n      \
         \"content_hash\": \"<sha256 hex>\",\n      \
         \"provenance_class\": \"SystemOfRecord\" | \"Interview\" | \"Inference\" | \"ExternalApi\",\n      \
         \"retrieved_at\": \"<RFC3339 timestamp>\"\n    }}\n  ],\n  \
         \"requirement_refs\": [\n    {{\n      \
         \"orchestration_id\": \"<this orchestration's id>\",\n      \
         \"matrix_row_id\": \"<row id within the matrix>\",\n      \
         \"content_hash\": \"<sha256 hex of the requirement statement>\",\n      \
         \"statement\": \"<the requirement text>\"\n    }}\n  ],\n  \
         \"spec_challenge\": {{\n    \
         \"work_id\": \"<stable id for this work unit>\",\n    \
         \"agent_id\": \"<your agent id>\",\n    \
         \"challenged_spec\": {{ \"hash\": \"<sha256 hex of the brief>\", \"source\": \"<source label>\" }},\n    \
         \"reversibility_class\": \"{class:?}\",\n    \
         \"assumptions\": {{ \"items\": [{{ \"statement\": \"...\", \"confidence\": \"Low\"|\"Medium\"|\"High\", \"blast_if_wrong\": \"<class>\" }}], \"explicit_assertion_of_none\": null }},\n    \
         \"gaps\": {{ \"items\": [{{ \"topic\": \"...\", \"how_resolved\": \"OperatorClarified\"|\"InferredFromContext\"|\"DefaultApplied\", \"inference_source\": null }}], \"explicit_assertion_of_none\": null }},\n    \
         \"ambiguities\": {{ \"items\": [{{ \"spec_excerpt\": \"...\", \"interpretations\": [\"a\", \"b\"], \"chosen\": \"a\", \"rationale\": \"...\" }}], \"explicit_assertion_of_none\": null }},\n    \
         \"alternatives_considered\": {{ \"items\": [{{ \"description\": \"...\", \"why_rejected\": \"...\" }}], \"explicit_assertion_of_none\": null }},\n    \
         \"constraints_not_satisfied\": {{ \"items\": [], \"explicit_assertion_of_none\": \"all stated constraints satisfied\" }},\n    \
         \"created_at\": \"<RFC3339>\"\n  }}\n}}\n\n\
         Rules:\n\
         - Every claim in `body` must have a corresponding `citation`.\n\
         - Every recommendation must trace to a `requirement_ref`.\n\
         - Each category in `spec_challenge` MUST have either items \
         OR an `explicit_assertion_of_none` reason — silent empties \
         are rejected by sentinel's A13 gate.\n\
         - Each ambiguity must have ≥ 2 interpretations.\n\
         - If `how_resolved` is `InferredFromContext`, \
         `inference_source` MUST be a non-empty string.",
        brief = request.brief,
        audience = request.stakeholder_audience.key(),
        constraints = constraints,
        class = expected_class,
    )
}

fn parse_raw(text: &str) -> Result<RawRecommendation, BaOrchestratorError> {
    let cleaned = strip_code_fence(text);
    serde_json::from_str::<RawRecommendation>(&cleaned).map_err(|e| {
        BaOrchestratorError::Malformed(format!(
            "could not parse orchestrator JSON: {e} (response head: {head})",
            head = preview(&cleaned, 200),
        ))
    })
}

fn strip_code_fence(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        return rest.trim_end_matches("```").trim().to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        return rest.trim_end_matches("```").trim().to_string();
    }
    trimmed.to_string()
}

fn preview(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_chars).collect();
        format!("{truncated}...")
    }
}

#[derive(Debug, Deserialize)]
struct RawRecommendation {
    body: String,
    citations: Vec<ArtifactReference>,
    requirement_refs: Vec<RequirementRef>,
    spec_challenge: SpecChallenge,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::TimeZone;
    use std::sync::Mutex;

    /// Test LLM: returns a canned response and records the last
    /// request for assertions.
    struct StubLlm {
        response: Mutex<Result<String, String>>,
        last_request: Mutex<Option<LlmRequest>>,
    }

    impl StubLlm {
        fn returning(text: &str) -> Self {
            Self {
                response: Mutex::new(Ok(text.to_string())),
                last_request: Mutex::new(None),
            }
        }
        fn failing(msg: &str) -> Self {
            Self {
                response: Mutex::new(Err(msg.to_string())),
                last_request: Mutex::new(None),
            }
        }
        fn last_prompt(&self) -> Option<String> {
            self.last_request.lock().unwrap().clone().map(|r| r.prompt)
        }
    }

    #[async_trait]
    impl LlmPort for StubLlm {
        async fn complete(&self, request: LlmRequest) -> anyhow::Result<String> {
            *self.last_request.lock().unwrap() = Some(request);
            let result = self.response.lock().unwrap().clone();
            match result {
                Ok(text) => Ok(text),
                Err(msg) => Err(anyhow::anyhow!("{msg}")),
            }
        }
    }

    fn fixed_clock() -> impl Fn() -> DateTime<Utc> {
        || Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    fn well_formed_response() -> String {
        r#"{
            "body": "Recommend scaling horizontally. The Linear roadmap signals user growth.",
            "citations": [
                {
                    "artifact_id": "linear://issue/FPCRM-42",
                    "content_hash": "abc123",
                    "provenance_class": "SystemOfRecord",
                    "retrieved_at": "2026-05-19T10:00:00Z"
                }
            ],
            "requirement_refs": [
                {
                    "orchestration_id": "orch-1",
                    "matrix_row_id": "row-1",
                    "content_hash": "hashreq1",
                    "statement": "Stakeholder wants p99 < 100ms under 10x users"
                }
            ],
            "spec_challenge": {
                "work_id": "w-1",
                "agent_id": "ba-orchestrator",
                "challenged_spec": {"hash": "briefhash", "source": "stakeholder brief"},
                "reversibility_class": "Catastrophic",
                "assumptions": {
                    "items": [{
                        "statement": "stakeholder wants user-visible latency, not throughput",
                        "confidence": "Medium",
                        "blast_if_wrong": "Irreversible"
                    }],
                    "explicit_assertion_of_none": null
                },
                "gaps": {
                    "items": [{
                        "topic": "budget",
                        "how_resolved": "OperatorClarified",
                        "inference_source": null
                    }],
                    "explicit_assertion_of_none": null
                },
                "ambiguities": {
                    "items": [{
                        "spec_excerpt": "scale up",
                        "interpretations": ["10x traffic", "10x users"],
                        "chosen": "10x users",
                        "rationale": "context implies user growth"
                    }],
                    "explicit_assertion_of_none": null
                },
                "alternatives_considered": {
                    "items": [{
                        "description": "vertical scaling",
                        "why_rejected": "operator ruled out"
                    }],
                    "explicit_assertion_of_none": null
                },
                "constraints_not_satisfied": {
                    "items": [],
                    "explicit_assertion_of_none": "all constraints satisfied"
                },
                "created_at": "2026-05-19T10:00:00Z"
            }
        }"#
        .to_string()
    }

    fn well_formed_request() -> BaDraftRequest {
        BaDraftRequest {
            brief: "scale the platform".to_string(),
            stakeholder_audience: StakeholderAudience::Exec,
            constraints: vec!["no PII".to_string()],
        }
    }

    #[tokio::test]
    async fn draft_returns_recommendation_for_well_formed_request() {
        let llm = StubLlm::returning(&well_formed_response());
        let rec = draft(
            &well_formed_request(),
            &llm,
            "ba-orchestrator",
            fixed_clock(),
        )
        .await
        .expect("should draft");
        assert_eq!(rec.brief, "scale the platform");
        assert_eq!(rec.agent_id, "ba-orchestrator");
        assert_eq!(rec.stakeholder_audience, StakeholderAudience::Exec);
        assert_eq!(rec.citations.len(), 1);
        assert_eq!(rec.requirement_refs.len(), 1);
        assert!(rec.is_structurally_ready_for_publication());
    }

    #[tokio::test]
    async fn draft_rejects_blank_brief() {
        let llm = StubLlm::returning(&well_formed_response());
        let request = BaDraftRequest {
            brief: "   ".to_string(),
            stakeholder_audience: StakeholderAudience::Exec,
            constraints: vec![],
        };
        let err = draft(&request, &llm, "ba-orchestrator", fixed_clock())
            .await
            .unwrap_err();
        assert!(matches!(err, BaOrchestratorError::InvalidRequest(_)));
        // LLM should NOT have been called for an invalid request.
        assert!(llm.last_prompt().is_none());
    }

    #[tokio::test]
    async fn draft_surfaces_backend_error_when_llm_fails() {
        let llm = StubLlm::failing("rate limit");
        let err = draft(&well_formed_request(), &llm, "ba", fixed_clock())
            .await
            .unwrap_err();
        assert!(matches!(err, BaOrchestratorError::Backend(_)));
        assert!(err.to_string().contains("rate limit"));
    }

    #[tokio::test]
    async fn draft_surfaces_malformed_error_on_non_json_response() {
        let llm = StubLlm::returning("not json at all");
        let err = draft(&well_formed_request(), &llm, "ba", fixed_clock())
            .await
            .unwrap_err();
        assert!(matches!(err, BaOrchestratorError::Malformed(_)));
    }

    #[tokio::test]
    async fn draft_strips_markdown_code_fence() {
        let fenced = format!("```json\n{}\n```", well_formed_response());
        let llm = StubLlm::returning(&fenced);
        let rec = draft(&well_formed_request(), &llm, "ba", fixed_clock())
            .await
            .expect("should parse fenced");
        assert!(rec.is_structurally_ready_for_publication());
    }

    #[tokio::test]
    async fn prompt_carries_brief_and_audience_and_constraints() {
        let llm = StubLlm::returning(&well_formed_response());
        let request = BaDraftRequest {
            brief: "specific brief text here".to_string(),
            stakeholder_audience: StakeholderAudience::Board,
            constraints: vec![
                "no vendor lockin".to_string(),
                "cite ≥ 2 sources".to_string(),
            ],
        };
        let _ = draft(&request, &llm, "ba", fixed_clock()).await;
        let prompt = llm.last_prompt().unwrap();
        assert!(prompt.contains("specific brief text here"));
        assert!(prompt.contains("board"));
        assert!(prompt.contains("no vendor lockin"));
        assert!(prompt.contains("cite ≥ 2 sources"));
        assert!(prompt.contains("Catastrophic"), "Board → Catastrophic");
    }

    #[tokio::test]
    async fn prompt_handles_empty_constraints() {
        let llm = StubLlm::returning(&well_formed_response());
        let request = BaDraftRequest {
            brief: "brief".to_string(),
            stakeholder_audience: StakeholderAudience::Exec,
            constraints: vec![],
        };
        let _ = draft(&request, &llm, "ba", fixed_clock()).await;
        let prompt = llm.last_prompt().unwrap();
        assert!(prompt.contains("(none stated)"));
    }

    #[tokio::test]
    async fn draft_synthesizes_recommendation_id_from_clock() {
        let llm = StubLlm::returning(&well_formed_response());
        let rec = draft(&well_formed_request(), &llm, "ba", fixed_clock())
            .await
            .unwrap();
        // Clock returns 1_700_000_000.
        assert_eq!(rec.recommendation_id.as_str(), "rec-1700000000");
    }

    #[test]
    fn reversibility_for_audience_maps_correctly() {
        assert_eq!(
            reversibility_for_audience(StakeholderAudience::Exec),
            ReversibilityClass::Catastrophic
        );
        assert_eq!(
            reversibility_for_audience(StakeholderAudience::Board),
            ReversibilityClass::Catastrophic
        );
        assert_eq!(
            reversibility_for_audience(StakeholderAudience::Customer),
            ReversibilityClass::Irreversible
        );
        assert_eq!(
            reversibility_for_audience(StakeholderAudience::InternalTeam),
            ReversibilityClass::ReversibleWithEffort
        );
    }

    #[test]
    fn error_display_names_each_variant() {
        assert!(BaOrchestratorError::InvalidRequest("x".into())
            .to_string()
            .contains("invalid request"));
        assert!(BaOrchestratorError::Backend("x".into())
            .to_string()
            .contains("backend error"));
        assert!(BaOrchestratorError::Malformed("x".into())
            .to_string()
            .contains("malformed"));
    }

    #[test]
    fn error_implements_std_error() {
        fn assert_error<E: std::error::Error>() {}
        assert_error::<BaOrchestratorError>();
    }

    #[test]
    fn preview_truncates_long_text() {
        let s = "x".repeat(500);
        let p = preview(&s, 50);
        assert_eq!(p.chars().count(), 53);
        assert!(p.ends_with("..."));
    }

    #[test]
    fn preview_passes_short_text_through() {
        assert_eq!(preview("short", 100), "short");
    }
}
