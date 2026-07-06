//! Channel event emitter for MCP push notifications.
//!
//! Writes event files to `~/.claude/sentinel/events/{session_id}/` for the
//! sentinel-mcp server to pick up and push into the correct Claude Code
//! session via MCP channels. Session-scoped directories prevent cross-session
//! event contamination when multiple Claude sessions run concurrently.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use sentinel_domain::ports::{EnvPort, FileSystemPort};

/// A lifecycle event to be pushed into the Claude Code session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelEvent {
    /// Event type identifier
    pub event: String,
    /// Human-readable summary for Claude
    pub summary: String,
    /// ISO 8601 timestamp
    pub ts: String,
    /// Session ID that owns this event
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Project context (cwd basename or project name)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Source agent or hook that generated this event
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_agent: Option<String>,
    /// Optional structured metadata
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub meta: serde_json::Map<String, serde_json::Value>,
}

/// Get the base events directory (parent of all session subdirs).
fn events_base_dir(fs: &dyn FileSystemPort) -> std::path::PathBuf {
    fs.claude_dir().join("sentinel").join("events")
}

/// Delegates to the canonical session-id validator
/// (`crate::hooks::session_path_component`). This body was a byte-for-byte copy
/// of the canonical logic; centralizing it keeps the event-routing key
/// derivation in lockstep with the rest of sentinel.
fn concrete_session_id(session_id: &str) -> Option<&str> {
    crate::hooks::session_path_component(session_id)
}

/// Get the session-scoped events directory.
///
/// Returns `~/.claude/sentinel/events/{session_id}/` only for concrete,
/// validated session identities. Missing or synthetic session IDs do not get a
/// shared fallback directory because that would mix event authority across
/// concurrent Claude sessions.
pub fn events_dir_for_session(
    fs: &dyn FileSystemPort,
    session_id: Option<&str>,
) -> Option<std::path::PathBuf> {
    let subdir = concrete_session_id(session_id?)?;
    Some(events_base_dir(fs).join(subdir))
}

/// Get the events directory for the current session, with the session id
/// resolved from the env adapter.
pub fn events_dir(fs: &dyn FileSystemPort, env: &dyn EnvPort) -> Option<std::path::PathBuf> {
    let session_id = detect_session_id(env);
    events_dir_for_session(fs, session_id.as_deref())
}

/// Env vars that may carry the session identity, in priority order.
///
/// MUST stay in lockstep with the consumer (`sentinel-mcp-rust`
/// `resolve_session_id_opt`): Claude Code exports `CLAUDE_CODE_SESSION_ID`,
/// while the handler/SDK stack uses `VULCAN_SESSION_ID`/`CLAUDE_SESSION_ID`.
/// A producer and consumer that resolve different strings for the same
/// session strand every event in a directory no live watcher reads.
pub const SESSION_ID_ENV_VARS: [&str; 4] = [
    "VULCAN_SESSION_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_SESSION_ID",
    "SESSION_ID",
];

/// Detect the current session ID from the env adapter.
fn detect_session_id(env: &dyn EnvPort) -> Option<String> {
    SESSION_ID_ENV_VARS
        .iter()
        .find_map(|key| env.var(key).and_then(|s| concrete_session_id(&s).map(str::to_string)))
}

/// Derive a project name from a cwd path (uses the last path component).
fn project_from_cwd(cwd: Option<&str>) -> Option<String> {
    cwd.and_then(|p| {
        std::path::Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
    })
}

/// Emoji vocabulary for channel-event kinds — one glyph per event family so
/// pushed notifications scan at a glance. Unknown kinds get no prefix.
fn event_emoji(event: &str) -> Option<&'static str> {
    Some(match event {
        "agent_completed" | "task_completed" => "✅",
        "teammate_idle" => "💤",
        "build_completed" => "🔨",
        "deploy_completed" => "🚀",
        "mcp_server_failure" | "mcp_registration_missing" => "🚨",
        "context_threshold" => "🟡",
        "plan_organized" => "📋",
        "linear_inbound_drift" => "🔄",
        _ if event.starts_with("hookdeck.") => "🔔",
        _ => return None,
    })
}

/// Prefix `summary` with the event's vocabulary emoji unless the producer
/// already leads with a non-ASCII glyph of its own.
fn decorate_summary(event: &str, summary: &str) -> String {
    match event_emoji(event) {
        Some(emoji) if summary.chars().next().is_some_and(|c| c.is_ascii()) => {
            format!("{emoji} {summary}")
        }
        _ => summary.to_string(),
    }
}

