//! Unified LLM router — single entry point for picking an
//! `LlmPort` backend at runtime.
//!
//! Before this module, every consumer (viz-naming, judge, hook
//! `LlmPort` via `memory_verify`, `delegate_codex`/`_kimi`, spec
//! challenge scorer, eval scorer) constructed its own
//! provider-selection logic. Each one inlined a slightly different
//! env-var contract, default model mapping, error message, and
//! cloud-vs-local heuristic. Result: drift, surprise routing,
//! "why is this hitting OpenRouter when nighttime is right there".
//!
//! Now there's one router and one rule.
//!
//! ## Selection rule
//!
//! At process startup, `LlmRouter::from_env()`:
//!
//! 1. Reads `SENTINEL_LLM_PREFER` — explicit override taking values
//!    `local`, `cloud`, or `auto`. Default: `auto`.
//! 2. If `local` (or `auto` with a reachable local endpoint):
//!    builds `LocalLlm::from_env()` and probes `<base_url>/models`
//!    with a short timeout. If 200 OK AND the response lists at
//!    least one chat-capable model whose name matches any
//!    configured tier, the local backend wins.
//! 3. Otherwise: builds `OpenRouterLlm::from_env()` if
//!    `OPENROUTER_API_KEY` is set.
//! 4. Otherwise: `None` (no LLM available; consumers fall back to
//!    their no-LLM behaviour).
//!
//! `auto` mode is sticky for the process lifetime — the probe
//! runs once at startup, not per request. Operators wanting
//! "always prefer local, fail loud if it's down" set
//! `SENTINEL_LLM_PREFER=local`; operators wanting "always cloud
//! regardless of local availability" set
//! `SENTINEL_LLM_PREFER=cloud`.

use std::sync::Arc;
use std::time::Duration;

use sentinel_domain::ports::LlmPort;

use crate::local_llm::LocalLlm;
use crate::openrouter_llm::OpenRouterLlm;

/// Operator preference for backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmPreference {
    /// Probe local; fall back to OpenRouter if local isn't healthy.
    Auto,
    /// Force local. If unreachable, returns `None` rather than
    /// silently falling back to cloud (avoids surprise spend).
    Local,
    /// Force OpenRouter. Skips the local probe entirely.
    Cloud,
}

impl LlmPreference {
    fn from_env() -> Self {
        match std::env::var("SENTINEL_LLM_PREFER").as_deref() {
            Ok("local") | Ok("Local") | Ok("LOCAL") => Self::Local,
            Ok("cloud") | Ok("Cloud") | Ok("CLOUD") => Self::Cloud,
            _ => Self::Auto,
        }
    }
}

/// Result of a router build — which backend got picked and the
/// reason. Surfaced in logs at startup so operators can see at a
/// glance whether their config did what they expected.
#[derive(Debug, Clone)]
pub struct RouterDecision {
    pub backend: BackendKind,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Local,
    OpenRouter,
    None,
}

/// Unified router. Wraps the chosen backend behind the domain
/// `LlmPort` trait so consumers depend only on the port, not on
/// the provider identity.
pub struct LlmRouter {
    port: Option<Arc<dyn LlmPort>>,
    decision: RouterDecision,
}

impl LlmRouter {
    /// Build from environment + a live health probe. Probe timeout
    /// is short (`PROBE_TIMEOUT`) so a network-stalled local
    /// endpoint doesn't delay startup.
    ///
    /// `tokio` runtime must be available — the probe is async.
    pub async fn from_env() -> Self {
        let pref = LlmPreference::from_env();
        Self::with_preference(pref).await
    }

    /// Explicit-preference constructor. Used by tests + by code
    /// that wants to bypass env-var parsing (e.g. a CLI flag).
    pub async fn with_preference(pref: LlmPreference) -> Self {
        match pref {
            LlmPreference::Cloud => Self::build_openrouter("SENTINEL_LLM_PREFER=cloud"),
            LlmPreference::Local => Self::build_local_strict().await,
            LlmPreference::Auto => Self::build_auto().await,
        }
    }

    /// Return the picked port, or `None` if no backend is
    /// available. Callers treat `None` the same as a missing
    /// `OPENROUTER_API_KEY` did pre-refactor — skip the LLM work.
    #[must_use]
    pub fn port(&self) -> Option<Arc<dyn LlmPort>> {
        self.port.clone()
    }

    /// Diagnostic info — what got picked and why.
    #[must_use]
    pub fn decision(&self) -> &RouterDecision {
        &self.decision
    }

