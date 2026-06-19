//! WIP-by-Stage Live View (SENTINEL-8)
//!
//! Persists a per-team-per-state count of in-flight Linear tickets to
//! `~/.claude/sentinel/state/wip-snapshot.json`. The poller (Linear API
//! client + 5-minute cron) is built in a follow-up task; this module owns
//! the data structures, the on-disk format, the bottleneck-detection rules,
//! and the read API surfaced through `mcp__sentinel__get_wip_snapshot`.
//!
//! ## On-disk schema
//!
//! ```json
//! {
//!   "captured_at": "2026-05-02T19:30:00Z",
//!   "teams": {
//!     "FPCRM": {
//!       "Backlog": 12,
//!       "Todo": 4,
//!       "In Progress": 7,
//!       "Code Review": 3,
//!       "QA Testing": 2
//!     }
//!   },
//!   "bottlenecks": [
//!     {
//!       "team": "FPCRM",
//!       "stage": "Code Review",
//!       "kind": "review_clog",
//!       "detail": "WIP=14, 7d throughput=1.0, projected wait 14d (>14d limit)"
//!     }
//!   ]
//! }
//! ```
//!
//! ## Bottleneck rules
//!
//! Two simple rules from the SEN-8 spec:
//!   * `review_clog`: Code Review WIP / 7d throughput > 14 days
//!   * `qa_ceiling`:  any ticket sitting in QA Testing > 5 days
//!
//! The QA-ceiling rule needs per-ticket dwell time and lives in the future
//! poller; the snapshot type carries the field so consumers (local API, MCP
//! tool) can render it consistently once populated.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Stable per-team-per-state count snapshot. `BTreeMap` keeps the JSON output
/// deterministic so file diffs stay readable across snapshots.
pub type StateCounts = BTreeMap<String, u32>;

/// One bottleneck flag emitted by `compute_bottlenecks`. Surfaced both in the
/// snapshot JSON and (later) as a sentinel channel push when a new bottleneck
/// appears between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BottleneckFlag {
    pub team: String,
    pub stage: String,
    /// Stable identifier — `review_clog` or `qa_ceiling` today; new rule
    /// kinds add new variants.
    pub kind: String,
    /// Human-readable detail string for local display + push payload.
    pub detail: String,
}

/// Top-level WIP snapshot. One file per workspace, refreshed every 5 min by
/// the poller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WipSnapshot {
    /// RFC3339 capture time. Consumers compare this against `now` to detect
    /// stale snapshots (poller down, etc.).
    pub captured_at: String,
    /// `team_key → state_name → count`. Empty teams are dropped before write
    /// so we don't carry inactive workspaces around.
    pub teams: BTreeMap<String, StateCounts>,
    /// Bottlenecks identified at capture time. Empty when nothing tripped.
    pub bottlenecks: Vec<BottleneckFlag>,
}

impl WipSnapshot {
    /// Build an empty snapshot anchored at `now`. Mostly useful for tests and
    /// for the poller's first run before any data lands.
    #[must_use]
    pub fn empty_at(now: DateTime<Utc>) -> Self {
        Self {
            captured_at: now.to_rfc3339(),
            teams: BTreeMap::new(),
            bottlenecks: Vec::new(),
        }
    }

    /// Total in-flight ticket count across all teams + stages.
    #[must_use]
    pub fn total_wip(&self) -> u32 {
        self.teams
            .values()
            .flat_map(BTreeMap::values)
            .copied()
            .sum()
    }

    /// Convenience accessor for one team's stage count, returning 0 when
    /// the team or stage is absent.
    #[must_use]
    pub fn count(&self, team: &str, stage: &str) -> u32 {
        self.teams
            .get(team)
            .and_then(|s| s.get(stage))
            .copied()
            .unwrap_or(0)
    }
}

/// 7-day throughput input for the `review_clog` rule. Keyed by team key.
pub type ThroughputPerTeam = BTreeMap<String, f64>;

