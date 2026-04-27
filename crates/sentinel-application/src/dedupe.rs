//! Coalescing buffer for channel events.
//!
//! When a burst of related webhook events arrives (e.g. a Linear bulk-assign
//! fires 50 `Issue.update` events in <2s), the session should wake once on the
//! final state — not once per event. This module provides a time-window
//! coalescer keyed on `(source, resource_id, event_type)` that collapses
//! repeated keys in a sliding quiet window and emits only the last value
//! after the window expires.
//!
//! # Time abstraction
//!
//! Tests inject a mock clock via [`Clock`] so the buffer logic is deterministic
//! without real sleeps. Production callers use [`SystemClock`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::channel_events::ChannelEvent;

/// Abstract clock so tests can advance time without sleeping.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Real monotonic clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Mock clock for tests. Time only advances when [`MockClock::advance`] is called.
#[derive(Debug, Clone)]
pub struct MockClock {
    inner: Arc<Mutex<Instant>>,
}

impl MockClock {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Instant::now())),
        }
    }

    pub fn advance(&self, d: Duration) {
        let mut g = self.inner.lock().expect("mock clock poisoned");
        *g += d;
    }
}

impl Default for MockClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MockClock {
    fn now(&self) -> Instant {
        *self.inner.lock().expect("mock clock poisoned")
    }
}

/// Dedup key — (source, resource_id, event_type).
///
/// - `source`: "linear", "github", "vercel", etc. Drawn from the webhook source.
/// - `resource_id`: the specific entity being affected (issue identifier, PR
///    number, etc). `None` when the event doesn't target a single resource, in
///    which case coalescing falls back to (source, event_type).
/// - `event_type`: "Issue.update", "check_run.completed", etc.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct DedupKey {
    pub source: String,
    pub resource_id: Option<String>,
    pub event_type: String,
}

impl DedupKey {
    pub fn new(
        source: impl Into<String>,
        resource_id: Option<String>,
        event_type: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            resource_id,
            event_type: event_type.into(),
        }
    }

    /// Attempt to derive a dedup key from a [`ChannelEvent`]'s meta map.
    ///
    /// Looks for `meta.source` + `meta.resource_id` + (meta.event_type || top-level event).
    /// Returns `None` if the event has no `source` — such events bypass dedup.
    pub fn from_event(event: &ChannelEvent) -> Option<Self> {
        let source = event
            .meta
            .get("source")
            .and_then(|v| v.as_str())
            .map(String::from)?;
        let resource_id = event
            .meta
            .get("resource_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let event_type = event
            .meta
            .get("event_type")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| event.event.clone());
        Some(Self::new(source, resource_id, event_type))
    }
}

/// A pending event entry held in the buffer.
struct Pending {
    /// Latest event payload (overwritten on each duplicate arrival).
    event: ChannelEvent,
    /// File path to delete when the coalesced event is finally emitted.
    /// We keep the *latest* path so that the earlier ones are consumed up-front.
    latest_path: Option<std::path::PathBuf>,
    /// Paths of earlier coalesced events — they are deleted on flush regardless
    /// of whether the final emit succeeds, so they don't re-drain.
    superseded_paths: Vec<std::path::PathBuf>,
    /// Number of events collapsed (including the final one). Useful for meta.
    count: u32,
    /// When the last arrival for this key happened. Used to determine whether
    /// the quiet window has passed.
    last_seen: Instant,
}

/// Coalescing buffer. Thread-safe: wrap with `Arc<Mutex<_>>` to share across
/// tasks. The buffer does not own a timer — callers flush by calling
/// [`Coalescer::flush_ready`] on a schedule (e.g. inside the file-watcher loop
/// or a periodic task).
pub struct Coalescer<C: Clock = SystemClock> {
    window: Duration,
    pending: HashMap<DedupKey, Pending>,
    clock: C,
}

impl Coalescer<SystemClock> {
    /// Create a coalescer with the default 3-second window and the real clock.
    pub fn new() -> Self {
        Self::with_window(Duration::from_secs(3))
    }

    /// Create a coalescer with a custom window and the real clock.
    pub fn with_window(window: Duration) -> Self {
        Self {
            window,
            pending: HashMap::new(),
            clock: SystemClock,
        }
    }
}

impl Default for Coalescer<SystemClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Clock> Coalescer<C> {
    /// Create a coalescer with a custom clock (for tests).
    pub fn with_clock(window: Duration, clock: C) -> Self {
        Self {
            window,
            pending: HashMap::new(),
            clock,
        }
    }

