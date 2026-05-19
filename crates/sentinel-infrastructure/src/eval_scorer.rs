//! A12 Phase 3d-2 — LLM-as-judge `EvalScorerPort` adapter.
//!
//! Mirrors [`crate::dry_run_auditor::RigAuditor`]: the scorer wraps a
//! `rig-core` client behind a uniform `PromptFn` seam, builds a
//! judge prompt that asks the model to rate 6 axes on the 0.0-1.0
//! scale, parses the response, and constructs an
//! [`EvalScore`](sentinel_domain::eval::EvalScore) honoring the
//! case's per-case [`ScoringRubric`](sentinel_domain::eval::ScoringRubric).
//!
//! ## Providers
//!
//! Selected by `SENTINEL_EVAL_SCORER_PROVIDER` (default `openrouter`):
//! - `openrouter` — `OPENROUTER_API_KEY` required; default model
//!   `anthropic/claude-opus-4.7` (override via
//!   `SENTINEL_EVAL_SCORER_MODEL`).
//! - `ollama` — auto-detects local vs cloud by `OLLAMA_API_KEY`
//!   presence; `SENTINEL_EVAL_SCORER_MODEL` required.
//!
//! The judge prompt is v1; operators iterate by changing the prompt
//! source (recompile) without touching the runner or use-case code.
//! The rubric weights live on the case so weight tuning needs no
//! adapter change.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use rig_core::agent::AgentBuilder;
use rig_core::completion::Prompt;
use rig_core::prelude::CompletionClient;
use rig_core::providers::{openai, openrouter};
use serde::Deserialize;
use tracing::{debug, warn};

use sentinel_domain::eval::{
    EvalAxis, EvalAxisScore, EvalCase, EvalRunId, EvalScore, ScoringRubric,
};
use sentinel_domain::ports::{EvalScorerError, EvalScorerPort};

pub const DEFAULT_SCORER_PROVIDER: &str = "openrouter";
pub const DEFAULT_SCORER_OPENROUTER_MODEL: &str = "anthropic/claude-opus-4.7";
pub const DEFAULT_SCORER_TIMEOUT: Duration = Duration::from_secs(60);
const OLLAMA_LOCAL_DUMMY_KEY: &str = "ollama-local";

type PromptFn = Arc<
    dyn Fn(String, String, String) -> BoxFuture<'static, anyhow::Result<String>>
        + Send
        + Sync,
>;

/// Rig-backed `EvalScorerPort` implementation.
pub struct LlmEvalScorer {
    prompt_fn: PromptFn,
    model_id: String,
    /// Provider-attribution prefix (`"openrouter"`, `"ollama-cloud"`,
    /// `"ollama-local"`). Not currently surfaced on `EvalScore` but
    /// kept for parity with `RigAuditor` and future telemetry.
    #[allow(dead_code)]
    provider_prefix: String,
    timeout: Duration,
}

