//! [`ConnectionEventLog`] — append-only JSONL stream of every
//! state transition the reconnect wrapper makes (Connecting,
//! Connected, Reconnecting with reason, Disconnected). Persistent
//! across daemon restarts, designed to answer the operator's 3am
//! "why did my daemon disconnect at 3:14am?" question after the
//! fact.
//!
//! Storage shape: one JSON object per line under
//! `~/.claude/sentinel/state/legatus-connection-events.jsonl`,
//! example:
//!
//! ```jsonl
//! {"t":"2026-05-24T16:30:00.000Z","state":"connecting","attempt":1,"reason":null}
//! {"t":"2026-05-24T16:30:00.450Z","state":"connected","attempt":1,"reason":null}
//! {"t":"2026-05-24T16:42:13.000Z","state":"reconnecting","attempt":1,"reason":"WS recv: connection reset by peer"}
//! {"t":"2026-05-24T16:42:14.100Z","state":"connecting","attempt":2,"reason":null}
//! {"t":"2026-05-24T16:42:14.580Z","state":"connected","attempt":2,"reason":null}
//! ```
//!
//! Designed to be cheap: each append is a single `O_APPEND` write
//! of one JSON line. No rotation logic (operators can `mv` it
//! aside and the wrapper will just create a fresh one on the next
//! append). On-disk failures (full disk, permission denied) are
//! logged at `warn` and otherwise silent — the connection itself
//! must never block on event-log write.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::connection_status::ConnectionState;

/// One transition record. Serialized to compact JSON (no pretty-
/// printing) so each line stays a single `O_APPEND` write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionEvent {
    /// ISO-8601 UTC timestamp at the moment of the transition.
    #[serde(rename = "t")]
    pub timestamp: String,
    /// New state after the transition.
    pub state: String,
    /// Reconnect attempt counter — incremented per attempt across
    /// the wrapper's lifetime. `Connecting` carries the in-progress
    /// attempt; `Reconnecting` carries the failed attempt that
    /// caused the transition.
    pub attempt: u64,
    /// For `Reconnecting`, the transport failure reason. `None`
    /// for `Connecting` / `Connected` / `Disconnected` (no
    /// meaningful reason — the state itself is the signal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Append-only JSONL writer. Clone-cheap (internally an `Arc`) so
/// the wrapper task and any future inspector share the same path
/// handle without re-resolving it on every write.
#[derive(Clone, Debug)]
pub struct ConnectionEventLog {
    inner: Arc<ConnectionEventLogInner>,
}

#[derive(Debug)]
struct ConnectionEventLogInner {
    path: PathBuf,
}

impl ConnectionEventLog {
    /// Build a log writer for an explicit path. Use this in tests
    /// (point at a tempfile); production uses [`Self::default`].
    #[must_use]
    pub fn at_path(path: PathBuf) -> Self {
        Self {
            inner: Arc::new(ConnectionEventLogInner { path }),
        }
    }

    /// Default path under `~/.claude/sentinel/state/`. Returns
    /// `None` when no home dir can be resolved — the wrapper
    /// degrades to no-event-log in that case (e.g. CI chroots).
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        Some(
            dirs::home_dir()?
                .join(".claude")
                .join("sentinel")
                .join("state")
                .join("legatus-connection-events.jsonl"),
        )
    }

    /// Append one event line. Best-effort: I/O failures log at
    /// `warn` and don't propagate (the WS loop must not block on
    /// disk).
    pub fn append(&self, event: &ConnectionEvent) {
        if let Some(parent) = self.inner.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut line = match serde_json::to_string(event) {
            Ok(s) => s,
            Err(err) => {
                warn!(?err, "ConnectionEventLog: serialize failed");
                return;
            }
        };
        line.push('\n');
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.inner.path)
        {
            Ok(mut f) => {
                if let Err(err) = f.write_all(line.as_bytes()) {
                    warn!(?err, path = ?self.inner.path, "ConnectionEventLog: write failed");
                }
            }
            Err(err) => {
                warn!(?err, path = ?self.inner.path, "ConnectionEventLog: open failed");
            }
        }
    }

    /// Convenience: record `Connecting` for the current attempt.
    pub fn record_connecting(&self, attempt: u64) {
        self.append(&ConnectionEvent {
            timestamp: now_iso(),
            state: ConnectionState::Connecting.as_str().to_owned(),
            attempt,
            reason: None,
        });
    }

    /// Convenience: record `Connected` for the current attempt.
    pub fn record_connected(&self, attempt: u64) {
        self.append(&ConnectionEvent {
            timestamp: now_iso(),
            state: ConnectionState::Connected.as_str().to_owned(),
            attempt,
            reason: None,
        });
    }

    /// Convenience: record `Reconnecting` triggered by the named
    /// transport failure.
    pub fn record_reconnecting(&self, attempt: u64, reason: impl Into<String>) {
        self.append(&ConnectionEvent {
            timestamp: now_iso(),
            state: ConnectionState::Reconnecting.as_str().to_owned(),
            attempt,
            reason: Some(reason.into()),
        });
    }

    /// Convenience: record `Disconnected` (terminal — cancel or
    /// fatal `VersionMismatch`).
    pub fn record_disconnected(&self, attempt: u64, reason: Option<String>) {
        self.append(&ConnectionEvent {
            timestamp: now_iso(),
            state: ConnectionState::Disconnected.as_str().to_owned(),
            attempt,
            reason,
        });
    }

    /// Path on disk (for diagnostics + tests).
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.inner.path
    }
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn append_creates_file_and_writes_one_jsonl_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = ConnectionEventLog::at_path(path.clone());
        log.record_connecting(1);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.ends_with('\n'), "lines must be newline-terminated");
        let v: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(v["state"], "connecting");
        assert_eq!(v["attempt"], 1);
        assert!(v.get("reason").is_none(), "no-reason events omit the field");
    }

    #[test]
    fn append_creates_parent_directory_if_missing() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("nested").join("more").join("events.jsonl");
        let log = ConnectionEventLog::at_path(nested.clone());
        log.record_connected(7);
        assert!(nested.exists(), "parent dirs must be created on first append");
    }

    #[test]
    fn multiple_appends_accumulate_in_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = ConnectionEventLog::at_path(path.clone());
        log.record_connecting(1);
        log.record_connected(1);
        log.record_reconnecting(1, "WS recv: connection reset");
        log.record_connecting(2);
        log.record_connected(2);
        let lines: Vec<&str> = std::fs::read_to_string(&path).unwrap()
            .lines()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|s| Box::leak(s.to_owned().into_boxed_str()) as &str)
            .collect();
        assert_eq!(lines.len(), 5);
        let third: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(third["state"], "reconnecting");
        assert_eq!(third["reason"], "WS recv: connection reset");
    }

    #[test]
    fn reason_is_omitted_when_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = ConnectionEventLog::at_path(path.clone());
        log.record_disconnected(3, None);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("\"reason\""),
            "skip_serializing_if must drop the field for None"
        );
    }

    #[test]
    fn disconnected_with_reason_carries_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = ConnectionEventLog::at_path(path.clone());
        log.record_disconnected(2, Some("VersionMismatch".to_owned()));
        let content = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(v["reason"], "VersionMismatch");
    }
}