/// Emit a channel event by writing a JSON file to the session-scoped events directory.
///
/// `session_id` and `cwd` should come from `HookInput` when available.
/// If `session_id` is `None`, env-adapter detection is attempted. Missing,
/// synthetic, or malformed session identities do not emit event files.
pub fn emit(
    fs: &dyn FileSystemPort,
    env: &dyn EnvPort,
    event: &str,
    summary: &str,
    meta: serde_json::Map<String, serde_json::Value>,
    session_id: Option<&str>,
    cwd: Option<&str>,
    source_agent: Option<&str>,
) {
    let resolved_session_id = match session_id {
        Some(session_id) => concrete_session_id(session_id).map(str::to_string),
        None => detect_session_id(env),
    };

    let Some(dir) = events_dir_for_session(fs, resolved_session_id.as_deref()) else {
        warn!("Skipping channel event without concrete session identity");
        return;
    };
    if let Err(e) = fs.create_dir_all(&dir) {
        warn!(error = %e, "Failed to create events directory");
        return;
    }

    let now = Utc::now();
    let filename = format!(
        "{}_{}.json",
        now.timestamp_millis(),
        event.replace(|c: char| !c.is_ascii_alphanumeric() && c != '_', "_")
    );

    let channel_event = ChannelEvent {
        summary: decorate_summary(event, summary),
        event: event.to_string(),
        ts: now.to_rfc3339(),
        session_id: resolved_session_id,
        project: project_from_cwd(cwd),
        source_agent: source_agent.map(String::from),
        meta,
    };

    let path = dir.join(&filename);
    match serde_json::to_string(&channel_event) {
        Ok(json) => {
            if let Err(e) = fs.write(&path, json.as_bytes()) {
                warn!(error = %e, path = %path.display(), "Failed to write channel event");
            } else {
                debug!(event, path = %path.display(), "Channel event emitted");
            }
        }
        Err(e) => warn!(error = %e, "Failed to serialize channel event"),
    }
}

/// Read and parse a channel event file.
pub fn read_event(fs: &dyn FileSystemPort, path: &std::path::Path) -> Option<ChannelEvent> {
    let content = fs.read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// List all pending event files in a session-scoped directory, sorted oldest first.
pub fn pending_events_for_session(
    fs: &dyn FileSystemPort,
    session_id: Option<&str>,
) -> Vec<std::path::PathBuf> {
    let Some(dir) = events_dir_for_session(fs, session_id) else {
        return Vec::new();
    };
    pending_events_in_dir(fs, &dir)
}

/// List all pending event files in the current session's directory.
pub fn pending_events(fs: &dyn FileSystemPort, env: &dyn EnvPort) -> Vec<std::path::PathBuf> {
    let Some(dir) = events_dir(fs, env) else {
        return Vec::new();
    };
    pending_events_in_dir(fs, &dir)
}

fn pending_events_in_dir(
    fs: &dyn FileSystemPort,
    dir: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    let mut entries: Vec<std::path::PathBuf> = fs
        .read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    entries.sort();
    entries
}

/// Delete an event file after it has been consumed.
pub fn consume(fs: &dyn FileSystemPort, path: &std::path::Path) {
    if let Err(e) = fs.remove_file(path) {
        warn!(error = %e, path = %path.display(), "Failed to remove consumed event file");
    }
}

/// Remove session event directories older than the given duration.
///
/// Call during `SessionStart` to prevent stale directories from accumulating.
pub fn cleanup_stale_sessions(fs: &dyn FileSystemPort, max_age: std::time::Duration) {
    let base = events_base_dir(fs);
    let cutoff = std::time::SystemTime::now()
        .checked_sub(max_age)
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    let entries = match fs.read_dir(&base) {
        Ok(e) => e,
        Err(_) => return,
    };

    for path in entries {
        if !fs.is_dir(&path) {
            continue;
        }

        let modified = match fs
            .metadata(&path)
            .and_then(|m| m.modified().map_err(Into::into))
        {
            Ok(t) => t,
            Err(_) => continue,
        };

        if modified < cutoff {
            if let Err(e) = fs.remove_dir_all(&path) {
                debug!(error = %e, path = %path.display(), "Failed to remove stale session events dir");
            } else {
                debug!(path = %path.display(), "Cleaned up stale session events directory");
            }
        }
    }
}

/// Remove individual event *files* older than `max_age`, whatever the age of
/// their session directory.
///
/// `cleanup_stale_sessions` removes whole directories by mtime — but a
/// directory whose producer is alive while no consumer watches it (session-id
/// split-brain, crashed watcher) keeps a fresh mtime forever as its backlog
/// grows. Event filenames lead with the emit timestamp in unix millis, so
/// staleness is decided from the name alone; unparseable names fall back to
/// fs metadata. Directories themselves are left in place — deleting a live
/// consumer's watched directory would sever its `notify` subscription.
pub fn cleanup_stale_events(fs: &dyn FileSystemPort, max_age: std::time::Duration) {
    let base = events_base_dir(fs);
    let Ok(entries) = fs.read_dir(&base) else {
        return;
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(max_age)
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let cutoff_ms = cutoff
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());

    for dir in entries {
        if !fs.is_dir(&dir) {
            continue;
        }
        for file in pending_events_in_dir(fs, &dir) {
            let by_name = file
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.split('_').next())
                .and_then(|ms| ms.parse::<u128>().ok())
                .map(|ms| ms < cutoff_ms);
            let stale = by_name.unwrap_or_else(|| {
                fs.metadata(&file)
                    .and_then(|m| m.modified().map_err(Into::into))
                    .is_ok_and(|t| t < cutoff)
            });
            if stale {
                if let Err(e) = fs.remove_file(&file) {
                    debug!(error = %e, path = %file.display(), "Failed to remove stale event file");
                }
            }
        }
    }
}