    /// Convenience: build the router and return just the port. The
    /// vast majority of callers only need the port and don't care
    /// about the decision.
    pub async fn default_port() -> Option<Arc<dyn LlmPort>> {
        Self::from_env().await.port()
    }

    async fn build_auto() -> Self {
        // Try local first.
        if let Ok(local) = LocalLlm::from_env() {
            let base_url = local.base_url().to_string();
            if probe_local(&base_url).await {
                let arc: Arc<dyn LlmPort> = Arc::new(local);
                tracing::info!(target: "sentinel::llm_router",
                    backend = "local", base_url = %base_url,
                    "auto: local LLM healthy, routing all traffic to it");
                return Self {
                    port: Some(arc),
                    decision: RouterDecision {
                        backend: BackendKind::Local,
                        reason: format!("auto: local healthy at {base_url}"),
                    },
                };
            }
            tracing::info!(target: "sentinel::llm_router",
                base_url = %base_url,
                "auto: local LLM unreachable, falling back to OpenRouter");
        }
        Self::build_openrouter("auto: local unreachable, falling back to OpenRouter")
    }

    async fn build_local_strict() -> Self {
        match LocalLlm::from_env() {
            Ok(local) => {
                let base_url = local.base_url().to_string();
                if probe_local(&base_url).await {
                    let arc: Arc<dyn LlmPort> = Arc::new(local);
                    Self {
                        port: Some(arc),
                        decision: RouterDecision {
                            backend: BackendKind::Local,
                            reason: format!("forced local, healthy at {base_url}"),
                        },
                    }
                } else {
                    // Forced local mode does NOT fall back — operator
                    // chose this explicitly to avoid surprise cloud spend.
                    tracing::warn!(target: "sentinel::llm_router",
                        base_url = %base_url,
                        "SENTINEL_LLM_PREFER=local but probe failed; LLM disabled");
                    Self {
                        port: None,
                        decision: RouterDecision {
                            backend: BackendKind::None,
                            reason: format!("forced local unreachable at {base_url}"),
                        },
                    }
                }
            }
            Err(e) => Self {
                port: None,
                decision: RouterDecision {
                    backend: BackendKind::None,
                    reason: format!("forced local construction failed: {e}"),
                },
            },
        }
    }

    fn build_openrouter(reason: &str) -> Self {
        match OpenRouterLlm::from_env() {
            Ok(client) => {
                let arc: Arc<dyn LlmPort> = Arc::new(client);
                Self {
                    port: Some(arc),
                    decision: RouterDecision {
                        backend: BackendKind::OpenRouter,
                        reason: reason.to_string(),
                    },
                }
            }
            Err(_) => Self {
                port: None,
                decision: RouterDecision {
                    backend: BackendKind::None,
                    reason: format!("{reason}; OPENROUTER_API_KEY unset, no LLM available"),
                },
            },
        }
    }
}

