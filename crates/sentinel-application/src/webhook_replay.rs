//! Webhook replay — catchup for events missed while the session was down.
//!
//! Sentinel persists the timestamp of the most recent webhook delivered to
//! each session. On the next `SessionStart`, a helper reads that timestamp,
//! calls the webhook gateway (Hookdeck) to fetch events since then, and
//! surfaces a compact catchup summary so the new session can continue from
//! where the prior one left off.
//!
//! This module provides:
//!   * Pure state-file I/O (`LastSeenStore`) — trivially unit-testable.
//!   * A `ReplayResult` summary shape that callers serialize into channel
//!     events or log lines.
//!   * An `analyze_events` helper that turns a list of raw Hookdeck events
//!     (the decoded, typed shape produced by the 4b decoders) into a summary.
//!
//! The HTTP/Hookdeck side of replay lives in `sentinel-mcp-rust` where the
//! async runtime and the Hookdeck API client are already wired up.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The persisted "last webhook seen" marker for a single session.
///
/// Stored as a plain RFC3339 timestamp inside
/// `~/.claude/sentinel/state/{session_id}/last_webhook_ts.txt`. Intentionally
/// a simple text file so humans can edit or delete it to force a full
/// catchup.
pub struct LastSeenStore {
    path: PathBuf,
}

impl LastSeenStore {
    /// Build a store for the given session in the default state directory.
    pub fn for_session(session_id: &str) -> Self {
        let dir = state_dir_for_session(session_id);
        Self {
            path: dir.join("last_webhook_ts.txt"),
        }
    }

    /// Build a store backed by an explicit path (useful for tests).
    pub const fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Path of the marker file, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the stored timestamp, or `None` if the file is missing or empty.
    /// Returns `Err` only on I/O errors that aren't "file missing".
    pub fn read(&self) -> std::io::Result<Option<DateTime<Utc>>> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                match DateTime::parse_from_rfc3339(trimmed) {
                    Ok(ts) => Ok(Some(ts.with_timezone(&Utc))),
                    Err(_) => {
                        // Corrupt marker — treat as absent rather than surfacing
                        // a parse error to the caller.
                        Ok(None)
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Overwrite the marker with the given timestamp.
    pub fn write(&self, ts: DateTime<Utc>) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write via tempfile + rename for atomicity on all platforms.
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, ts.to_rfc3339())?;
        std::fs::rename(&tmp, &self.path)
    }
}

/// Default state dir for a session: `~/.claude/sentinel/state/{session_id}/`.
pub fn state_dir_for_session(session_id: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("sentinel")
        .join("state")
        .join(session_id)
}

/// A decoded webhook event as produced by the 4b typed decoders. The replay
/// analyzer doesn't care about the raw payload — it only reads fields that
/// the decoders promise to set.
///
/// This struct intentionally mirrors what the decoder emits so downstream
/// consumers (including [`analyze_events`]) can be written once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodedWebhook {
    /// Source name: "linear", "github", "vercel", etc.
    pub source: String,
    /// Event type: "Issue.update", "`pull_request.closed`", "`check_run.completed`".
    pub event_type: String,
    /// Primary resource identifier: "FPCRM-329", "owner/repo#123", etc.
    #[serde(default)]
    pub resource_id: Option<String>,
    /// Human summary (e.g. "FPCRM-329 → Done").
    #[serde(default)]
    pub summary: Option<String>,
    /// When the event was created at the source.
    pub ts: DateTime<Utc>,
    /// Outcome tag for failure-style events: "failure", "success", None.
    #[serde(default)]
    pub outcome: Option<String>,
}

/// Summary of a replay operation — safe to serialize into a single catchup
/// channel event or log line.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayResult {
    /// Window start (the prior session's last-seen timestamp), or None if no
    /// prior session was known and replay was skipped.
    pub since: Option<DateTime<Utc>>,
    /// Window end (now at replay time).
    pub until: Option<DateTime<Utc>>,
    /// Total events fetched.
    pub event_count: usize,
    /// Counts per (source, `event_type`) bucket, sorted by count desc.
    pub buckets: Vec<Bucket>,
    /// Human-readable one-line summaries for the top N events by importance
    /// (failures/state-changes first).
    pub highlights: Vec<String>,
    /// True when the catchup window was skipped (no prior marker found).
    pub skipped_first_run: bool,
}