/// Build a [`ChannelEvent`] from a Hookdeck webhook, using the typed decoders
/// to produce a human-readable `summary` and preserving the raw JSON body
/// under `meta.raw` so consumers can still drill into the full payload.
///
/// This is the glue between the `hookdeck_decoders` module and the channel
/// emission pipeline. The hookdeck channel bridge (in `vulcan-hookdeck`)
/// should call this when it wants a typed one-line summary — the bridge
/// doesn't depend on sentinel-application today, but any sidecar that writes
/// event files into `~/.claude/sentinel/events/{session_id}/` can use this
/// to build its payload.
pub fn channel_event_from_webhook(
    source: &str,
    event_type: Option<&str>,
    body: &serde_json::Value,
    extra_meta: serde_json::Map<String, serde_json::Value>,
) -> ChannelEvent {
    let decoded = crate::hooks::hookdeck_decoders::decode(source, event_type, body);

    // SEN-1: persist Linear Issue.update state transitions to cycle-time.jsonl
    // as a side effect of decoding the webhook. Failures are logged but never
    // propagate — JSONL persistence is opportunistic, the channel event is
    // the contract.
    if source == "linear" {
        if let Some(evt) = crate::cycle_time::extract_from_linear_webhook(body) {
            if let Err(e) = crate::cycle_time::append(&evt) {
                tracing::warn!(error = %e, issue = %evt.issue_id, "cycle-time append failed");
            }
        }
    }

    let mut meta = extra_meta;
    meta.insert(
        "source".to_string(),
        serde_json::Value::String(source.to_string()),
    );
    if let Some(et) = event_type {
        meta.insert(
            "event_type".to_string(),
            serde_json::Value::String(et.to_string()),
        );
    }
    // Preserve raw JSON so downstream consumers can still drill in if needed.
    // Session-visible content uses only `summary`.
    meta.insert("raw".to_string(), decoded.raw);

    let event = format!("hookdeck.{source}");
    let summary = decorate_summary(&event, &decoded.summary);
    ChannelEvent {
        event,
        summary,
        ts: Utc::now().to_rfc3339(),
        session_id: SESSION_ID_ENV_VARS.iter().find_map(|key| {
            std::env::var(key)
                .ok()
                .and_then(|s| concrete_session_id(&s).map(str::to_string))
        }),
        project: None,
        source_agent: Some("hookdeck".into()),
        meta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_event.json");

        let event = ChannelEvent {
            event: "agent_completed".to_string(),
            summary: "Researcher finished".to_string(),
            ts: Utc::now().to_rfc3339(),
            session_id: Some("test-session-123".to_string()),
            project: Some("sentinel".to_string()),
            source_agent: Some("debugger".to_string()),
            meta: serde_json::Map::new(),
        };

        let json = serde_json::to_string(&event).unwrap();
        std::fs::write(&path, &json).unwrap();

        // Inline FileSystemPort impl that delegates to real disk for read.
        struct TestFs;
        impl FileSystemPort for TestFs {
            fn home_dir(&self) -> Option<std::path::PathBuf> {
                dirs::home_dir()
            }
            fn read_to_string(
                &self,
                p: &std::path::Path,
            ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
                std::fs::read_to_string(p)
                    .map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn write(
                &self,
                _: &std::path::Path,
                _: &[u8],
            ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                Ok(())
            }
            fn create_dir_all(
                &self,
                _: &std::path::Path,
            ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                Ok(())
            }
            fn read_dir(
                &self,
                _: &std::path::Path,
            ) -> Result<Vec<std::path::PathBuf>, sentinel_domain::port_errors::FileSystemError>
            {
                Ok(vec![])
            }
            fn exists(&self, p: &std::path::Path) -> bool {
                p.exists()
            }
            fn is_dir(&self, p: &std::path::Path) -> bool {
                p.is_dir()
            }
            fn metadata(
                &self,
                p: &std::path::Path,
            ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError>
            {
                std::fs::metadata(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn append(
                &self,
                _: &std::path::Path,
                _: &[u8],
            ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                Ok(())
            }
        }

        let read = read_event(&TestFs, &path).unwrap();
        assert_eq!(read.event, "agent_completed");
        assert_eq!(read.summary, "Researcher finished");
        assert_eq!(read.session_id.as_deref(), Some("test-session-123"));
        assert_eq!(read.project.as_deref(), Some("sentinel"));
        assert_eq!(read.source_agent.as_deref(), Some("debugger"));
    }

    #[test]
    fn test_roundtrip_legacy_format() {
        let json = r#"{"event":"build_completed","summary":"Build done","ts":"2026-04-16T12:00:00Z","meta":{}}"#;
        let event: ChannelEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event, "build_completed");
        assert!(event.session_id.is_none());
        assert!(event.project.is_none());
        assert!(event.source_agent.is_none());
    }

    #[test]
    fn test_events_dir_for_session() {
        let fs = crate::hooks::test_support::StubFs;
        let dir = events_dir_for_session(&fs, Some("abc-123")).expect("concrete session dir");
        assert!(dir.ends_with("events/abc-123") || dir.ends_with("events\\abc-123"));

        assert!(events_dir_for_session(&fs, None).is_none());
        assert!(events_dir_for_session(&fs, Some("unknown")).is_none());
        assert!(events_dir_for_session(&fs, Some(" UNKNOWN ")).is_none());
        assert!(events_dir_for_session(&fs, Some("default")).is_none());
        assert!(events_dir_for_session(&fs, Some(" Default ")).is_none());
        assert!(events_dir_for_session(&fs, Some("../escape")).is_none());
    }

    struct TempHomeFs {
        home: std::path::PathBuf,
    }

    impl FileSystemPort for TempHomeFs {
        fn home_dir(&self) -> Option<std::path::PathBuf> {
            Some(self.home.clone())
        }

        fn read_to_string(
            &self,
            p: &std::path::Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_to_string(p)?)
        }

        fn write(
            &self,
            p: &std::path::Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Ok(std::fs::write(p, c)?)
        }

        fn create_dir_all(
            &self,
            p: &std::path::Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::create_dir_all(p)?)
        }

        fn read_dir(
            &self,
            p: &std::path::Path,
        ) -> Result<Vec<std::path::PathBuf>, sentinel_domain::port_errors::FileSystemError>
        {
            Ok(std::fs::read_dir(p)?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect())
        }

        fn exists(&self, p: &std::path::Path) -> bool {
            p.exists()
        }

        fn is_dir(&self, p: &std::path::Path) -> bool {
            p.is_dir()
        }

        fn metadata(
            &self,
            p: &std::path::Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::metadata(p)?)
        }

        fn append(
            &self,
            p: &std::path::Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?;
            Ok(file.write_all(c)?)
        }

        // The port's default `remove_file` fails as unsupported — the
        // stale-event sweep actually deletes, so the fixture must too.
        fn remove_file(
            &self,
            p: &std::path::Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::remove_file(p)?)
        }
    }

    #[test]
    fn emit_without_session_does_not_write_unscoped_event_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TempHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let env = crate::hooks::test_support::StubEnv::new();

        emit(
            &fs,
            &env,
            "agent_completed",
            "Agent completed",
            serde_json::Map::new(),
            None,
            Some("/tmp/sentinel"),
            Some("tester"),
        );

        assert!(!tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("events")
            .join("_unscoped")
            .exists());
    }

    #[test]
    fn emit_rejects_explicit_unknown_without_env_fallback() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TempHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let env =
            crate::hooks::test_support::StubEnv::with(&[("CLAUDE_SESSION_ID", "real-session")]);

        emit(
            &fs,
            &env,
            "agent_completed",
            "Agent completed",
            serde_json::Map::new(),
            Some("unknown"),
            Some("/tmp/sentinel"),
            Some("tester"),
        );

        assert!(!tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("events")
            .join("real-session")
            .exists());
    }

    #[test]
    fn emit_rejects_explicit_default_without_env_fallback() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TempHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let env =
            crate::hooks::test_support::StubEnv::with(&[("CLAUDE_SESSION_ID", "real-session")]);

        emit(
            &fs,
            &env,
            "agent_completed",
            "Agent completed",
            serde_json::Map::new(),
            Some(" Default "),
            Some("/tmp/sentinel"),
            Some("tester"),
        );

        let events_root = tmp.path().join(".claude").join("sentinel").join("events");
        assert!(
            !events_root.join("real-session").exists(),
            "synthetic explicit session must not fall back to env session"
        );
        assert!(
            !events_root.join("Default").exists(),
            "synthetic default session must not create durable event authority"
        );
    }

    #[test]
    fn emit_rejects_default_env_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TempHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let env = crate::hooks::test_support::StubEnv::with(&[("CLAUDE_SESSION_ID", " default ")]);

        emit(
            &fs,
            &env,
            "agent_completed",
            "Agent completed",
            serde_json::Map::new(),
            None,
            Some("/tmp/sentinel"),
            Some("tester"),
        );

        assert!(
            !tmp.path()
                .join(".claude")
                .join("sentinel")
                .join("events")
                .exists(),
            "synthetic env session must not create durable event authority"
        );
    }

    #[test]
    fn emit_with_concrete_session_writes_session_scoped_event() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TempHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let env = crate::hooks::test_support::StubEnv::new();

        emit(
            &fs,
            &env,
            "agent_completed",
            "Agent completed",
            serde_json::Map::new(),
            Some("channel-real-session"),
            Some("/tmp/sentinel"),
            Some("tester"),
        );

        let pending = pending_events_for_session(&fs, Some("channel-real-session"));
        assert_eq!(pending.len(), 1);
        let event = read_event(&fs, &pending[0]).expect("channel event");
        assert_eq!(event.session_id.as_deref(), Some("channel-real-session"));
        // `emit` stamps the event-kind emoji onto ASCII summaries.
        assert_eq!(event.summary, "✅ Agent completed");
    }

    #[test]
    fn detect_session_id_reads_claude_code_var() {
        // Claude Code exports CLAUDE_CODE_SESSION_ID — the producer must
        // resolve it, or events land in a dir no consumer watches.
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TempHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let env = crate::hooks::test_support::StubEnv::with(&[(
            "CLAUDE_CODE_SESSION_ID",
            "cc-session-1",
        )]);

        emit(
            &fs,
            &env,
            "agent_completed",
            "done",
            serde_json::Map::new(),
            None,
            None,
            Some("tester"),
        );

        assert_eq!(pending_events_for_session(&fs, Some("cc-session-1")).len(), 1);
    }

    #[test]
    fn detect_session_id_priority_order_prefers_vulcan() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TempHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let env = crate::hooks::test_support::StubEnv::with(&[
            ("VULCAN_SESSION_ID", "vulcan-sess"),
            ("CLAUDE_CODE_SESSION_ID", "cc-sess"),
            ("SESSION_ID", "generic-sess"),
        ]);

        emit(
            &fs,
            &env,
            "agent_completed",
            "done",
            serde_json::Map::new(),
            None,
            None,
            Some("tester"),
        );

        assert_eq!(pending_events_for_session(&fs, Some("vulcan-sess")).len(), 1);
        assert!(pending_events_for_session(&fs, Some("cc-sess")).is_empty());
        assert!(pending_events_for_session(&fs, Some("generic-sess")).is_empty());
    }

    #[test]
    fn summary_gets_event_emoji_prefix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TempHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let env = crate::hooks::test_support::StubEnv::new();

        emit(
            &fs,
            &env,
            "agent_completed",
            "Agent \"Explore\" has finished.",
            serde_json::Map::new(),
            Some("emoji-sess-1"),
            None,
            Some("Explore"),
        );

        let pending = pending_events_for_session(&fs, Some("emoji-sess-1"));
        let event = read_event(&fs, &pending[0]).expect("channel event");
        assert_eq!(event.summary, "✅ Agent \"Explore\" has finished.");
    }

    #[test]
    fn summary_with_existing_emoji_is_not_double_prefixed() {
        assert_eq!(decorate_summary("agent_completed", "✅ already tagged"), "✅ already tagged");
        assert_eq!(decorate_summary("teammate_idle", "plain text"), "💤 plain text");
        assert_eq!(decorate_summary("hookdeck.linear", "[linear] Issue.update"), "🔔 [linear] Issue.update");
        assert_eq!(decorate_summary("unmapped_kind", "plain text"), "plain text");
    }

    #[test]
    fn cleanup_stale_events_removes_old_files_keeps_fresh_and_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TempHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let dir = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("events")
            .join("stale-sess-1");
        std::fs::create_dir_all(&dir).unwrap();

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let old = dir.join("1000000000000_agent_completed.json");
        let fresh = dir.join(format!("{now_ms}_agent_completed.json"));
        std::fs::write(&old, b"{}").unwrap();
        std::fs::write(&fresh, b"{}").unwrap();

        cleanup_stale_events(&fs, std::time::Duration::from_secs(24 * 60 * 60));

        assert!(!old.exists(), "stale event file must be removed");
        assert!(fresh.exists(), "fresh event file must be kept");
        assert!(dir.exists(), "session dir must never be removed by file sweep");
    }

    #[test]
    fn test_project_from_cwd() {
        assert_eq!(
            project_from_cwd(Some("/Users/operator/projects/sentinel")),
            Some("sentinel".to_string())
        );
        #[cfg(windows)]
        assert_eq!(
            project_from_cwd(Some("C:\\Users\\operator\\sentinel")),
            Some("sentinel".to_string())
        );
        assert_eq!(project_from_cwd(None), None);
    }

    #[test]
    fn channel_event_from_webhook_uses_typed_decoder() {
        let body = serde_json::json!({
            "action": "create",
            "type": "Comment",
            "data": {
                "body": "hi",
                "issue": { "identifier": "FPCRM-1", "team": { "key": "FPCRM" } }
            },
            "actor": { "name": "QA reviewer" }
        });
        let ev = channel_event_from_webhook("linear", None, &body, serde_json::Map::new());
        assert_eq!(
            ev.summary,
            "🔔 [LINEAR] QA reviewer commented on FPCRM-1: \"hi\""
        );
        assert_eq!(ev.event, "hookdeck.linear");
        assert_eq!(ev.source_agent.as_deref(), Some("hookdeck"));
        assert_eq!(
            ev.meta.get("source").and_then(|v| v.as_str()),
            Some("linear")
        );
        // Raw JSON is preserved for drill-in.
        assert!(ev.meta.get("raw").is_some());
    }

    #[test]
    fn channel_event_from_webhook_falls_back_cleanly() {
        let body = serde_json::json!({
            "action": "weird",
            "data": { "id": "x1" }
        });
        let ev = channel_event_from_webhook(
            "unknown_src",
            Some("thing.event"),
            &body,
            serde_json::Map::new(),
        );
        assert_eq!(ev.summary, "🔔 [HOOKDECK:unknown_src] thing.event on x1");
        // Never 400-line JSON — summary stays one short line.
        assert!(ev.summary.len() < 200);
        assert_eq!(
            ev.meta.get("event_type").and_then(|v| v.as_str()),
            Some("thing.event")
        );
    }

    #[test]
    fn test_cleanup_removes_old_dirs() {
        let tmpdir = tempfile::tempdir().unwrap();
        let base = tmpdir.path();
        let stale = base.join("old-session");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("event.json"), "{}").unwrap();

        // With max_age=0, everything is "stale"
        // We need to call with the base dir overridden — test the logic directly
        let cutoff = std::time::SystemTime::now();
        // Sleep tiny bit so mtime < cutoff
        std::thread::sleep(std::time::Duration::from_millis(10));

        for entry in std::fs::read_dir(base).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Ok(meta) = path.metadata() {
                    if let Ok(modified) = meta.modified() {
                        if modified < cutoff {
                            let _ = std::fs::remove_dir_all(&path);
                        }
                    }
                }
            }
        }
        assert!(!stale.exists());
    }
}
