//! A13 Phase 4b — LLM-as-judge `SpecChallengeScorerPort` adapter.
//!
//! Mirrors [`crate::eval_scorer::LlmEvalScorer`]: wraps a `rig-core`
//! client behind a uniform `PromptFn` seam, builds a judge prompt
//! that asks the model to rate the 5 categories on `[0.0, 1.0]`,
//! parses the JSON response into a [`SpecChallengeScore`].
//!
//! ## Providers
//!
//! Selected by `SENTINEL_SPEC_CHALLENGE_SCORER_PROVIDER` (default
//! `openrouter`). Env namespace is distinct from A3 / A12 so
//! operators can run separate models for dry-run auditing,
//! eval scoring, and spec-challenge scoring in the same session:
//!
//! - `openrouter` — `OPENROUTER_API_KEY` required;
//!   `SENTINEL_SPEC_CHALLENGE_SCORER_MODEL` (default
//!   `anthropic/claude-opus-4.7`).
//! - `ollama` — auto-detects local vs cloud by `OLLAMA_API_KEY`
//!   presence; `SENTINEL_SPEC_CHALLENGE_SCORER_MODEL` required.
//!
//! ## What the judge scores
//!
//! NOT completeness — that's the deterministic floor handled by
//! [`SpecChallenge::completeness_findings`]. The judge scores
//! **semantic quality**: are the assumptions substantive or vague
//! handwaves, are the gaps real or trivial, are ambiguities
//! genuine forks or fake-for-show, are alternatives steelmanned
//! or strawmanned, are the unsatisfied constraints substantive
//! or filler. The hook layer (Phase 3) gates on the deterministic
//! check + (for Catastrophic class) every axis above the operator-
//! configured threshold.

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

use sentinel_domain::ports::{
    SpecChallengeScore, SpecChallengeScorerError, SpecChallengeScorerPort,
};
use sentinel_domain::spec_challenge::SpecChallenge;

pub const DEFAULT_SCORER_PROVIDER: &str = "openrouter";
pub const DEFAULT_SCORER_OPENROUTER_MODEL: &str = "anthropic/claude-opus-4.7";
pub const DEFAULT_SCORER_TIMEOUT: Duration = Duration::from_secs(60);
const OLLAMA_LOCAL_DUMMY_KEY: &str = "ollama-local";

type PromptFn = Arc<
    dyn Fn(String, String, String) -> BoxFuture<'static, anyhow::Result<String>>
        + Send
        + Sync,
>;

/// Rig-backed `SpecChallengeScorerPort` implementation.
pub struct LlmSpecChallengeScorer {
    prompt_fn: PromptFn,
    model_id: String,
    #[allow(dead_code)]
    provider_prefix: String,
    timeout: Duration,
}

impl std::fmt::Debug for LlmSpecChallengeScorer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmSpecChallengeScorer")
            .field("model_id", &self.model_id)
            .field("provider_prefix", &self.provider_prefix)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl LlmSpecChallengeScorer {
    /// Test-seam constructor.
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
        let provider = env("SENTINEL_SPEC_CHALLENGE_SCORER_PROVIDER")
            .unwrap_or_else(|| DEFAULT_SCORER_PROVIDER.to_string())
            .to_lowercase();
        match provider.as_str() {
            "openrouter" => Self::openrouter_from_env_with(env),
            "ollama" => Self::ollama_from_env_with(env),
            other => Err(anyhow::anyhow!(
                "unknown SENTINEL_SPEC_CHALLENGE_SCORER_PROVIDER={other:?}; \
                 expected one of: openrouter, ollama"
            )),
        }
    }

    fn openrouter_from_env_with<F>(env: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let key = env("OPENROUTER_API_KEY").context(
            "OPENROUTER_API_KEY not set (required for openrouter spec-challenge scorer)",
        )?;
        let model_id = env("SENTINEL_SPEC_CHALLENGE_SCORER_MODEL")
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
                result.map_err(|e| {
                    anyhow::anyhow!("openrouter spec-challenge scorer ({model_id}): {e}")
                })
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
        let model_id = env("SENTINEL_SPEC_CHALLENGE_SCORER_MODEL").context(
            "SENTINEL_SPEC_CHALLENGE_SCORER_MODEL not set (required for ollama scorer; \
             no sensible default — pick what you've pulled)",
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
                result.map_err(|e| {
                    anyhow::anyhow!("{provider} spec-challenge scorer ({model_id}): {e}")
                })
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
    env("SENTINEL_SPEC_CHALLENGE_SCORER_TIMEOUT_SECS")
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(DEFAULT_SCORER_TIMEOUT, Duration::from_secs)
}

fn sidecar_runtime() -> Option<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<Option<tokio::runtime::Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("sentinel-spec-challenge-scorer-sidecar")
                .build()
                .map_err(|e| warn!(?e, "failed to build spec-challenge scorer sidecar runtime"))
                .ok()
        })
        .as_ref()
}