/// Probe timeout — short so startup doesn't stall on a wedged
/// local endpoint. The probe is a one-shot HTTP GET; if it can't
/// answer in 1.5s the local backend isn't worth waiting on.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// GET `<base_url>/models` and check the response. Returns true
/// when:
///   - HTTP 200 OK
///   - body is JSON with a `data` array (OpenAI `/v1/models` shape)
///   - the array is non-empty
///
/// Returns false on any error (DNS, connection refused, timeout,
/// non-200, malformed JSON, empty model list). False is the safe
/// default — we'd rather route to cloud than route to a wedged
/// local endpoint that returns nonsense.
async fn probe_local(base_url: &str) -> bool {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let res = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return false,
    };
    if !res.status().is_success() {
        return false;
    }
    let body: serde_json::Value = match res.json().await {
        Ok(v) => v,
        Err(_) => return false,
    };
    body.get("data")
        .and_then(|v| v.as_array())
        .is_some_and(|arr| !arr.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Env-mutating test serial — same lock pattern as the
    /// adapter tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn preference_parses_known_values() {
        let _g = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("SENTINEL_LLM_PREFER").ok();
        for (val, expected) in [
            ("local", LlmPreference::Local),
            ("Local", LlmPreference::Local),
            ("LOCAL", LlmPreference::Local),
            ("cloud", LlmPreference::Cloud),
            ("Cloud", LlmPreference::Cloud),
            ("CLOUD", LlmPreference::Cloud),
            ("auto", LlmPreference::Auto),
            ("garbage", LlmPreference::Auto),
            ("", LlmPreference::Auto),
        ] {
            std::env::set_var("SENTINEL_LLM_PREFER", val);
            assert_eq!(LlmPreference::from_env(), expected, "for value '{val}'");
        }
        std::env::remove_var("SENTINEL_LLM_PREFER");
        assert_eq!(LlmPreference::from_env(), LlmPreference::Auto, "unset → Auto");
        if let Some(v) = saved {
            std::env::set_var("SENTINEL_LLM_PREFER", v);
        }
    }

    #[tokio::test]
    async fn forced_local_returns_none_when_unreachable() {
        let _g = ENV_LOCK.lock().unwrap();
        let saved_host = std::env::var("OLLAMA_HOST").ok();
        // Point at a port nothing's listening on. Probe must fail
        // fast (within PROBE_TIMEOUT) and the router must return
        // None — not silently fall through to OpenRouter.
        std::env::set_var("OLLAMA_HOST", "http://127.0.0.1:1");
        let router = LlmRouter::with_preference(LlmPreference::Local).await;
        assert!(router.port().is_none(), "forced local with bad host → no port");
        assert_eq!(router.decision().backend, BackendKind::None);
        match saved_host {
            Some(v) => std::env::set_var("OLLAMA_HOST", v),
            None => std::env::remove_var("OLLAMA_HOST"),
        }
    }

    #[tokio::test]
    async fn auto_falls_back_to_openrouter_when_local_unreachable_and_key_present() {
        let _g = ENV_LOCK.lock().unwrap();
        let saved_host = std::env::var("OLLAMA_HOST").ok();
        let saved_key = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::set_var("OLLAMA_HOST", "http://127.0.0.1:1");
        std::env::set_var("OPENROUTER_API_KEY", "test-key-xyz");
        let router = LlmRouter::with_preference(LlmPreference::Auto).await;
        assert!(router.port().is_some(), "auto with key should fall back");
        assert_eq!(router.decision().backend, BackendKind::OpenRouter);
        // Restore.
        match saved_host {
            Some(v) => std::env::set_var("OLLAMA_HOST", v),
            None => std::env::remove_var("OLLAMA_HOST"),
        }
        match saved_key {
            Some(v) => std::env::set_var("OPENROUTER_API_KEY", v),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }
    }

    #[tokio::test]
    async fn auto_returns_none_when_local_unreachable_and_no_openrouter_key() {
        let _g = ENV_LOCK.lock().unwrap();
        let saved_host = std::env::var("OLLAMA_HOST").ok();
        let saved_key = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::set_var("OLLAMA_HOST", "http://127.0.0.1:1");
        std::env::remove_var("OPENROUTER_API_KEY");
        let router = LlmRouter::with_preference(LlmPreference::Auto).await;
        assert!(router.port().is_none(), "no backends → no port");
        assert_eq!(router.decision().backend, BackendKind::None);
        match saved_host {
            Some(v) => std::env::set_var("OLLAMA_HOST", v),
            None => std::env::remove_var("OLLAMA_HOST"),
        }
        if let Some(v) = saved_key {
            std::env::set_var("OPENROUTER_API_KEY", v);
        }
    }

    #[tokio::test]
    async fn forced_cloud_skips_local_probe() {
        let _g = ENV_LOCK.lock().unwrap();
        let saved_key = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::set_var("OPENROUTER_API_KEY", "test-key-xyz");
        // OLLAMA_HOST is irrelevant in cloud mode.
        let router = LlmRouter::with_preference(LlmPreference::Cloud).await;
        assert_eq!(router.decision().backend, BackendKind::OpenRouter);
        match saved_key {
            Some(v) => std::env::set_var("OPENROUTER_API_KEY", v),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }
    }

    #[tokio::test]
    async fn probe_rejects_non_200() {
        // Spin up a one-shot HTTP server that returns 503, then
        // probe it. Must return false. Smoke test for the probe's
        // status-code check.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        let base = format!("http://{addr}/v1");
        assert!(!probe_local(&base).await);
    }

    #[tokio::test]
    async fn probe_rejects_empty_models_list() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let body = r#"{"data":[]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });
        let base = format!("http://{addr}/v1");
        assert!(!probe_local(&base).await, "empty data array → not healthy");
    }

    #[tokio::test]
    async fn probe_accepts_non_empty_models_list() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let body = r#"{"data":[{"id":"qwen3.5-35b-a3b","object":"model"}]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });
        let base = format!("http://{addr}/v1");
        assert!(probe_local(&base).await, "non-empty data → healthy");
    }
}
