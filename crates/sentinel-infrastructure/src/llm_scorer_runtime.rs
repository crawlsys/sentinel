//! Shared LLM-bridge plumbing for the three scorer adapters:
//! [`crate::dry_run_auditor`], [`crate::eval_scorer`], and
//! [`crate::spec_challenge_scorer`].
//!
//! ## What lives here
//!
//! All three adapters need identical infrastructure:
//! - A type-erased `PromptFn` seam for rig-core async calls.
//! - `real_env` — the production `std::env::var` resolver.
//! - `read_timeout` — parse `<NAMESPACE>_TIMEOUT_SECS` with a fallback.
//! - `sidecar` — the per-scorer lazily-built tokio sidecar runtime
//!   (each scorer passes its own `thread_name` so logs are distinct).
//! - `run_blocking` — the `std::thread::scope` + `Handle::block_on`
//!   bridge that drives the async `PromptFn` from the sync `score()`
//!   method without tripping "Cannot start a runtime from within a
//!   runtime" when the caller is already inside a `#[tokio::main]`
//!   multi-thread runtime. Each caller maps the timeout / error result
//!   into its own typed error via the `map_timeout` closure.
//! - `strip_code_fence` — strip ```` ```json ```` / ```` ``` ```` wrappers
//!   that models sometimes emit despite instructions not to.
//! - `preview` — truncate a string for error messages.
//! - `build_rig_prompt_fn` — construct the openrouter / ollama-local /
//!   ollama-cloud `PromptFn` from environment, returning both the
//!   function and the `provider_prefix` string.
//!
//! ## Preserved invariant: nested-runtime safety (fix #18)
//!
//! `run_blocking` MUST drive `handle.block_on(...)` on a **dedicated
//! `std::thread::scope` thread**, never on the calling thread.  The
//! calling thread may be a tokio worker (the `PreToolUse` hook
//! dispatch); calling `block_on` there would panic.  The spawned
//! thread is outside every runtime's worker pool, so the
//! runtime-entry guard is never tripped.  This is the exact pattern
//! that fixed the browserbase hook crash (SEN-18) — do not "simplify"
//! it by calling `block_on` directly.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use futures::future::BoxFuture;
use rig_core::agent::AgentBuilder;
use rig_core::completion::Prompt;
use rig_core::prelude::CompletionClient;
use rig_core::providers::{openai, openrouter};
use tracing::warn;

// ---------------------------------------------------------------------------
// Public type alias
// ---------------------------------------------------------------------------

/// Type-erased prompt function: `(model_id, system, user_msg) -> response_text`.
///
/// Every scorer adapter stores one of these behind its struct and calls
/// it from the sync `score()` method via [`run_blocking`].
pub type PromptFn = Arc<
    dyn Fn(String, String, String) -> BoxFuture<'static, anyhow::Result<String>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// Environment helpers
// ---------------------------------------------------------------------------

/// Production env resolver — wraps `std::env::var`.
///
/// Tests inject HashMap-backed closures via the private `*_from_env_with`
/// variants instead, avoiding the process-wide env mutation that Rust 2024
/// marks as `unsafe`.
pub fn real_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Read a `<namespace>_TIMEOUT_SECS` variable from the supplied env
/// resolver, falling back to `default` on absent or unparseable values.
///
/// Each scorer passes its own env-var name:
///
/// | Scorer                  | var_name                                  |
/// |-------------------------|-------------------------------------------|
/// | `dry_run_auditor`       | `"SENTINEL_AUDITOR_TIMEOUT_SECS"`         |
/// | `eval_scorer`           | `"SENTINEL_EVAL_SCORER_TIMEOUT_SECS"`     |
/// | `spec_challenge_scorer` | `"SENTINEL_SPEC_CHALLENGE_SCORER_TIMEOUT_SECS"` |
pub fn read_timeout<F>(env: &F, var_name: &str, default: Duration) -> Duration
where
    F: Fn(&str) -> Option<String>,
{
    env(var_name)
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(default, Duration::from_secs)
}

// ---------------------------------------------------------------------------
// Sidecar runtime
// ---------------------------------------------------------------------------

/// Lazily build a named sidecar tokio runtime for a scorer.
///
/// Each call site passes a unique `thread_name` so runtime worker
/// threads are identifiable in traces / thread dumps:
///
/// | Scorer                  | `thread_name`                              |
/// |-------------------------|--------------------------------------------|
/// | `dry_run_auditor`       | `"sentinel-auditor-sidecar"`               |
/// | `eval_scorer`           | `"sentinel-eval-scorer-sidecar"`           |
/// | `spec_challenge_scorer` | `"sentinel-spec-challenge-scorer-sidecar"` |
///
/// The runtime is stored in a `OnceLock<Option<Runtime>>` supplied by
/// the caller so each scorer gets its own independent runtime slot.
/// Returns `None` when the runtime could not be built (logs a warning).
pub fn sidecar(
    once: &'static OnceLock<Option<tokio::runtime::Runtime>>,
    thread_name: &'static str,
) -> Option<&'static tokio::runtime::Runtime> {
    once.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name(thread_name)
            .build()
            .map_err(|e| warn!(?e, thread_name, "failed to build scorer sidecar runtime"))
            .ok()
    })
    .as_ref()
}

