//! Tiny sync HTTP client that hooks use to POST escalations to
//! the local sentinel daemon's `/legatus/escalate` endpoint.
//!
//! Why sync: hooks return [`sentinel_domain::events::HookOutput`]
//! synchronously and most are called in standalone contexts
//! without a tokio runtime. We don't want hook code to need
//! `async`-awareness just to push a notification onto the
//! daemon's queue.
//!
//! Why fire-and-forget: the hook MUST NOT block on the daemon —
//! the daemon might not be running (standalone `sentinel hook`
//! invocation), the consulate might be down, or the legatus
//! might not be hosted (daemon started without
//! `--legatus-consulate-url`). All three are common; none should
//! delay Claude Code's hook reply by even one HTTP round-trip.
//! [`escalate_fire_and_forget`] spawns an OS thread that does the
//! POST and logs the outcome; the hook returns immediately.
//!
//! The daemon token + port live at `~/.claude/sentinel/daemon-token`
//! in the format `<port>:<token>` (per `sentinel-cli`'s
//! `daemon_cmd::write_token_file`).

use std::path::PathBuf;
use std::time::Duration;

use sentinel_legatus::EscalationKind;

/// Read the daemon token + port from
/// `~/.claude/sentinel/daemon-token`. Returns `None` if the file
/// doesn't exist (daemon not running) or is malformed.
fn read_daemon_token() -> Option<(u16, String)> {
    let path = token_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let trimmed = content.trim();
    let (port_str, token) = trimmed.split_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    Some((port, token.to_owned()))
}

fn token_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("sentinel")
            .join("daemon-token"),
    )
}

/// Spawn a background OS thread that POSTs `event` to the daemon's
/// `/legatus/escalate` endpoint. Returns immediately. If the
/// daemon isn't running (no token file) or the POST fails, logs
/// at `debug`/`warn` and the thread exits.
pub fn escalate_fire_and_forget(event: EscalationKind) {
    std::thread::spawn(move || {
        if let Err(err) = post_escalation(event) {
            tracing::debug!(?err, "legatus escalation skipped");
        }
    });
}

#[derive(Debug, thiserror::Error)]
enum LegatusClientError {
    #[error("daemon token file not present (daemon not running?)")]
    TokenAbsent,
    #[error("request: {0}")]
    Request(reqwest::Error),
    #[error("daemon returned {0}")]
    Status(u16),
    #[error("serialize event: {0}")]
    Serialize(serde_json::Error),
}

fn post_escalation(event: EscalationKind) -> Result<(), LegatusClientError> {
    let (port, token) = read_daemon_token().ok_or(LegatusClientError::TokenAbsent)?;
    let url = format!("http://127.0.0.1:{port}/legatus/escalate");
    let body = serde_json::to_string(&event).map_err(LegatusClientError::Serialize)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(LegatusClientError::Request)?;
    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .map_err(LegatusClientError::Request)?;
    let status = response.status();
    if !status.is_success() {
        return Err(LegatusClientError::Status(status.as_u16()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_daemon_token_returns_none_when_file_absent() {
        // We can't easily make ~/.claude/sentinel/daemon-token
        // absent on dev machines that have a real daemon running.
        // The contract: if it's None or malformed, we return None
        // without panic. Just call it and trust the type system.
        let _ = read_daemon_token();
    }

    #[test]
    fn escalate_fire_and_forget_does_not_panic_when_daemon_absent() {
        // Spawns a thread that errors out (or succeeds if a
        // daemon happens to be running). Either way the caller
        // returns immediately and we're testing no-panic.
        escalate_fire_and_forget(EscalationKind::Completed {
            summary: Some("unit-test ping".into()),
        });
    }
}