impl std::fmt::Debug for LlmEvalScorer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmEvalScorer")
            .field("model_id", &self.model_id)
            .field("provider_prefix", &self.provider_prefix)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl LlmEvalScorer {
    /// Test-seam constructor: inject a stub `PromptFn` instead of
    /// hitting the network.
    #[must_use]
    pub fn with_prompt_fn(prompt_fn: PromptFn, model_id: impl Into<String>) -> Self {
        Self {
            prompt_fn,
            model_id: model_id.into(),
            provider_prefix: "openrouter".to_string(),
            timeout: DEFAULT_SCORER_TIMEOUT,
        }
    }

    #[must_use]
    pub fn with_provider_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.provider_prefix = prefix.into();
        self
    }

    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn from_env() -> Result<Self> {
        Self::from_env_with(real_env)
    }

    pub fn openrouter_from_env() -> Result<Self> {
        Self::openrouter_from_env_with(real_env)
    }

    pub fn ollama_from_env() -> Result<Self> {
        Self::ollama_from_env_with(real_env)
    }

    fn from_env_with<F>(env: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let provider = env("SENTINEL_EVAL_SCORER_PROVIDER")
            .unwrap_or_else(|| DEFAULT_SCORER_PROVIDER.to_string())
            .to_lowercase();
        match provider.as_str() {
            "openrouter" => Self::openrouter_from_env_with(env),
            "ollama" => Self::ollama_from_env_with(env),
            other => Err(anyhow::anyhow!(
                "unknown SENTINEL_EVAL_SCORER_PROVIDER={other:?}; expected one of: openrouter, ollama"
            )),
        }
    }

    fn openrouter_from_env_with<F>(env: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let key = env("OPENROUTER_API_KEY")
            .context("OPENROUTER_API_KEY not set (required for openrouter scorer)")?;
        let model_id = env("SENTINEL_EVAL_SCORER_MODEL")
            .unwrap_or_else(|| DEFAULT_SCORER_OPENROUTER_MODEL.to_string());
        let timeout = read_timeout(&env);

        let client = Arc::new(
            openrouter::Client::new(&key)
                .map_err(|e| anyhow::anyhow!("failed to build OpenRouter client: {e}"))?,
        );
        let prompt_fn: PromptFn = Arc::new(move |model_id, system, user_msg| {
            let client = client.clone();
            Box::pin(async move {
                let agent = AgentBuilder::new(client.completion_model(&model_id))
                    .preamble(&system)
                    .build();
                let result: anyhow::Result<String, _> = agent.prompt(user_msg).await;
                result.map_err(|e| anyhow::anyhow!("openrouter scorer ({model_id}): {e}"))
            })
        });
        Ok(Self {
            prompt_fn,
            model_id,
            provider_prefix: "openrouter".to_string(),
            timeout,
        })
    }

    fn ollama_from_env_with<F>(env: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let model_id = env("SENTINEL_EVAL_SCORER_MODEL").context(
            "SENTINEL_EVAL_SCORER_MODEL not set (required for ollama scorer; no sensible \
             default — pick what you've pulled, e.g. moonshotai/kimi-k2)",
        )?;
        let timeout = read_timeout(&env);

        let (base_url, api_key, provider_prefix) = env("OLLAMA_API_KEY").map_or_else(
            || {
                let host =
                    env("OLLAMA_HOST").unwrap_or_else(|| "http://localhost:11434".to_string());
                let base = format!("{}/v1", host.trim_end_matches('/'));
                (
                    base,
                    OLLAMA_LOCAL_DUMMY_KEY.to_string(),
                    "ollama-local".to_string(),
                )
            },
            |key| {
                let base = env("OLLAMA_BASE_URL")
                    .unwrap_or_else(|| "https://ollama.com/v1".to_string());
                (base, key, "ollama-cloud".to_string())
            },
        );

        let client = Arc::new(
            openai::Client::builder()
                .api_key(&api_key)
                .base_url(&base_url)
                .build()
                .map_err(|e| {
                    anyhow::anyhow!("failed to build ollama client (base_url={base_url}): {e}")
                })?,
        );
        let provider_for_prompt = provider_prefix.clone();
        let prompt_fn: PromptFn = Arc::new(move |model_id, system, user_msg| {
            let client = client.clone();
            let provider = provider_for_prompt.clone();
            Box::pin(async move {
                let agent = AgentBuilder::new(client.completion_model(&model_id))
                    .preamble(&system)
                    .build();
                let result: anyhow::Result<String, _> = agent.prompt(user_msg).await;
                result.map_err(|e| anyhow::anyhow!("{provider} scorer ({model_id}): {e}"))
            })
        });
        Ok(Self {
            prompt_fn,
            model_id,
            provider_prefix,
            timeout,
        })
    }
}

fn real_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn read_timeout<F>(env: &F) -> Duration
where
    F: Fn(&str) -> Option<String>,
{
    env("SENTINEL_EVAL_SCORER_TIMEOUT_SECS")
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(DEFAULT_SCORER_TIMEOUT, Duration::from_secs)
}

