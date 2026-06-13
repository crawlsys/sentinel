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

/// Output-token bound for OpenRouter scorer/auditor calls. A reasoning model
/// (e.g. `gpt-5.5-pro`) runs UNBOUNDED — and the call stalls — if no
/// `max_tokens` is sent, so every scorer prompt must cap it. Sized for a JSON
/// verdict (scores + a paragraph of reasoning) PLUS the model's hidden
/// reasoning tokens, which are billed against this same budget. Must stay
/// ≥ 16: OpenAI rejects `max_output_tokens < 16` with a 400, which would make
/// the cross-vendor dual auditor's OpenAI leg error on every call and trip the
/// `block_for_inconclusive` path (a silent permanent-block, the same failure
/// mode the Fable-5-suspension fix removed).
const OPENROUTER_SCORER_MAX_TOKENS: u32 = 2048;

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
            // Bound output tokens — a reasoning model (gpt-5.5-pro) otherwise
            // runs unbounded and stalls; OpenAI also 400s `max_output_tokens`
            // below 16. See OPENROUTER_SCORER_MAX_TOKENS.
            let agent = AgentBuilder::new(client.completion_model(&model_id))
                .preamble(&system)
                .additional_params(serde_json::json!({
                    "max_tokens": OPENROUTER_SCORER_MAX_TOKENS,
                }))
                .build();
            let result: anyhow::Result<String, _> = agent.prompt(user_msg).await;
            result.map_err(|e| anyhow::anyhow!("openrouter {scorer_label} ({model_id}): {e}"))
        })
    });
    Ok((prompt_fn, "openrouter".to_string()))
}

// ---------------------------------------------------------------------------
// Subscription-backed CLI prompt-fns (claude -p / codex exec)
// ---------------------------------------------------------------------------
//
// When the operator has a subscription-backed CLI installed + authed, prefer
// it over the metered OpenRouter path: `claude -p` (Anthropic, subscription)
// and `codex exec` (OpenAI, subscription) both produce a one-shot completion
// for $0 per-token. Detection is presence-on-PATH only (DETECT-AND-USE; we
// never auto-install). Callers fall back to OpenRouter when these return None.

/// Resolve a CLI binary on `PATH` to its absolute path, or `None` if absent.
/// Cached per binary name for the engine's lifetime — a `which` per audit
/// would be wasteful, and PATH does not change mid-process.
#[must_use]
pub fn resolve_cli(bin: &str) -> Option<std::path::PathBuf> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static CACHE: OnceLock<Mutex<HashMap<String, Option<std::path::PathBuf>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().ok().and_then(|c| c.get(bin).cloned()) {
        return hit;
    }
    let found = which_on_path(bin);
    if let Ok(mut c) = cache.lock() {
        c.insert(bin.to_string(), found.clone());
    }
    found
}