    /// Size of the currently buffered set. Exposed for tests/metrics.
    pub fn buffered_count(&self) -> usize {
        self.pending.len()
    }

    /// Record the arrival of an event. Returns:
    ///
    /// - `IngestOutcome::Coalesced { superseded }` — the event was collapsed
    ///    against an existing key; `superseded` is a path to a prior event
    ///    file that is now obsolete and should be deleted by the caller.
    /// - `IngestOutcome::Buffered` — first event for this key in the window,
    ///    buffered until the next flush.
    /// - `IngestOutcome::NotCoalescable` — event lacked a dedup key (e.g. no
    ///    `source` in meta). Caller should emit it directly without delay.
    pub fn ingest(
        &mut self,
        event: ChannelEvent,
        event_path: Option<std::path::PathBuf>,
    ) -> IngestOutcome {
        let Some(key) = DedupKey::from_event(&event) else {
            return IngestOutcome::NotCoalescable(event);
        };
        let now = self.clock.now();

        match self.pending.get_mut(&key) {
            Some(existing) => {
                let superseded = std::mem::take(&mut existing.latest_path);
                if let Some(ref p) = superseded {
                    existing.superseded_paths.push(p.clone());
                }
                existing.event = event;
                existing.latest_path = event_path;
                existing.count += 1;
                existing.last_seen = now;
                IngestOutcome::Coalesced { superseded }
            }
            None => {
                self.pending.insert(
                    key,
                    Pending {
                        event,
                        latest_path: event_path,
                        superseded_paths: Vec::new(),
                        count: 1,
                        last_seen: now,
                    },
                );
                IngestOutcome::Buffered
            }
        }
    }

    /// Drain all keys whose quiet window has elapsed and return them as
    /// emittable events. Keys seen more recently than `now - window` remain
    /// buffered.
    ///
    /// The returned [`ReadyEvent`]s contain a `coalesce_count` annotation
    /// merged into their meta so downstream observers can see how many were
    /// collapsed.
    pub fn flush_ready(&mut self) -> Vec<ReadyEvent> {
        let now = self.clock.now();
        let window = self.window;
        let mut ready = Vec::new();

        let keys: Vec<DedupKey> = self
            .pending
            .iter()
            .filter(|(_, p)| now.duration_since(p.last_seen) >= window)
            .map(|(k, _)| k.clone())
            .collect();

        for key in keys {
            if let Some(mut pending) = self.pending.remove(&key) {
                if pending.count > 1 {
                    pending.event.meta.insert(
                        "coalesce_count".to_string(),
                        serde_json::Value::from(pending.count),
                    );
                }
                ready.push(ReadyEvent {
                    key,
                    event: pending.event,
                    latest_path: pending.latest_path,
                    superseded_paths: pending.superseded_paths,
                    coalesce_count: pending.count,
                });
            }
        }

        ready
    }

    /// Force-flush all pending keys regardless of quiet window. Used at
    /// session shutdown so nothing is lost.
    pub fn flush_all(&mut self) -> Vec<ReadyEvent> {
        let mut ready = Vec::new();
        for (key, mut pending) in self.pending.drain() {
            if pending.count > 1 {
                pending.event.meta.insert(
                    "coalesce_count".to_string(),
                    serde_json::Value::from(pending.count),
                );
            }
            ready.push(ReadyEvent {
                key,
                event: pending.event,
                latest_path: pending.latest_path,
                superseded_paths: pending.superseded_paths,
                coalesce_count: pending.count,
            });
        }
        ready
    }
}

/// Outcome of a single [`Coalescer::ingest`] call.
pub enum IngestOutcome {
    /// Event was collapsed against an existing key. `superseded` is the path
    /// of the prior event file (if any) that the caller should delete so it
    /// doesn't re-drain.
    Coalesced {
        superseded: Option<std::path::PathBuf>,
    },
    /// New key — buffered until the quiet window elapses.
    Buffered,
    /// Event lacked a dedup key. The caller owns the event back and should
    /// emit it directly.
    NotCoalescable(ChannelEvent),
}