/// Module-local sidecar tokio runtime — same pattern as
/// [`crate::dry_run_auditor`]. Built lazily, reused across calls.
fn sidecar_runtime() -> Option<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<Option<tokio::runtime::Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("sentinel-eval-scorer-sidecar")
                .build()
                .map_err(|e| warn!(?e, "failed to build eval scorer sidecar runtime"))
                .ok()
        })
        .as_ref()
}

impl EvalScorerPort for LlmEvalScorer {
    fn score(
        &self,
        case: &EvalCase,
        candidate_output: &str,
        run_id: &EvalRunId,
    ) -> Result<EvalScore, EvalScorerError> {
        let system_prompt = build_system_prompt();
        let user_prompt = build_user_prompt(case, candidate_output);

        let runtime = sidecar_runtime().ok_or_else(|| {
            EvalScorerError::Configuration("eval scorer sidecar runtime unavailable".to_string())
        })?;

        let prompt_fn = self.prompt_fn.clone();
        let model_id = self.model_id.clone();
        let timeout = self.timeout;
        let response_text = runtime.block_on(async move {
            let call = prompt_fn(model_id, system_prompt, user_prompt);
            match tokio::time::timeout(timeout, call).await {
                Ok(Ok(text)) => Ok(text),
                Ok(Err(err)) => Err(EvalScorerError::Backend(format!("{err:#}"))),
                Err(_elapsed) => Err(EvalScorerError::Backend(format!(
                    "eval scorer timed out after {}s",
                    timeout.as_secs()
                ))),
            }
        })?;

        debug!(
            response_len = response_text.len(),
            "eval scorer returned"
        );
        parse_score(&response_text, case, run_id)
    }
}

fn build_system_prompt() -> String {
    r#"You are sentinel's BA-Eval scoring judge. You receive an
EvalCase (stakeholder brief + gold artifact) and a CANDIDATE OUTPUT
from a BA agent. Score the candidate on 6 axes, each 0.0-1.0, higher
is better.

Axes:
- citation_density_accuracy: every factual claim cited; citations
  match the source; right type of source for the claim.
- requirements_coverage: every gold-recommendation addressed or
  explicitly traded off; no recommendations untraceable to stated
  need.
- alternatives_seriousness: top-2 alternatives steelmanned, not
  strawmanned.
- tonal_calibration: confidence proportional to evidence; no spin;
  explicit uncertainty where warranted.
- outcome_realism: candidate's recommendation matches or
  substantively reasons about what actually happened (only
  meaningful when gold_outcomes is present + the source isn't
  synthetic; rate 0.5 with note "not scorable" when gold_outcomes is
  absent).
- stakeholder_fit: output shape matches the stated audience (exec /
  board / customer / internal team).

Return EXACTLY this JSON shape and NOTHING else (no markdown, no
prose before or after — the response is parsed verbatim):

{
  "axes": {
    "citation_density_accuracy": <float 0.0-1.0>,
    "requirements_coverage": <float 0.0-1.0>,
    "alternatives_seriousness": <float 0.0-1.0>,
    "tonal_calibration": <float 0.0-1.0>,
    "outcome_realism": <float 0.0-1.0>,
    "stakeholder_fit": <float 0.0-1.0>
  },
  "reasoning": "<one-paragraph operator-facing summary>"
}

Be honest about uncertainty: clamp axis scores to a tight range when
you can't tell (e.g. 0.4-0.6) rather than emitting confidently wrong
high or low scores."#
        .to_string()
}

fn build_user_prompt(case: &EvalCase, candidate_output: &str) -> String {
    serde_json::json!({
        "case_id": case.case_id.as_str(),
        "stakeholder_brief": case.stakeholder_brief,
        "gold_artifact": case.gold_artifact.as_ref().map(|g| serde_json::json!({
            "text": g.text,
            "author": g.author,
        })),
        "gold_outcomes_present": case.gold_outcomes.is_some(),
        "candidate_output": candidate_output,
    })
    .to_string()
}