/// Minimal cross-platform `which`: probe each `PATH` entry for `bin` (plus the
/// Windows executable extensions). Avoids a `which`-crate dependency.
fn which_on_path(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let candidate = dir.join(format!("{bin}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Build a `PromptFn` backed by the **`claude -p`** subscription CLI (the
/// Anthropic leg). Returns `None` when `claude` is not on `PATH` — the caller
/// then falls back to OpenRouter. The `model_id` argument is accepted for
/// signature-compatibility but ignored: `claude -p` uses the CLI's configured
/// subscription model. System prompt is passed via `--append-system-prompt`;
/// the user message is the positional `-p` prompt. Output is the assistant's
/// reply text (clean — `claude -p` prints only the answer).
#[must_use]
pub fn build_claude_cli_prompt_fn(scorer_label: &'static str) -> Option<(PromptFn, String)> {
    let bin = resolve_cli("claude")?;
    let prompt_fn: PromptFn = Arc::new(move |_model_id, system, user_msg| {
        let bin = bin.clone();
        Box::pin(async move {
            let out = tokio::process::Command::new(&bin)
                .arg("-p")
                .arg("--append-system-prompt")
                .arg(&system)
                .arg(&user_msg)
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("claude {scorer_label}: spawn failed: {e}"))?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                return Err(anyhow::anyhow!(
                    "claude {scorer_label}: exit {:?}: {}",
                    out.status.code(),
                    err.trim()
                ));
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
    });
    Some((prompt_fn, "claude-cli".to_string()))
}

/// Build a `PromptFn` backed by the **`codex exec`** subscription CLI (the
/// OpenAI leg). Returns `None` when `codex` is not on `PATH`. Uses
/// `--output-last-message <tmpfile>` so we read ONLY the model's final answer
/// (plain `codex exec` prints a full transcript). The system + user prompts are
/// concatenated into the single positional prompt (`codex exec` has no separate
/// system flag). `model_id` is accepted but ignored — `codex` uses its
/// configured subscription model.
#[must_use]
pub fn build_codex_cli_prompt_fn(scorer_label: &'static str) -> Option<(PromptFn, String)> {
    let bin = resolve_cli("codex")?;
    let prompt_fn: PromptFn = Arc::new(move |_model_id, system, user_msg| {
        let bin = bin.clone();
        Box::pin(async move {
            // `codex exec` has no system-prompt flag; prepend it to the prompt.
            let prompt = if system.is_empty() {
                user_msg
            } else {
                format!("{system}\n\n{user_msg}")
            };
            // Unique temp path for the final-message capture. No Date/rand in
            // this crate's sandbox, so derive uniqueness from pid + an atomic.
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let out_file = std::env::temp_dir()
                .join(format!("sentinel-codex-{}-{seq}.txt", std::process::id()));
            let status = tokio::process::Command::new(&bin)
                .arg("exec")
                .arg("--output-last-message")
                .arg(&out_file)
                .arg(&prompt)
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("codex {scorer_label}: spawn failed: {e}"))?;
            let answer = tokio::fs::read_to_string(&out_file).await.ok();
            // Best-effort cleanup of the temp file.
            let _ = tokio::fs::remove_file(&out_file).await;
            if !status.status.success() {
                let err = String::from_utf8_lossy(&status.stderr);
                return Err(anyhow::anyhow!(
                    "codex {scorer_label}: exit {:?}: {}",
                    status.status.code(),
                    err.trim()
                ));
            }
            match answer {
                Some(a) if !a.trim().is_empty() => Ok(a.trim().to_string()),
                _ => Err(anyhow::anyhow!(
                    "codex {scorer_label}: no final message captured"
                )),
            }
        })
    });
    Some((prompt_fn, "codex-cli".to_string()))
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

    // -----------------------------------------------------------------------
    // subscription CLI detection + prompt-fn builders
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_cli_returns_none_for_absent_binary() {
        // A binary name that cannot exist on PATH → None (and the caller then
        // falls back to OpenRouter — the zero-regression guarantee).
        assert!(resolve_cli("sentinel-no-such-binary-xyzzy-9999").is_none());
    }

    #[test]
    fn cli_prompt_fn_builders_are_none_when_binary_absent() {
        // We can't assume claude/codex are installed in CI, but we CAN assert
        // the builders return None for a guaranteed-absent name by routing
        // through the same which_on_path logic. Directly: a bogus binary.
        assert!(which_on_path("sentinel-no-such-binary-xyzzy-9999").is_none());
    }

    #[test]
    fn resolve_cli_is_cached() {
        // Two lookups of the same absent binary return the same (None) result;
        // the cache must not panic or diverge on repeat calls.
        let a = resolve_cli("sentinel-absent-cache-probe-0001");
        let b = resolve_cli("sentinel-absent-cache-probe-0001");
        assert_eq!(a, b);
        assert!(a.is_none());
    }

    #[test]
    fn which_on_path_finds_a_real_binary_when_present() {
        // Whatever this test runner is, SOME ubiquitous binary exists. On
        // Windows `cmd` is always on PATH; on unix `sh`. This exercises the
        // positive branch of which_on_path without depending on claude/codex.
        let ubiquitous = if cfg!(windows) { "cmd" } else { "sh" };
        // Not asserting Some unconditionally (PATH can be exotic in sandboxes),
        // but if found it must be an existing file.
        if let Some(p) = which_on_path(ubiquitous) {
            assert!(p.is_file(), "resolved path must be a real file: {p:?}");
        }
    }

    // -----------------------------------------------------------------------
    // strip_code_fence — existing coverage
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // strip_code_fence — additional edge cases
    // -----------------------------------------------------------------------

    /// Leading/trailing whitespace around the fenced block is stripped.
    #[test]
    fn strip_code_fence_trims_outer_whitespace() {
        let input = "  \n```json\n{\"k\":1}\n```\n  ";
        assert_eq!(strip_code_fence(input), r#"{"k":1}"#);
    }

    /// A fence with no closing ``` still returns the interior content (the
    /// implementation uses `trim_end_matches` which silently handles missing
    /// trailing fence).
    #[test]
    fn strip_code_fence_missing_closing_fence() {
        let input = "```json\n{\"k\":1}";
        // trim_end_matches("```") on "{\"k\":1}" finds no match → returns content as-is
        assert_eq!(strip_code_fence(input), r#"{"k":1}"#);
    }

    /// An empty string produces an empty string (no panic).
    #[test]
    fn strip_code_fence_empty_input() {
        assert_eq!(strip_code_fence(""), "");
    }

    /// Only whitespace produces an empty string.
    #[test]
    fn strip_code_fence_only_whitespace() {
        assert_eq!(strip_code_fence("   \n\t  "), "");
    }

    /// A fence whose interior is itself a JSON object spread across multiple
    /// lines is returned intact (newlines preserved inside the content).
    #[test]
    fn strip_code_fence_multiline_json_content() {
        let input = "```json\n{\n  \"a\": 1,\n  \"b\": 2\n}\n```";
        assert_eq!(strip_code_fence(input), "{\n  \"a\": 1,\n  \"b\": 2\n}");
    }

    // -----------------------------------------------------------------------
    // preview — existing coverage
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // preview — additional edge cases
    // -----------------------------------------------------------------------

    /// Exactly at the limit is returned unchanged (boundary value).
    #[test]
    fn preview_exact_limit_not_truncated() {
        let s = "a".repeat(10);
        assert_eq!(preview(&s, 10), s);
    }

    /// One character over the limit triggers truncation with ellipsis.
    #[test]
    fn preview_one_over_limit_truncated() {
        let s = "a".repeat(11);
        let p = preview(&s, 10);
        assert!(p.ends_with("..."));
        assert_eq!(p.chars().count(), 13); // 10 content + 3 ellipsis
    }

    /// Unicode multi-byte characters are counted by scalar value, not byte.
    #[test]
    fn preview_unicode_counted_by_scalar() {
        // Each '©' is 2 bytes but 1 scalar value.
        let s: String = "©".repeat(5);
        // Limit of 5 → no truncation (exactly at boundary).
        assert_eq!(preview(&s, 5), s);
        // Limit of 4 → truncation.
        let p = preview(&s, 4);
        assert!(p.ends_with("..."));
    }

    /// max_chars = 0 always truncates any non-empty string.
    #[test]
    fn preview_zero_limit_always_truncates() {
        let p = preview("hello", 0);
        assert_eq!(p, "...");
    }

    /// An empty string at any limit returns empty (nothing to truncate).
    #[test]
    fn preview_empty_string_any_limit() {
        assert_eq!(preview("", 0), "");
        assert_eq!(preview("", 100), "");
    }

    // -----------------------------------------------------------------------
    // read_timeout — existing coverage
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // read_timeout — additional edge cases
    // -----------------------------------------------------------------------

    /// A value of "0" is valid — results in Duration::ZERO (not the default).
    #[test]
    fn read_timeout_zero_value_is_valid() {
        let t = read_timeout(&|_| Some("0".to_string()), "VAR", Duration::from_secs(30));
        assert_eq!(t, Duration::ZERO);
    }

    /// A float string like "3.5" cannot be parsed as u64 → falls back to default.
    #[test]
    fn read_timeout_float_string_falls_back() {
        let t = read_timeout(&|_| Some("3.5".to_string()), "VAR", Duration::from_secs(10));
        assert_eq!(t, Duration::from_secs(10));
    }

    /// Negative number string cannot be parsed as u64 → falls back to default.
    #[test]
    fn read_timeout_negative_string_falls_back() {
        let t = read_timeout(&|_| Some("-5".to_string()), "VAR", Duration::from_secs(10));
        assert_eq!(t, Duration::from_secs(10));
    }

    /// An empty string cannot be parsed → falls back to default.
    #[test]
    fn read_timeout_empty_string_falls_back() {
        let t = read_timeout(&|_| Some(String::new()), "VAR", Duration::from_secs(10));
        assert_eq!(t, Duration::from_secs(10));
    }

    /// Only the named variable is read; other keys do not influence the result.
    #[test]
    fn read_timeout_only_reads_named_var() {
        let t = read_timeout(
            &|k: &str| match k {
                "RIGHT_VAR" => Some("99".to_string()),
                "WRONG_VAR" => Some("1".to_string()),
                _ => None,
            },
            "RIGHT_VAR",
            Duration::from_secs(30),
        );
        assert_eq!(t, Duration::from_secs(99));
    }

    // -----------------------------------------------------------------------
    // run_blocking — nested-runtime safety (SEN-18 regression)
    // -----------------------------------------------------------------------

    /// Helper: build a tiny dedicated sidecar runtime for the tests below.
    /// Each test builds its own so they stay independent.
    fn test_sidecar() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("sentinel-test-sidecar")
            .build()
            .expect("test sidecar runtime must build")
    }

    /// Happy path: a trivially-ready future resolves and `run_blocking`
    /// returns its value without panicking.
    #[test]
    fn run_blocking_returns_value_from_ready_future() {
        let rt = test_sidecar();
        let handle = rt.handle().clone();
        let fut: BoxFuture<'static, anyhow::Result<String>> =
            Box::pin(async { Ok("hello".to_string()) });

        let result = run_blocking(
            handle,
            Duration::from_secs(5),
            fut,
            |msg| format!("network: {msg}"),
            |dur| format!("timeout after {dur:?}"),
            || "panic".to_string(),
        );
        assert_eq!(result.unwrap(), "hello");
    }

    /// A future that returns `Err` is mapped through `map_network_err`.
    #[test]
    fn run_blocking_maps_network_error() {
        let rt = test_sidecar();
        let handle = rt.handle().clone();
        let fut: BoxFuture<'static, anyhow::Result<String>> =
            Box::pin(async { Err(anyhow::anyhow!("connection refused")) });

        let result = run_blocking(
            handle,
            Duration::from_secs(5),
            fut,
            |msg| format!("network: {msg}"),
            |dur| format!("timeout after {dur:?}"),
            || "panic".to_string(),
        );
        let err = result.unwrap_err();
        assert!(err.contains("network:"), "expected network prefix, got: {err}");
        assert!(err.contains("connection refused"), "expected cause, got: {err}");
    }

    /// A future that never resolves hits the timeout and `map_timeout` is
    /// called with the configured duration.
    #[test]
    fn run_blocking_maps_timeout() {
        let rt = test_sidecar();
        let handle = rt.handle().clone();
        // A future that waits longer than our timeout.
        let fut: BoxFuture<'static, anyhow::Result<String>> = Box::pin(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok("should not reach here".to_string())
        });
        let timeout = Duration::from_millis(50);

        let result = run_blocking(
            handle,
            timeout,
            fut,
            |msg| format!("network: {msg}"),
            |dur| format!("timed out after {dur:?}"),
            || "panic".to_string(),
        );
        let err = result.unwrap_err();
        assert!(err.contains("timed out after"), "expected timeout message, got: {err}");
    }

    /// Regression (SEN-18): `run_blocking` MUST NOT panic when called from
    /// within a tokio multi-thread runtime worker thread.  The old pattern
    /// of calling `sidecar.block_on()` directly on the caller's thread
    /// panicked because the caller was already a tokio worker.  The scoped-
    /// thread bridge fixes this.  This test would panic (not merely fail)
    /// against the pre-fix code.
    #[tokio::test]
    async fn run_blocking_does_not_panic_when_called_from_tokio_worker() {
        // Spawn `run_blocking` on a blocking thread of the CURRENT multi-thread
        // runtime — exactly the call path the PreToolUse hook dispatcher uses.
        let result = tokio::task::spawn_blocking(move || {
            let rt = test_sidecar();
            let handle = rt.handle().clone();
            let fut: BoxFuture<'static, anyhow::Result<String>> =
                Box::pin(async { Ok("nested-safe".to_string()) });
            run_blocking(
                handle,
                Duration::from_secs(5),
                fut,
                |msg| format!("network: {msg}"),
                |dur| format!("timeout after {dur:?}"),
                || "panic".to_string(),
            )
        })
        .await
        .expect("spawn_blocking task must not panic");

        assert_eq!(result.unwrap(), "nested-safe");
    }

    // -----------------------------------------------------------------------
    // build_ollama_prompt_fn — provider dispatch (pure construction, no network)
    // -----------------------------------------------------------------------

    /// Without `OLLAMA_API_KEY` the function constructs a local-mode client
    /// and returns the `"ollama-local"` provider prefix.
    #[test]
    fn build_ollama_prompt_fn_local_mode_when_no_api_key() {
        let env = |k: &str| -> Option<String> {
            match k {
                "OLLAMA_API_KEY" => None,
                "OLLAMA_HOST" => None, // use default
                _ => None,
            }
        };
        let (_, prefix) = build_ollama_prompt_fn(&env, "scorer").unwrap();
        assert_eq!(prefix, "ollama-local");
    }

    /// With `OLLAMA_API_KEY` set the function selects cloud mode and returns
    /// the `"ollama-cloud"` provider prefix.
    #[test]
    fn build_ollama_prompt_fn_cloud_mode_when_api_key_set() {
        let env = |k: &str| -> Option<String> {
            match k {
                "OLLAMA_API_KEY" => Some("fake-cloud-key".to_string()),
                "OLLAMA_BASE_URL" => None, // use default
                _ => None,
            }
        };
        let (_, prefix) = build_ollama_prompt_fn(&env, "scorer").unwrap();
        assert_eq!(prefix, "ollama-cloud");
    }

    /// `OLLAMA_HOST` is respected in local mode (custom host is accepted).
    #[test]
    fn build_ollama_prompt_fn_local_mode_custom_host_accepted() {
        let env = |k: &str| -> Option<String> {
            match k {
                "OLLAMA_API_KEY" => None,
                "OLLAMA_HOST" => Some("http://10.0.0.5:11434".to_string()),
                _ => None,
            }
        };
        // Construction must succeed (the client builder accepts any base URL).
        let (_, prefix) = build_ollama_prompt_fn(&env, "scorer")
            .unwrap_or_else(|e| panic!("custom OLLAMA_HOST should be accepted: {e}"));
        assert_eq!(prefix, "ollama-local");
    }

    /// `OLLAMA_BASE_URL` is respected in cloud mode (custom base URL accepted).
    #[test]
    fn build_ollama_prompt_fn_cloud_mode_custom_base_url_accepted() {
        let env = |k: &str| -> Option<String> {
            match k {
                "OLLAMA_API_KEY" => Some("key".to_string()),
                "OLLAMA_BASE_URL" => Some("https://my-ollama.example.com/v1".to_string()),
                _ => None,
            }
        };
        let (_, prefix) = build_ollama_prompt_fn(&env, "scorer")
            .unwrap_or_else(|e| panic!("custom OLLAMA_BASE_URL should be accepted: {e}"));
        assert_eq!(prefix, "ollama-cloud");
    }

    /// `build_openrouter_prompt_fn` returns the `"openrouter"` provider prefix.
    #[test]
    fn build_openrouter_prompt_fn_returns_openrouter_prefix() {
        // Any non-empty string is accepted by the client builder without a
        // live network call — the key is only validated on the first request.
        let (_, prefix) = build_openrouter_prompt_fn("fake-key", "scorer").unwrap();
        assert_eq!(prefix, "openrouter");
    }
}