impl SpecChallengeScorerPort for LlmSpecChallengeScorer {
    fn score(
        &self,
        challenge: &SpecChallenge,
    ) -> Result<SpecChallengeScore, SpecChallengeScorerError> {
        let system_prompt = build_system_prompt();
        let user_prompt = build_user_prompt(challenge);

        let runtime = sidecar_runtime().ok_or_else(|| {
            SpecChallengeScorerError::Configuration(
                "spec-challenge scorer sidecar runtime unavailable".to_string(),
            )
        })?;

        let prompt_fn = self.prompt_fn.clone();
        let model_id = self.model_id.clone();
        let timeout = self.timeout;
        // Drive the blocking call on a dedicated thread (outside any tokio
        // worker) so `block_on` never trips the "runtime within a runtime"
        // panic when `score` is reached from inside the CLI's #[tokio::main]
        // runtime. The work runs on the shared sidecar runtime via its Handle.
        let handle = runtime.handle().clone();
        let response_text = std::thread::scope(|s| {
            s.spawn(move || {
                handle.block_on(async move {
                    let call = prompt_fn(model_id, system_prompt, user_prompt);
                    match tokio::time::timeout(timeout, call).await {
                        Ok(Ok(text)) => Ok(text),
                        Ok(Err(err)) => Err(SpecChallengeScorerError::Backend(format!("{err:#}"))),
                        Err(_elapsed) => Err(SpecChallengeScorerError::Backend(format!(
                            "spec-challenge scorer timed out after {}s",
                            timeout.as_secs()
                        ))),
                    }
                })
            })
            .join()
            .unwrap_or_else(|_| {
                Err(SpecChallengeScorerError::Backend(
                    "spec-challenge scorer worker thread panicked".to_string(),
                ))
            })
        })?;

        debug!(
            response_len = response_text.len(),
            "spec-challenge scorer returned"
        );
        parse_score(&response_text)
    }
}