fn parse_score(
    text: &str,
    case: &EvalCase,
    run_id: &EvalRunId,
) -> Result<EvalScore, EvalScorerError> {
    let cleaned = strip_code_fence(text);
    let raw: RawJudge = serde_json::from_str(&cleaned).map_err(|e| {
        EvalScorerError::Malformed(format!(
            "could not parse scorer JSON: {e} (response head: {head})",
            head = preview(&cleaned, 200),
        ))
    })?;
    let rubric = &case.scoring_rubric;
    let axis_scores = vec![
        axis_score(EvalAxis::CitationDensityAccuracy, raw.axes.citation_density_accuracy, rubric),
        axis_score(EvalAxis::RequirementsCoverage, raw.axes.requirements_coverage, rubric),
        axis_score(EvalAxis::AlternativesSeriousness, raw.axes.alternatives_seriousness, rubric),
        axis_score(EvalAxis::TonalCalibration, raw.axes.tonal_calibration, rubric),
        axis_score(EvalAxis::OutcomeRealism, raw.axes.outcome_realism, rubric),
        axis_score(EvalAxis::StakeholderFit, raw.axes.stakeholder_fit, rubric),
    ];
    Ok(EvalScore::new(
        case.case_id.clone(),
        run_id.clone(),
        axis_scores,
        rubric,
    ))
}

