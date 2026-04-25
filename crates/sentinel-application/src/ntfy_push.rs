//! ntfy.sh push notifications — best-effort phone notifications from sentinel hooks.
//!
//! Resolves credentials and target topic at call time (caller > env > active
//! account in `~/.ntfy/accounts.json`). Spawns a background tokio task so the
//! hook never blocks on network I/O. Any failure (no token, no topic, network
//! error, non-2xx) is logged at `debug!` and swallowed — this function NEVER
//! returns an error to the hook.
//!
//! Usage:
//! ```ignore
//! ntfy_push::push_attention("Build FAILED", "cargo build error", 4, &["x"]);
//! ntfy_push::push_to_topic(
//!     "gary-somerhalder-deploys", "Deploy OK", "vercel done", 2, &["rocket"]);
//! ```

use std::path::PathBuf;

use serde_json::json;

const DEFAULT_BASE_URL: &str = "https://ntfy.sh";
const ACCOUNTS_FILE: &str = ".ntfy/accounts.json";

/// Topic reserved for events that need Gary's eyes immediately.
pub const TOPIC_ATTENTION: &str = "gary-somerhalder-claude-code-attention";

/// Push to the canonical "needs attention" topic. Use for hard failures and
/// session-blocking events.
pub fn push_attention(title: &str, message: &str, priority: u8, tags: &[&str]) {
    push_to_topic(TOPIC_ATTENTION, title, message, priority, tags);
}

/// Push to an arbitrary ntfy topic. Best-effort; spawns a tokio task, never
/// blocks, never returns errors.
pub fn push_to_topic(topic: &str, title: &str, message: &str, priority: u8, tags: &[&str]) {
    // Allow tests to disable real network calls without faking out the
    // resolver — checked here, before the spawn, so unit tests don't leak
    // tasks into the runtime.
    if std::env::var("SENTINEL_NTFY_DISABLE").ok().as_deref() == Some("1") {
        tracing::debug!(topic, "ntfy_push disabled via SENTINEL_NTFY_DISABLE=1");
        return;
    }

    let creds = match resolve_creds() {
        Some(c) => c,
        None => {
            tracing::debug!("ntfy_push: no credentials available, skipping");
            return;
        }
    };

    let topic = topic.to_owned();
    let title = title.to_owned();
    let message = message.to_owned();
    let priority = priority.clamp(1, 5);
    let tags: Vec<String> = tags.iter().map(|t| (*t).to_string()).collect();

    // If we're already inside a tokio runtime, spawn there. Otherwise spawn
    // a one-shot thread with a small runtime so this works from sync hook
    // contexts too. Either way: fully non-blocking from the caller's POV.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            send(&creds, &topic, &title, &message, priority, &tags).await;
        });
    } else {
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::debug!(error = %e, "ntfy_push: failed to build runtime");
                    return;
                }
            };
            rt.block_on(async {
                send(&creds, &topic, &title, &message, priority, &tags).await;
            });
        });
    }
}

#[derive(Clone)]
struct Creds {
    token: String,
    base_url: String,
}

/// Resolve credentials in priority order. Returns `None` if nothing usable.
///
/// 1. `NTFY_TOKEN` env var (with optional `NTFY_BASE_URL`)
/// 2. Active account in `~/.ntfy/accounts.json`
fn resolve_creds() -> Option<Creds> {
    if let Ok(token) = std::env::var("NTFY_TOKEN") {
        if !token.is_empty() {
            let base_url = std::env::var("NTFY_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
            return Some(Creds { token, base_url });
        }
    }

    let path = accounts_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let active = v.get("active_account")?.as_str()?;
    let cfg = v.get("accounts")?.get(active)?;
    let token = cfg.get("token")?.as_str()?.to_string();
    if token.is_empty() {
        return None;
    }
    let base_url = cfg
        .get("base_url")
        .and_then(|x| x.as_str())
        .map_or_else(|| DEFAULT_BASE_URL.to_string(), ToString::to_string);
    Some(Creds { token, base_url })
}

fn accounts_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(ACCOUNTS_FILE))
}

async fn send(creds: &Creds, topic: &str, title: &str, message: &str, priority: u8, tags: &[String]) {
    let body = json!({
        "topic": topic,
        "title": title,
        "message": message,
        "priority": priority,
        "tags": tags,
    });

    let url = creds.base_url.trim_end_matches('/').to_string();
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .bearer_auth(&creds.token)
        .json(&body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            tracing::debug!(topic, "ntfy_push delivered");
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            tracing::debug!(topic, %status, body = %body, "ntfy_push non-2xx");
        }
        Err(e) => {
            tracing::debug!(topic, error = %e, "ntfy_push network error");
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Priority clamps to 1..=5 — exercised purely through the value-clamping
    /// logic to avoid spawning network tasks or mutating process env (the
    /// workspace forbids `unsafe`, and `std::env::set_var` is unsafe in
    /// edition 2024).
    #[test]
    fn priority_clamp_logic() {
        assert_eq!(0_u8.clamp(1, 5), 1);
        assert_eq!(1_u8.clamp(1, 5), 1);
        assert_eq!(3_u8.clamp(1, 5), 3);
        assert_eq!(5_u8.clamp(1, 5), 5);
        assert_eq!(99_u8.clamp(1, 5), 5);
    }

    /// `accounts_path` returns `Some(<home>/.ntfy/accounts.json)` whenever a
    /// home directory is resolvable on the host. Smoke-tests the path shape
    /// without requiring the file to exist.
    #[test]
    fn accounts_path_under_home() {
        if let Some(p) = accounts_path() {
            let s = p.to_string_lossy();
            assert!(s.ends_with("accounts.json"), "got {s}");
            assert!(s.contains(".ntfy"), "got {s}");
        }
    }

    /// `TOPIC_ATTENTION` matches the topic Gary reserved on ntfy.sh.
    /// Hard-coded constant test guards against accidental rename.
    #[test]
    fn attention_topic_constant() {
        assert_eq!(TOPIC_ATTENTION, "gary-somerhalder-claude-code-attention");
    }
}