// ---------------------------------------------------------------------------
// Nested-runtime-safe blocking bridge  (#18 fix)
// ---------------------------------------------------------------------------

/// Drive an async future from a sync context via a sidecar runtime,
/// without panicking even when the **calling thread is already a tokio
/// worker** (the `PreToolUse` hook dispatch situation that caused the
/// browserbase crash).
///
/// ## Why this works
///
/// `tokio::runtime::Handle::block_on` blocks the *current* thread.
/// If the current thread is a tokio worker it panics.  The fix: spawn
/// a fresh `std::thread::scope` thread — that thread is outside every
/// runtime's worker pool — and call `block_on` there.
///
/// ## Parameters
///
/// - `handle` — a clone of the sidecar runtime's `Handle`.
/// - `timeout` — applied around the future with
///   `tokio::time::timeout`.
/// - `fut` — the `PromptFn` output, already constructed by the
///   caller.
/// - `map_timeout` — maps an `Elapsed` event into the caller's typed
///   error (avoids any dependency on specific error types here).
///
/// ## Return
///
/// `Ok(String)` on success, `Err(E)` on network error (propagated
/// from the future) or timeout (mapped by `map_timeout`), or
/// `Err(E)` when the worker thread panics (via the
/// `unwrap_or_else` branch).
pub fn run_blocking<E>(
    handle: tokio::runtime::Handle,
    timeout: Duration,
    fut: BoxFuture<'static, anyhow::Result<String>>,
    map_network_err: impl FnOnce(String) -> E + Send + 'static,
    map_timeout: impl FnOnce(Duration) -> E + Send + 'static,
    map_panic: impl FnOnce() -> E + Send + 'static,
) -> Result<String, E>
where
    E: Send + 'static,
{
    std::thread::scope(|s| {
        s.spawn(move || {
            handle.block_on(async move {
                match tokio::time::timeout(timeout, fut).await {
                    Ok(Ok(text)) => Ok(text),
                    Ok(Err(err)) => Err(map_network_err(format!("{err:#}"))),
                    Err(_elapsed) => Err(map_timeout(timeout)),
                }
            })
        })
        .join()
        .unwrap_or_else(|_| Err(map_panic()))
    })
}

// ---------------------------------------------------------------------------
// String helpers
// ---------------------------------------------------------------------------

/// Strip a markdown code fence (```` ```json ```` or ```` ``` ````) that
/// models sometimes wrap their JSON in despite instructions.
///
/// Returns an owned `String` — callers pass it to `serde_json::from_str`.
pub fn strip_code_fence(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        return rest.trim_end_matches("```").trim().to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        return rest.trim_end_matches("```").trim().to_string();
    }
    trimmed.to_string()
}

/// Truncate `text` to at most `max_chars` Unicode scalar values for use
/// in error messages.  Appends `"..."` when truncation occurs.
pub fn preview(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_chars).collect();
        format!("{truncated}...")
    }
}

// ---------------------------------------------------------------------------
// Provider dispatch
// ---------------------------------------------------------------------------

/// Dummy bearer token sent to local Ollama (which ignores the value on
/// its OpenAI-compat endpoint).
const OLLAMA_LOCAL_DUMMY_KEY: &str = "ollama-local";

/// Default base URL for local Ollama's OpenAI-compatible endpoint.
pub const DEFAULT_OLLAMA_LOCAL_BASE_URL: &str = "http://localhost:11434/v1";

/// Default base URL for Ollama Cloud's OpenAI-compatible endpoint.
pub const DEFAULT_OLLAMA_CLOUD_BASE_URL: &str = "https://ollama.com/v1";

/// Build a `PromptFn` that routes to the `OpenRouter` provider.
///
/// Returns `(PromptFn, "openrouter")`.
///
/// ## Parameters
///
/// - `key` — the `OPENROUTER_API_KEY` value.
/// - `scorer_label` — a short human-readable label used in error
///   messages, e.g. `"auditor"`, `"scorer"`,
///   `"spec-challenge scorer"`.
pub fn build_openrouter_prompt_fn(
    key: &str,
    scorer_label: &'static str,
) -> Result<(PromptFn, String)> {
    let client = Arc::new(
        openrouter::Client::new(key)
            .map_err(|e| anyhow::anyhow!("failed to build OpenRouter client: {e}"))?,
    );
    let prompt_fn: PromptFn = Arc::new(move |model_id, system, user_msg| {
        let client = client.clone();
        Box::pin(async move {
            let agent = AgentBuilder::new(client.completion_model(&model_id))
                .preamble(&system)
                .build();
            let result: anyhow::Result<String, _> = agent.prompt(user_msg).await;
            result.map_err(|e| anyhow::anyhow!("openrouter {scorer_label} ({model_id}): {e}"))
        })
    });
    Ok((prompt_fn, "openrouter".to_string()))
}