/// An event that is ready to emit (quiet window elapsed).
pub struct ReadyEvent {
    pub key: DedupKey,
    pub event: ChannelEvent,
    /// Path of the latest on-disk file for this event, if any.
    pub latest_path: Option<std::path::PathBuf>,
    /// Paths of earlier collapsed events — delete on emit.
    pub superseded_paths: Vec<std::path::PathBuf>,
    /// Total number of events collapsed into this one (including the final).
    pub coalesce_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_event(
        name: &str,
        source: &str,
        resource_id: Option<&str>,
        event_type: &str,
    ) -> ChannelEvent {
        let mut meta = serde_json::Map::new();
        meta.insert("source".into(), json!(source));
        if let Some(r) = resource_id {
            meta.insert("resource_id".into(), json!(r));
        }
        meta.insert("event_type".into(), json!(event_type));
        ChannelEvent {
            event: name.into(),
            summary: format!("{name} {}", resource_id.unwrap_or("?")),
            ts: chrono::Utc::now().to_rfc3339(),
            session_id: None,
            project: None,
            source_agent: None,
            meta,
        }
    }

    #[test]
    fn coalesces_burst_within_window_into_single_emission() {
        let clock = MockClock::new();
        let mut c = Coalescer::with_clock(Duration::from_secs(3), clock.clone());

        // 10 Issue.update events for FPCRM-329 within 100ms
        for i in 0..10 {
            let mut ev = make_event(
                "linear.issue.update",
                "linear",
                Some("FPCRM-329"),
                "Issue.update",
            );
            ev.summary = format!("update #{i}");
            let _ = c.ingest(ev, None);
            clock.advance(Duration::from_millis(10));
        }

        // Before quiet window → nothing ready yet
        let ready_early = c.flush_ready();
        assert!(ready_early.is_empty(), "expected no flush before window");
        assert_eq!(c.buffered_count(), 1);

        // Advance past the 3s window
        clock.advance(Duration::from_secs(3));
        let ready = c.flush_ready();
        assert_eq!(ready.len(), 1, "expected a single coalesced emission");
        let out = &ready[0];
        assert_eq!(out.coalesce_count, 10);
        assert_eq!(out.event.summary, "update #9"); // last wins
        // Meta should be annotated with coalesce_count.
        assert_eq!(
            out.event.meta.get("coalesce_count").and_then(|v| v.as_u64()),
            Some(10)
        );
    }

    #[test]
    fn distinct_keys_do_not_coalesce() {
        let clock = MockClock::new();
        let mut c = Coalescer::with_clock(Duration::from_secs(3), clock.clone());

        c.ingest(
            make_event("linear.issue.update", "linear", Some("FPCRM-1"), "Issue.update"),
            None,
        );
        c.ingest(
            make_event("linear.issue.update", "linear", Some("FPCRM-2"), "Issue.update"),
            None,
        );
        c.ingest(
            make_event("gh.check_run", "github", Some("123"), "check_run.completed"),
            None,
        );

        assert_eq!(c.buffered_count(), 3);
        clock.advance(Duration::from_secs(3));
        let ready = c.flush_ready();
        assert_eq!(ready.len(), 3);
        for e in &ready {
            assert_eq!(e.coalesce_count, 1);
            // Single-event emissions do NOT have coalesce_count annotation.
            assert!(e.event.meta.get("coalesce_count").is_none());
        }
    }