fn axis_score(axis: EvalAxis, raw: f32, rubric: &ScoringRubric) -> EvalAxisScore {
    EvalAxisScore::new(axis, raw, rubric.weight(axis))
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
struct RawJudge {
    axes: RawAxes,
    #[allow(dead_code)]
    #[serde(default)]
    reasoning: String,
}

#[derive(Debug, Deserialize)]
struct RawAxes {
    citation_density_accuracy: f32,
    requirements_coverage: f32,
    alternatives_seriousness: f32,
    tonal_calibration: f32,
    outcome_realism: f32,
    stakeholder_fit: f32,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::eval::{
        CaseProvenance, EvalCaseId, GoldArtifact, ScoringRubric, SourceCorpus,
    };

    fn make_case(id: &str) -> EvalCase {
        EvalCase {
            case_id: EvalCaseId::new(id).unwrap(),
            stakeholder_brief: "Help us decide pricing for new product".to_string(),
            source_corpus: SourceCorpus::Public {
                url: "https://example.com".to_string(),
                license: "CC-BY-4.0".to_string(),
            },
            gold_artifact: Some(GoldArtifact {
                text: "gold recommendation".to_string(),
                author: "tester".to_string(),
                content_hash: None,
            }),
            gold_outcomes: None,
            scoring_rubric: ScoringRubric::ba_default(),
            provenance: CaseProvenance {
                contributor: "tester".to_string(),
                license: "CC-BY-4.0".to_string(),
                is_private_test: false,
            },
        }
    }

    fn stub_returning(response: String) -> PromptFn {
        Arc::new(move |_model, _sys, _user| {
            let r = response.clone();
            Box::pin(async move { Ok(r) })
        })
    }

    fn stub_failing(err_msg: String) -> PromptFn {
        Arc::new(move |_model, _sys, _user| {
            let m = err_msg.clone();
            Box::pin(async move { Err(anyhow::anyhow!("{m}")) })
        })
    }

    #[test]
    fn score_with_well_formed_response_returns_eval_score() {
        let response = r#"{
            "axes": {
                "citation_density_accuracy": 0.8,
                "requirements_coverage": 0.7,
                "alternatives_seriousness": 0.6,
                "tonal_calibration": 0.85,
                "outcome_realism": 0.5,
                "stakeholder_fit": 0.9
            },
            "reasoning": "looked solid except outcome realism"
        }"#
        .to_string();
        let scorer = LlmEvalScorer::with_prompt_fn(stub_returning(response), "test-model");
        let case = make_case("c1");
        let run_id = EvalRunId::new("r1").unwrap();
        let score = scorer.score(&case, "candidate", &run_id).expect("should score");
        assert_eq!(score.axis_scores.len(), 6);
        let cit = score.for_axis(EvalAxis::CitationDensityAccuracy).unwrap();
        assert!((cit.raw - 0.8).abs() < 1e-3);
        assert!((cit.weight - 1.0).abs() < 1e-3);
        let outcome = score.for_axis(EvalAxis::OutcomeRealism).unwrap();
        assert!((outcome.weight - 2.0).abs() < 1e-3, "should pick up BA-default 2.0 weight");
    }

    #[test]
    fn score_strips_markdown_code_fence() {
        let response = "```json\n{\n  \"axes\": {\n    \"citation_density_accuracy\": 0.9,\n    \"requirements_coverage\": 0.9,\n    \"alternatives_seriousness\": 0.9,\n    \"tonal_calibration\": 0.9,\n    \"outcome_realism\": 0.9,\n    \"stakeholder_fit\": 0.9\n  },\n  \"reasoning\": \"all 0.9\"\n}\n```";
        let scorer =
            LlmEvalScorer::with_prompt_fn(stub_returning(response.to_string()), "test-model");
        let case = make_case("c1");
        let run_id = EvalRunId::new("r1").unwrap();
        let score = scorer.score(&case, "out", &run_id).expect("should parse fenced");
        assert_eq!(score.axis_scores.len(), 6);
    }

    #[test]
    fn score_with_backend_error_surfaces_backend_error() {
        let scorer = LlmEvalScorer::with_prompt_fn(
            stub_failing("network unreachable".to_string()),
            "test-model",
        );
        let case = make_case("c1");
        let run_id = EvalRunId::new("r1").unwrap();
        let err = scorer.score(&case, "out", &run_id).unwrap_err();
        assert!(matches!(err, EvalScorerError::Backend(_)), "got {err:?}");
        assert!(err.to_string().contains("network unreachable"), "got {err}");
    }

    #[test]
    fn score_with_malformed_json_surfaces_malformed_error() {
        let scorer = LlmEvalScorer::with_prompt_fn(
            stub_returning("not json at all".to_string()),
            "test-model",
        );
        let case = make_case("c1");
        let run_id = EvalRunId::new("r1").unwrap();
        let err = scorer.score(&case, "out", &run_id).unwrap_err();
        assert!(matches!(err, EvalScorerError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn score_with_missing_axis_in_response_surfaces_malformed_error() {
        // Missing outcome_realism field.
        let response = r#"{
            "axes": {
                "citation_density_accuracy": 0.8,
                "requirements_coverage": 0.7,
                "alternatives_seriousness": 0.6,
                "tonal_calibration": 0.85,
                "stakeholder_fit": 0.9
            },
            "reasoning": "missing one axis"
        }"#
        .to_string();
        let scorer = LlmEvalScorer::with_prompt_fn(stub_returning(response), "test-model");
        let case = make_case("c1");
        let run_id = EvalRunId::new("r1").unwrap();
        let err = scorer.score(&case, "out", &run_id).unwrap_err();
        assert!(matches!(err, EvalScorerError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn score_clamps_axis_values_to_valid_range() {
        // Judge returns 1.7 and -0.3; EvalAxisScore::new clamps to [0, 1].
        let response = r#"{
            "axes": {
                "citation_density_accuracy": 1.7,
                "requirements_coverage": -0.3,
                "alternatives_seriousness": 0.5,
                "tonal_calibration": 0.5,
                "outcome_realism": 0.5,
                "stakeholder_fit": 0.5
            },
            "reasoning": "out of range; should be clamped"
        }"#
        .to_string();
        let scorer = LlmEvalScorer::with_prompt_fn(stub_returning(response), "test-model");
        let case = make_case("c1");
        let run_id = EvalRunId::new("r1").unwrap();
        let score = scorer.score(&case, "out", &run_id).unwrap();
        let cit = score.for_axis(EvalAxis::CitationDensityAccuracy).unwrap();
        assert!((cit.raw - 1.0).abs() < 1e-3, "should clamp to 1.0; got {}", cit.raw);
        let req = score.for_axis(EvalAxis::RequirementsCoverage).unwrap();
        assert!((req.raw - 0.0).abs() < 1e-3, "should clamp to 0.0; got {}", req.raw);
    }

    #[test]
    fn score_composite_reflects_rubric_weights() {
        // All axes 0.5; BA-default rubric weights = (1,1,1,1,2,1) sum=7.
        // weighted_sum = 0.5*7 = 3.5; composite = 3.5 / 7.0 = 0.5.
        let response = r#"{
            "axes": {
                "citation_density_accuracy": 0.5,
                "requirements_coverage": 0.5,
                "alternatives_seriousness": 0.5,
                "tonal_calibration": 0.5,
                "outcome_realism": 0.5,
                "stakeholder_fit": 0.5
            },
            "reasoning": "uniform"
        }"#
        .to_string();
        let scorer = LlmEvalScorer::with_prompt_fn(stub_returning(response), "test-model");
        let case = make_case("c1");
        let run_id = EvalRunId::new("r1").unwrap();
        let score = scorer.score(&case, "out", &run_id).unwrap();
        assert!((score.composite - 0.5).abs() < 1e-3, "got {}", score.composite);
    }

    #[test]
    fn score_carries_run_id_and_case_id() {
        let response = r#"{
            "axes": {
                "citation_density_accuracy": 0.5,
                "requirements_coverage": 0.5,
                "alternatives_seriousness": 0.5,
                "tonal_calibration": 0.5,
                "outcome_realism": 0.5,
                "stakeholder_fit": 0.5
            },
            "reasoning": "ok"
        }"#
        .to_string();
        let scorer = LlmEvalScorer::with_prompt_fn(stub_returning(response), "test-model");
        let case = make_case("specific-case");
        let run_id = EvalRunId::new("specific-run").unwrap();
        let score = scorer.score(&case, "out", &run_id).unwrap();
        assert_eq!(score.case_id.as_str(), "specific-case");
        assert_eq!(score.run_id.as_str(), "specific-run");
    }

    #[test]
    fn from_env_unknown_provider_errors() {
        let result = LlmEvalScorer::from_env_with(|key| match key {
            "SENTINEL_EVAL_SCORER_PROVIDER" => Some("not-a-real-provider".to_string()),
            _ => None,
        });
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown SENTINEL_EVAL_SCORER_PROVIDER"));
    }

    #[test]
    fn openrouter_from_env_requires_api_key() {
        let result = LlmEvalScorer::openrouter_from_env_with(|_key| None);
        let err = result.unwrap_err();
        assert!(err.to_string().contains("OPENROUTER_API_KEY"));
    }

    #[test]
    fn ollama_from_env_requires_scorer_model() {
        let result = LlmEvalScorer::ollama_from_env_with(|_key| None);
        let err = result.unwrap_err();
        assert!(err.to_string().contains("SENTINEL_EVAL_SCORER_MODEL"));
    }

    #[test]
    fn read_timeout_falls_back_to_default_on_missing() {
        let env = |_: &str| None;
        let t = read_timeout(&env);
        assert_eq!(t, DEFAULT_SCORER_TIMEOUT);
    }

    #[test]
    fn read_timeout_parses_override() {
        let env = |k: &str| {
            if k == "SENTINEL_EVAL_SCORER_TIMEOUT_SECS" {
                Some("5".to_string())
            } else {
                None
            }
        };
        let t = read_timeout(&env);
        assert_eq!(t, Duration::from_secs(5));
    }

    #[test]
    fn preview_truncates_long_text() {
        let s = "x".repeat(500);
        let p = preview(&s, 100);
        assert_eq!(p.chars().count(), 103, "100 chars + '...'");
        assert!(p.ends_with("..."));
    }

    #[test]
    fn preview_passes_short_text_through() {
        let s = "short";
        let p = preview(s, 100);
        assert_eq!(p, "short");
    }
}