impl ReplayResult {
    /// Render a single-line banner for injection into the session.
    pub fn banner(&self) -> String {
        if self.skipped_first_run {
            return "[HOOKDECK REPLAY] first session — no catchup window".to_string();
        }
        match (self.since, self.until) {
            (Some(since), _) if self.event_count == 0 => {
                format!("[HOOKDECK REPLAY] since {} — no events", since.to_rfc3339())
            }
            (Some(since), _) => {
                let bucket_summary = self
                    .buckets
                    .iter()
                    .take(5)
                    .map(|b| format!("{} {}", b.count, b.label))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "[HOOKDECK REPLAY] Since {}: {} event{} — {}",
                    since.to_rfc3339(),
                    self.event_count,
                    if self.event_count == 1 { "" } else { "s" },
                    bucket_summary
                )
            }
            _ => "[HOOKDECK REPLAY] no prior window".to_string(),
        }
    }
}

/// A counted bucket for [`ReplayResult::buckets`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bucket {
    /// Human label, e.g. "PRs merged" or "github `check_run.completed`".
    pub label: String,
    /// How many events fell into this bucket.
    pub count: usize,
}

/// Summarize a batch of decoded webhooks into a [`ReplayResult`].
///
/// Pure function — no I/O, no clock assumptions beyond what the caller passes
/// via `since`/`until`. Sorting is stable so the output is deterministic.
pub fn analyze_events(
    events: &[DecodedWebhook],
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> ReplayResult {
    let mut buckets_map: std::collections::BTreeMap<String, usize> = Default::default();
    let mut highlights: Vec<(i32, String)> = Vec::new();

    for ev in events {
        let label = bucket_label_for(&ev.source, &ev.event_type);
        *buckets_map.entry(label).or_insert(0) += 1;

        // Score: failures > state changes > everything else. Only top few surface.
        let score = match ev.outcome.as_deref() {
            Some("failure" | "error" | "timed_out" | "cancelled" | "action_required") => 100,
            _ if ev.event_type.contains("closed") || ev.event_type.contains("merged") => 50,
            _ if ev.event_type.contains("update") || ev.event_type.contains("completed") => 20,
            _ => 1,
        };
        if let Some(s) = ev.summary.clone() {
            highlights.push((score, s));
        }
    }

    // Buckets sorted by count desc, then label asc for stability.
    let mut buckets: Vec<Bucket> = buckets_map
        .into_iter()
        .map(|(label, count)| Bucket { label, count })
        .collect();
    buckets.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.label.cmp(&b.label)));

    // Highlights sorted by score desc, keep top 5.
    highlights.sort_by(|a, b| b.0.cmp(&a.0));
    let highlights: Vec<String> = highlights.into_iter().take(5).map(|(_, s)| s).collect();

    ReplayResult {
        since,
        until,
        event_count: events.len(),
        buckets,
        highlights,
        skipped_first_run: false,
    }
}