fn build_system_prompt() -> String {
    r#"You are sentinel's A13 spec-challenge judge. You receive a
SpecChallenge artifact: an agent's pre-action self-examination
across 5 categories. Score the SEMANTIC QUALITY of each category
on 0.0-1.0 (higher is better). Completeness (every category
filled) is checked deterministically elsewhere — your job is to
rate whether each category's CONTENT is substantive.

Categories and what "high score" looks like:

- assumptions: each Assumption names a specific factual claim
  (not "things might work"), has plausible confidence calibration
  (Low/Medium/High matches the evidence), and the blast_if_wrong
  matches reality. Vague handwaves score low.

- gaps: each SpecGap names a real missing piece of information
  (not trivial details). InferredFromContext resolutions have
  substantive inference_source. OperatorClarified gaps were
  actually clarified (not phantom clarifications).

- ambiguities: each Ambiguity has ≥ 2 GENUINELY DIFFERENT
  interpretations (not "interpretation 1" / "almost the same
  interpretation 1"). The chosen interpretation has substantive
  rationale.

- alternatives_considered: each Alternative is STEELMANNED in
  description (not strawmanned to make rejection easy). The
  why_rejected is substantive engineering reasoning.

- constraints_not_satisfied: each entry is a real constraint the
  approach fails to meet (not made-up filler to satisfy this
  category). The why_not_satisfiable is honest. Workaround
  proposals are concrete when present.

Return EXACTLY this JSON shape and NOTHING else (no markdown, no
prose before or after — the response is parsed verbatim):

{
  "axes": {
    "assumptions": <float 0.0-1.0>,
    "gaps": <float 0.0-1.0>,
    "ambiguities": <float 0.0-1.0>,
    "alternatives_considered": <float 0.0-1.0>,
    "constraints_not_satisfied": <float 0.0-1.0>
  },
  "reasoning": "<one-paragraph operator-facing summary>"
}

When a category is `explicit_assertion_of_none` (the items list is
empty with a stated reason), score 0.7 if the reason is plausible
and the work doesn't obviously call for items in that category;
score 0.4 if the reason looks like a dodge. Be honest about
uncertainty: keep scores in `[0.4, 0.6]` when you can't tell
rather than emitting confidently wrong high or low values."#
        .to_string()
}

fn build_user_prompt(challenge: &SpecChallenge) -> String {
    serde_json::to_string(challenge).unwrap_or_else(|_| "{}".to_string())
}

fn parse_score(text: &str) -> Result<SpecChallengeScore, SpecChallengeScorerError> {
    let cleaned = strip_code_fence(text);
    let raw: RawJudge = serde_json::from_str(&cleaned).map_err(|e| {
        SpecChallengeScorerError::Malformed(format!(
            "could not parse scorer JSON: {e} (response head: {head})",
            head = preview(&cleaned, 200),
        ))
    })?;
    Ok(SpecChallengeScore::new(
        raw.axes.assumptions,
        raw.axes.gaps,
        raw.axes.ambiguities,
        raw.axes.alternatives_considered,
        raw.axes.constraints_not_satisfied,
    ))
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
    assumptions: f32,
    gaps: f32,
    ambiguities: f32,
    alternatives_considered: f32,
    constraints_not_satisfied: f32,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use sentinel_domain::reversibility::ReversibilityClass;
    use sentinel_domain::spec_challenge::{
        Alternative, Ambiguity, Assumption, AssumptionConfidence, ChallengeCategory,
        GapResolution, SpecChallenge, SpecGap, SpecReference, WorkId,
    };

    fn make_challenge() -> SpecChallenge {
        SpecChallenge {
            work_id: WorkId::new("w1").unwrap(),
            agent_id: "agent-x".to_string(),
            challenged_spec: SpecReference {
                hash: "abc".to_string(),
                source: "issue X".to_string(),
            },
            reversibility_class: ReversibilityClass::Catastrophic,
            assumptions: ChallengeCategory::new(vec![Assumption {
                statement: "postgres 15 is available".to_string(),
                confidence: AssumptionConfidence::High,
                blast_if_wrong: ReversibilityClass::Irreversible,
            }]),
            gaps: ChallengeCategory::new(vec![SpecGap {
                topic: "auth method".to_string(),
                how_resolved: GapResolution::OperatorClarified,
                inference_source: None,
            }]),
            ambiguities: ChallengeCategory::new(vec![Ambiguity {
                spec_excerpt: "ship fast".to_string(),
                interpretations: vec!["p99".to_string(), "throughput".to_string()],
                chosen: "p99".to_string(),
                rationale: "user-visible".to_string(),
            }]),
            alternatives_considered: ChallengeCategory::new(vec![Alternative {
                description: "use Redis for the queue".to_string(),
                why_rejected: "durability story too weak".to_string(),
            }]),
            constraints_not_satisfied: ChallengeCategory::none("all met"),
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
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
    fn score_with_well_formed_response_returns_spec_challenge_score() {
        let response = r#"{
            "axes": {
                "assumptions": 0.8,
                "gaps": 0.7,
                "ambiguities": 0.6,
                "alternatives_considered": 0.85,
                "constraints_not_satisfied": 0.9
            },
            "reasoning": "solid analysis"
        }"#
        .to_string();
        let scorer =
            LlmSpecChallengeScorer::with_prompt_fn(stub_returning(response), "test-model");
        let challenge = make_challenge();
        let score = scorer.score(&challenge).expect("should score");
        assert!((score.assumptions - 0.8).abs() < 1e-3);
        assert!((score.gaps - 0.7).abs() < 1e-3);
        assert!((score.ambiguities - 0.6).abs() < 1e-3);
        assert!((score.alternatives_considered - 0.85).abs() < 1e-3);
        assert!((score.constraints_not_satisfied - 0.9).abs() < 1e-3);
    }

    #[test]
    fn score_strips_markdown_code_fence() {
        let response = "```json\n{\n  \"axes\": {\n    \"assumptions\": 0.9,\n    \"gaps\": 0.9,\n    \"ambiguities\": 0.9,\n    \"alternatives_considered\": 0.9,\n    \"constraints_not_satisfied\": 0.9\n  },\n  \"reasoning\": \"all 0.9\"\n}\n```";
        let scorer = LlmSpecChallengeScorer::with_prompt_fn(
            stub_returning(response.to_string()),
            "test-model",
        );
        let challenge = make_challenge();
        let score = scorer.score(&challenge).expect("should parse fenced");
        assert!((score.assumptions - 0.9).abs() < 1e-3);
    }

    #[test]
    fn score_with_backend_error_surfaces_backend_error() {
        let scorer = LlmSpecChallengeScorer::with_prompt_fn(
            stub_failing("network unreachable".to_string()),
            "test-model",
        );
        let challenge = make_challenge();
        let err = scorer.score(&challenge).unwrap_err();
        assert!(matches!(err, SpecChallengeScorerError::Backend(_)));
        assert!(err.to_string().contains("network unreachable"));
    }

    #[test]
    fn score_with_malformed_json_surfaces_malformed_error() {
        let scorer = LlmSpecChallengeScorer::with_prompt_fn(
            stub_returning("not json at all".to_string()),
            "test-model",
        );
        let challenge = make_challenge();
        let err = scorer.score(&challenge).unwrap_err();
        assert!(matches!(err, SpecChallengeScorerError::Malformed(_)));
    }

    #[test]
    fn score_with_missing_axis_surfaces_malformed_error() {
        let response = r#"{
            "axes": {
                "assumptions": 0.8,
                "gaps": 0.7
            },
            "reasoning": "missing axes"
        }"#
        .to_string();
        let scorer =
            LlmSpecChallengeScorer::with_prompt_fn(stub_returning(response), "test-model");
        let challenge = make_challenge();
        let err = scorer.score(&challenge).unwrap_err();
        assert!(matches!(err, SpecChallengeScorerError::Malformed(_)));
    }

    #[test]
    fn score_clamps_axis_values_to_valid_range() {
        let response = r#"{
            "axes": {
                "assumptions": 1.7,
                "gaps": -0.3,
                "ambiguities": 0.5,
                "alternatives_considered": 0.5,
                "constraints_not_satisfied": 0.5
            },
            "reasoning": "out of range; clamped"
        }"#
        .to_string();
        let scorer =
            LlmSpecChallengeScorer::with_prompt_fn(stub_returning(response), "test-model");
        let challenge = make_challenge();
        let score = scorer.score(&challenge).unwrap();
        assert!((score.assumptions - 1.0).abs() < 1e-3);
        assert!((score.gaps - 0.0).abs() < 1e-3);
    }

    #[test]
    fn all_axes_above_threshold_works_on_judge_output() {
        let response = r#"{
            "axes": {
                "assumptions": 0.8,
                "gaps": 0.8,
                "ambiguities": 0.8,
                "alternatives_considered": 0.8,
                "constraints_not_satisfied": 0.8
            },
            "reasoning": "uniformly good"
        }"#
        .to_string();
        let scorer =
            LlmSpecChallengeScorer::with_prompt_fn(stub_returning(response), "test-model");
        let challenge = make_challenge();
        let score = scorer.score(&challenge).unwrap();
        assert!(score.all_axes_above(0.7));
        assert!(!score.all_axes_above(0.9));
    }

    #[test]
    fn from_env_unknown_provider_errors() {
        let result = LlmSpecChallengeScorer::from_env_with(|key| match key {
            "SENTINEL_SPEC_CHALLENGE_SCORER_PROVIDER" => Some("nonsense".to_string()),
            _ => None,
        });
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown SENTINEL_SPEC_CHALLENGE_SCORER_PROVIDER"));
    }

    #[test]
    fn openrouter_from_env_requires_api_key() {
        let result = LlmSpecChallengeScorer::openrouter_from_env_with(|_| None);
        let err = result.unwrap_err();
        assert!(err.to_string().contains("OPENROUTER_API_KEY"));
    }

    #[test]
    fn ollama_from_env_requires_scorer_model() {
        let result = LlmSpecChallengeScorer::ollama_from_env_with(|_| None);
        let err = result.unwrap_err();
        assert!(err.to_string().contains("SENTINEL_SPEC_CHALLENGE_SCORER_MODEL"));
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
            if k == "SENTINEL_SPEC_CHALLENGE_SCORER_TIMEOUT_SECS" {
                Some("10".to_string())
            } else {
                None
            }
        };
        let t = read_timeout(&env);
        assert_eq!(t, Duration::from_secs(10));
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
        let p = preview("short", 100);
        assert_eq!(p, "short");
    }
}