/// Build a `PromptFn` that routes to Ollama (local or cloud), auto-
/// detecting which mode to use from the env resolver.
///
/// - If `OLLAMA_API_KEY` is set → **Ollama Cloud**. Uses
///   `OLLAMA_BASE_URL` (default [`DEFAULT_OLLAMA_CLOUD_BASE_URL`]) with
///   bearer auth via rig-core's `openai` provider.
/// - Otherwise → **local Ollama**. Uses `OLLAMA_HOST` (default
///   `http://localhost:11434`); `/v1` is appended; a dummy bearer token
///   is sent because local Ollama's OpenAI-compat endpoint ignores it.
///
/// Returns `(PromptFn, provider_prefix)` where `provider_prefix` is
/// `"ollama-cloud"` or `"ollama-local"`.
///
/// ## Parameters
///
/// - `env` — the env resolver (same seam used everywhere in the scorers).
/// - `scorer_label` — short label for error messages.
pub fn build_ollama_prompt_fn<F>(
    env: &F,
    scorer_label: &'static str,
) -> Result<(PromptFn, String)>
where
    F: Fn(&str) -> Option<String>,
{
    let (base_url, api_key, provider_prefix) = env("OLLAMA_API_KEY").map_or_else(
        || {
            let host = env("OLLAMA_HOST").unwrap_or_else(|| "http://localhost:11434".to_string());
            let base = format!("{}/v1", host.trim_end_matches('/'));
            (
                base,
                OLLAMA_LOCAL_DUMMY_KEY.to_string(),
                "ollama-local".to_string(),
            )
        },
        |key| {
            let base =
                env("OLLAMA_BASE_URL").unwrap_or_else(|| DEFAULT_OLLAMA_CLOUD_BASE_URL.to_string());
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
    let provider_for_closure = provider_prefix.clone();
    let prompt_fn: PromptFn = Arc::new(move |model_id, system, user_msg| {
        let client = client.clone();
        let provider = provider_for_closure.clone();
        Box::pin(async move {
            let agent = AgentBuilder::new(client.completion_model(&model_id))
                .preamble(&system)
                .build();
            let result: anyhow::Result<String, _> = agent.prompt(user_msg).await;
            result.map_err(|e| anyhow::anyhow!("{provider} {scorer_label} ({model_id}): {e}"))
        })
    });
    Ok((prompt_fn, provider_prefix))
}

// ---------------------------------------------------------------------------
// Unit tests for the shared utilities
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_code_fence_json_variant() {
        let input = "```json\n{\"k\":1}\n```";
        assert_eq!(strip_code_fence(input), r#"{"k":1}"#);
    }

    #[test]
    fn strip_code_fence_bare_variant() {
        let input = "```\n{\"k\":1}\n```";
        assert_eq!(strip_code_fence(input), r#"{"k":1}"#);
    }

    #[test]
    fn strip_code_fence_passthrough_plain_json() {
        let input = r#"{"k":1}"#;
        assert_eq!(strip_code_fence(input), r#"{"k":1}"#);
    }

    #[test]
    fn preview_truncates() {
        let s = "x".repeat(300);
        let p = preview(&s, 100);
        assert_eq!(p.chars().count(), 103);
        assert!(p.ends_with("..."));
    }

    #[test]
    fn preview_passthrough_short() {
        assert_eq!(preview("hi", 100), "hi");
    }

    #[test]
    fn read_timeout_returns_default_when_absent() {
        let t = read_timeout(&|_: &str| None, "NO_SUCH_VAR", Duration::from_secs(30));
        assert_eq!(t, Duration::from_secs(30));
    }

    #[test]
    fn read_timeout_parses_override() {
        let t = read_timeout(
            &|k: &str| {
                if k == "MY_TIMEOUT" {
                    Some("42".to_string())
                } else {
                    None
                }
            },
            "MY_TIMEOUT",
            Duration::from_secs(30),
        );
        assert_eq!(t, Duration::from_secs(42));
    }

    #[test]
    fn read_timeout_falls_back_on_non_numeric() {
        let t = read_timeout(
            &|_| Some("not-a-number".to_string()),
            "VAR",
            Duration::from_secs(30),
        );
        assert_eq!(t, Duration::from_secs(30));
    }
}