/// Human-friendly bucket label. Avoids dumping raw event type names at the
/// user when a friendlier phrase exists.
fn bucket_label_for(source: &str, event_type: &str) -> String {
    match (source, event_type) {
        ("github", "pull_request.closed") => "PRs closed".into(),
        ("github", "pull_request.merged") => "PRs merged".into(),
        ("github", "check_run.completed") => "CI runs".into(),
        ("github", "issue_comment.created") => "PR/issue comments".into(),
        ("linear", "Issue.update") => "Linear state changes".into(),
        ("vercel", e) if e.starts_with("deployment.") => "Vercel deploys".into(),
        ("railway", e) if e.starts_with("deployment.") => "Railway deploys".into(),
        (src, et) => format!("{src} {et}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn read_missing_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LastSeenStore::at(tmp.path().join("never_written.txt"));
        assert!(store.read().unwrap().is_none());
    }

    #[test]
    fn write_then_read_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LastSeenStore::at(tmp.path().join("marker.txt"));
        let t = Utc.with_ymd_and_hms(2026, 4, 22, 12, 34, 56).unwrap();
        store.write(t).unwrap();

        let read = store.read().unwrap().expect("should be present");
        assert_eq!(read, t);
    }

    #[test]
    fn read_corrupt_file_returns_none_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("marker.txt");
        std::fs::write(&path, "not a timestamp").unwrap();
        let store = LastSeenStore::at(path);
        assert!(store.read().unwrap().is_none());
    }

    #[test]
    fn read_empty_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("marker.txt");
        std::fs::write(&path, "").unwrap();
        let store = LastSeenStore::at(path);
        assert!(store.read().unwrap().is_none());
    }

    #[test]
    fn analyze_counts_buckets_and_orders_desc_by_count() {
        let t = Utc.with_ymd_and_hms(2026, 4, 22, 10, 0, 0).unwrap();
        let evs = vec![
            DecodedWebhook {
                source: "github".into(),
                event_type: "check_run.completed".into(),
                resource_id: Some("owner/repo#1".into()),
                summary: Some("CI failed for #1".into()),
                ts: t,
                outcome: Some("failure".into()),
            },
            DecodedWebhook {
                source: "github".into(),
                event_type: "check_run.completed".into(),
                resource_id: Some("owner/repo#2".into()),
                summary: Some("CI failed for #2".into()),
                ts: t,
                outcome: Some("failure".into()),
            },
            DecodedWebhook {
                source: "linear".into(),
                event_type: "Issue.update".into(),
                resource_id: Some("FPCRM-1".into()),
                summary: Some("FPCRM-1 → Done".into()),
                ts: t,
                outcome: None,
            },
        ];
        let result = analyze_events(&evs, Some(t), Some(t));
        assert_eq!(result.event_count, 3);
        assert_eq!(result.buckets.len(), 2);
        assert_eq!(result.buckets[0].label, "CI runs");
        assert_eq!(result.buckets[0].count, 2);
        assert_eq!(result.buckets[1].label, "Linear state changes");
        assert_eq!(result.buckets[1].count, 1);
    }

    #[test]
    fn highlights_prioritize_failures_over_mundane_updates() {
        let t = Utc::now();
        let evs = vec![
            DecodedWebhook {
                source: "linear".into(),
                event_type: "Issue.update".into(),
                resource_id: Some("FPCRM-1".into()),
                summary: Some("mundane update".into()),
                ts: t,
                outcome: None,
            },
            DecodedWebhook {
                source: "github".into(),
                event_type: "check_run.completed".into(),
                resource_id: None,
                summary: Some("CI FAILED".into()),
                ts: t,
                outcome: Some("failure".into()),
            },
        ];
        let result = analyze_events(&evs, None, None);
        assert_eq!(result.highlights[0], "CI FAILED");
    }

    #[test]
    fn banner_renders_empty_window() {
        let t = Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap();
        let result = ReplayResult {
            since: Some(t),
            until: Some(t),
            event_count: 0,
            buckets: vec![],
            highlights: vec![],
            skipped_first_run: false,
        };
        let banner = result.banner();
        assert!(banner.contains("HOOKDECK REPLAY"));
        assert!(banner.contains("no events"));
    }

    #[test]
    fn banner_renders_busy_window_with_bucket_summary() {
        let t = Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap();
        let result = ReplayResult {
            since: Some(t),
            until: Some(t),
            event_count: 5,
            buckets: vec![
                Bucket {
                    label: "CI runs".into(),
                    count: 3,
                },
                Bucket {
                    label: "Linear state changes".into(),
                    count: 2,
                },
            ],
            highlights: vec![],
            skipped_first_run: false,
        };
        let banner = result.banner();
        assert!(banner.contains("5 events"));
        assert!(banner.contains("3 CI runs"));
        assert!(banner.contains("2 Linear state changes"));
    }

    #[test]
    fn banner_handles_first_run() {
        let result = ReplayResult {
            skipped_first_run: true,
            ..Default::default()
        };
        assert!(result.banner().contains("first session"));
    }

    #[test]
    fn state_dir_uses_session_subdir() {
        let dir = state_dir_for_session("abc-123");
        let s = dir.to_string_lossy();
        assert!(s.contains("sentinel"));
        assert!(s.contains("state"));
        assert!(s.ends_with("abc-123"));
    }
}