    #[test]
    fn fresh_arrival_extends_window_sliding() {
        // Key arrives at t=0, again at t=2s, flush at t=2.5s — not ready.
        // Flush at t=5.1s (3s after last arrival) — ready.
        let clock = MockClock::new();
        let mut c = Coalescer::with_clock(Duration::from_secs(3), clock.clone());

        c.ingest(
            make_event("linear.issue.update", "linear", Some("X"), "Issue.update"),
            None,
        );
        clock.advance(Duration::from_secs(2));
        c.ingest(
            make_event("linear.issue.update", "linear", Some("X"), "Issue.update"),
            None,
        );
        // t=2.5s — only 0.5s since last arrival, window not elapsed
        clock.advance(Duration::from_millis(500));
        assert!(c.flush_ready().is_empty());

        // t=5.1s — 3.1s since last arrival
        clock.advance(Duration::from_millis(2600));
        let ready = c.flush_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].coalesce_count, 2);
    }

    #[test]
    fn events_without_source_are_not_coalescable() {
        let clock = MockClock::new();
        let mut c = Coalescer::with_clock(Duration::from_secs(3), clock.clone());

        let mut ev = ChannelEvent {
            event: "agent_completed".into(),
            summary: "done".into(),
            ts: chrono::Utc::now().to_rfc3339(),
            session_id: None,
            project: None,
            source_agent: None,
            meta: serde_json::Map::new(),
        };
        ev.meta.insert("project".into(), json!("sentinel"));

        match c.ingest(ev, None) {
            IngestOutcome::NotCoalescable(_) => {}
            _ => panic!("events without `meta.source` should be flagged NotCoalescable"),
        }
        assert_eq!(c.buffered_count(), 0);
    }

    #[test]
    fn superseded_paths_are_tracked_for_cleanup() {
        let clock = MockClock::new();
        let mut c = Coalescer::with_clock(Duration::from_secs(3), clock.clone());

        let p1 = std::path::PathBuf::from("/tmp/ev_1.json");
        let p2 = std::path::PathBuf::from("/tmp/ev_2.json");
        let p3 = std::path::PathBuf::from("/tmp/ev_3.json");

        c.ingest(
            make_event("linear.issue.update", "linear", Some("X"), "Issue.update"),
            Some(p1.clone()),
        );
        let o2 = c.ingest(
            make_event("linear.issue.update", "linear", Some("X"), "Issue.update"),
            Some(p2.clone()),
        );
        match o2 {
            IngestOutcome::Coalesced { superseded } => assert_eq!(superseded, Some(p1.clone())),
            _ => panic!("second ingest should coalesce"),
        }
        let o3 = c.ingest(
            make_event("linear.issue.update", "linear", Some("X"), "Issue.update"),
            Some(p3.clone()),
        );
        match o3 {
            IngestOutcome::Coalesced { superseded } => assert_eq!(superseded, Some(p2.clone())),
            _ => panic!("third ingest should coalesce"),
        }

        clock.advance(Duration::from_secs(3));
        let ready = c.flush_ready();
        assert_eq!(ready.len(), 1);
        let out = &ready[0];
        assert_eq!(out.latest_path, Some(p3));
        // `superseded_paths` accumulates p1 and p2 — p2 was captured during the
        // second Coalesced result, so the caller already removed it at that
        // point and we don't double-list it here. What *does* live on the
        // entry is p1 (captured when p2 was written) and p2 (captured when p3
        // was written).
        assert_eq!(out.superseded_paths, vec![p1, p2]);
    }

    #[test]
    fn flush_all_drains_regardless_of_window() {
        let clock = MockClock::new();
        let mut c = Coalescer::with_clock(Duration::from_secs(3), clock.clone());

        c.ingest(
            make_event("linear.issue.update", "linear", Some("A"), "Issue.update"),
            None,
        );
        c.ingest(
            make_event("linear.issue.update", "linear", Some("B"), "Issue.update"),
            None,
        );

        // No time advance — flush_ready would return nothing.
        assert!(c.flush_ready().is_empty());

        let all = c.flush_all();
        assert_eq!(all.len(), 2);
        assert_eq!(c.buffered_count(), 0);
    }

    #[test]
    fn key_from_event_falls_back_to_top_level_event_when_meta_event_type_missing() {
        let mut meta = serde_json::Map::new();
        meta.insert("source".into(), json!("linear"));
        meta.insert("resource_id".into(), json!("FPCRM-1"));
        let ev = ChannelEvent {
            event: "linear.issue.update".into(),
            summary: "hi".into(),
            ts: chrono::Utc::now().to_rfc3339(),
            session_id: None,
            project: None,
            source_agent: None,
            meta,
        };
        let k = DedupKey::from_event(&ev).expect("should derive");
        assert_eq!(k.event_type, "linear.issue.update");
    }

    #[test]
    fn key_without_resource_id_still_coalesces_by_source_and_type() {
        let clock = MockClock::new();
        let mut c = Coalescer::with_clock(Duration::from_secs(3), clock.clone());

        // e.g. "deployment.error" without a resource_id — all deployment errors
        // from the same source coalesce. That's intentional; noisy error
        // storms collapse into a single wake.
        let mk = || {
            let mut meta = serde_json::Map::new();
            meta.insert("source".into(), json!("vercel"));
            meta.insert("event_type".into(), json!("deployment.error"));
            ChannelEvent {
                event: "deploy.error".into(),
                summary: "deploy failed".into(),
                ts: chrono::Utc::now().to_rfc3339(),
                session_id: None,
                project: None,
                source_agent: None,
                meta,
            }
        };

        c.ingest(mk(), None);
        c.ingest(mk(), None);
        c.ingest(mk(), None);
        assert_eq!(c.buffered_count(), 1);

        clock.advance(Duration::from_secs(3));
        let ready = c.flush_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].coalesce_count, 3);
    }
}