/// Apply bottleneck-detection rules to a snapshot. Pure function — no I/O.
///
/// `throughput_7d` should give tickets-completed-per-day over the prior 7
/// days for each team, sourced from `cycle-time.jsonl`. The poller wires
/// that in; tests pass it directly.
///
/// Rule details:
///   * `review_clog`: Code Review WIP > 0 AND 7d throughput > 0 AND
///     `wip / throughput > 14` days. Skipped silently if either input is
///     zero (no clog if nothing in review; cannot project if no throughput).
///
/// QA ceiling is not implemented here yet — needs per-ticket dwell time
/// from the live poller — but the public API is shaped to receive it.
#[must_use]
pub fn compute_bottlenecks(
    snapshot: &WipSnapshot,
    throughput_7d: &ThroughputPerTeam,
) -> Vec<BottleneckFlag> {
    const REVIEW_LIMIT_DAYS: f64 = 14.0;
    const REVIEW_STAGE: &str = "Code Review";

    let mut out = Vec::new();
    for (team, counts) in &snapshot.teams {
        let wip = counts.get(REVIEW_STAGE).copied().unwrap_or(0);
        if wip == 0 {
            continue;
        }
        let throughput = throughput_7d.get(team).copied().unwrap_or(0.0);
        if throughput <= 0.0 {
            continue;
        }
        let projected = f64::from(wip) / throughput;
        if projected > REVIEW_LIMIT_DAYS {
            out.push(BottleneckFlag {
                team: team.clone(),
                stage: REVIEW_STAGE.into(),
                kind: "review_clog".into(),
                detail: format!(
                    "WIP={wip}, 7d throughput={throughput:.1}, projected wait {projected:.1}d (>14d limit)"
                ),
            });
        }
    }
    out
}

/// Resolve `~/.claude/sentinel/state/wip-snapshot.json`.
///
/// Returns `None` when `dirs::home_dir()` cannot resolve a home directory.
#[must_use]
pub fn snapshot_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        crate::hooks::sentinel_dir(&home)
            .join("state")
            .join("wip-snapshot.json"),
    )
}

/// Atomically write a snapshot to the canonical path. Uses tmp-file +
/// rename to avoid readers seeing a half-written JSON document.
///
/// # Errors
///
/// Returns an `io::Error` if the home directory cannot be resolved, the
/// state dir cannot be created, or the rename fails.
pub fn write(snapshot: &WipSnapshot) -> std::io::Result<()> {
    let path = snapshot_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not resolve home directory for wip-snapshot.json",
        )
    })?;
    write_to(&path, snapshot)
}

/// Write override that targets a caller-supplied path. Used by tests and
/// by callers that want to redirect the snapshot stream.
///
/// # Errors
///
/// Same as [`write`].
pub fn write_to(path: &Path, snapshot: &WipSnapshot) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(snapshot)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Read the canonical snapshot. Returns `Ok(None)` when the file does not
/// yet exist (poller hasn't run yet) so callers can render a "no data
/// captured yet" state instead of erroring.
///
/// # Errors
///
/// Returns an `io::Error` for any read or parse failure other than
/// `NotFound`.
pub fn read() -> std::io::Result<Option<WipSnapshot>> {
    let Some(path) = snapshot_path() else {
        return Ok(None);
    };
    read_from(&path)
}

/// Read override that targets a caller-supplied path.
///
/// # Errors
///
/// Same as [`read`].
pub fn read_from(path: &Path) -> std::io::Result<Option<WipSnapshot>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let snap: WipSnapshot = serde_json::from_slice(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(Some(snap))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 2, 19, 30, 0).unwrap()
    }

    fn snap_with(team: &str, stage: &str, count: u32) -> WipSnapshot {
        let mut snap = WipSnapshot::empty_at(fixed_now());
        let mut counts = StateCounts::new();
        counts.insert(stage.into(), count);
        snap.teams.insert(team.into(), counts);
        snap
    }

    #[test]
    fn total_wip_sums_across_teams_and_stages() {
        let mut snap = WipSnapshot::empty_at(fixed_now());
        let mut a = StateCounts::new();
        a.insert("Todo".into(), 3);
        a.insert("In Progress".into(), 4);
        let mut b = StateCounts::new();
        b.insert("Code Review".into(), 5);
        snap.teams.insert("FPCRM".into(), a);
        snap.teams.insert("LEG".into(), b);
        assert_eq!(snap.total_wip(), 12);
    }

    #[test]
    fn count_returns_zero_for_missing_team_or_stage() {
        let snap = snap_with("FPCRM", "In Progress", 7);
        assert_eq!(snap.count("FPCRM", "In Progress"), 7);
        assert_eq!(snap.count("FPCRM", "Done"), 0);
        assert_eq!(snap.count("LEG", "In Progress"), 0);
    }

    #[test]
    fn review_clog_fires_when_wip_over_throughput_exceeds_14_days() {
        let snap = snap_with("FPCRM", "Code Review", 16);
        let mut tput = ThroughputPerTeam::new();
        tput.insert("FPCRM".into(), 1.0); // 16 / 1.0 = 16 days > 14
        let flags = compute_bottlenecks(&snap, &tput);
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].kind, "review_clog");
        assert_eq!(flags[0].team, "FPCRM");
        assert_eq!(flags[0].stage, "Code Review");
        assert!(flags[0].detail.contains("16"));
    }

    #[test]
    fn review_clog_silent_at_exactly_14_days() {
        let snap = snap_with("FPCRM", "Code Review", 14);
        let mut tput = ThroughputPerTeam::new();
        tput.insert("FPCRM".into(), 1.0); // 14 / 1.0 = 14, not > 14
        assert!(compute_bottlenecks(&snap, &tput).is_empty());
    }

    #[test]
    fn review_clog_silent_when_no_review_wip() {
        let snap = snap_with("FPCRM", "In Progress", 50);
        let mut tput = ThroughputPerTeam::new();
        tput.insert("FPCRM".into(), 0.1);
        assert!(compute_bottlenecks(&snap, &tput).is_empty());
    }

    #[test]
    fn review_clog_silent_when_throughput_zero_or_missing() {
        let snap = snap_with("FPCRM", "Code Review", 5);
        let empty = ThroughputPerTeam::new();
        assert!(compute_bottlenecks(&snap, &empty).is_empty());

        let mut zero = ThroughputPerTeam::new();
        zero.insert("FPCRM".into(), 0.0);
        assert!(compute_bottlenecks(&snap, &zero).is_empty());
    }

    #[test]
    fn review_clog_per_team_independence() {
        let mut snap = WipSnapshot::empty_at(fixed_now());
        let mut a = StateCounts::new();
        a.insert("Code Review".into(), 20);
        let mut b = StateCounts::new();
        b.insert("Code Review".into(), 2);
        snap.teams.insert("FPCRM".into(), a);
        snap.teams.insert("LEG".into(), b);

        let mut tput = ThroughputPerTeam::new();
        tput.insert("FPCRM".into(), 1.0); // 20d → flag
        tput.insert("LEG".into(), 1.0); //   2d → silent

        let flags = compute_bottlenecks(&snap, &tput);
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].team, "FPCRM");
    }

    #[test]
    fn write_and_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("wip-snapshot.json");

        let mut original = snap_with("FPCRM", "Code Review", 4);
        original.bottlenecks.push(BottleneckFlag {
            team: "FPCRM".into(),
            stage: "Code Review".into(),
            kind: "review_clog".into(),
            detail: "test".into(),
        });

        write_to(&path, &original).unwrap();
        let loaded = read_from(&path).unwrap().expect("snapshot should exist");
        assert_eq!(loaded, original);
    }

    #[test]
    fn read_returns_none_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.json");
        assert!(read_from(&path).unwrap().is_none());
    }

    #[test]
    fn write_uses_atomic_rename_no_partial_files_after_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("wip-snapshot.json");
        let snap = snap_with("FPCRM", "Todo", 1);
        write_to(&path, &snap).unwrap();

        // No leftover .tmp file.
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists());
        assert!(path.exists());
    }
}
