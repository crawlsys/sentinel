//! Cycle Time Analytics — SEN-2 (percentile-calibrated stale-ticket
//! thresholds) + SEN-17 (per-stage cycle time breakdown).
//!
//! Both metrics consume the same upstream: `cycle-time.jsonl` rows produced
//! by SEN-1. This module is the single read path for both, plus the two
//! aggregation surfaces:
//!
//! * [`compute_stage_thresholds`] — for each (team, from_stage) pair,
//!   emit p50/p75/p90 of the elapsed hours into the next stage. The
//!   stale-ticket hook reads these to replace its hardcoded thresholds.
//! * [`compute_per_stage_breakdown`] — for each (team, stage) pair, emit
//!   mean / p50 / p75 / sample_count over the 30-day window. The
//!   dashboard's `CycleTimeBreakdown` organism + master enterprise
//!   dashboard (SEN-19) read this.
//!
//! ## Schema (consumed)
//!
//! Each line of `cycle-time.jsonl` is a `CycleTimeEvent` carrying the
//! transition into `to_state`. We derive elapsed time per pair by sorting
//! by `(issue_id, timestamp)` and diffing successive transitions of the
//! same issue. The hours-into-stage attributed to `from_state` is exactly
//! the same statistic the dashboard's `apps/dashboard/src/application/
//! get-dora-tier.ts` computes inline — keeping it server-side here means
//! the Rust collectors can emit percentile snapshots without re-running
//! the same logic on every dashboard render.
//!
//! ## Schema (emitted)
//!
//! Both summaries write JSON files under `~/.claude/sentinel/metrics/`:
//!
//! * `stage-thresholds-summary.json` (SEN-2)
//! * `per-stage-cycle-time-summary.json` (SEN-17)

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::cycle_time::CycleTimeEvent;

/// Default analysis window: 30 days. Matches the dashboard's DORA window
/// so percentile bases stay aligned across the two surfaces.
pub const DEFAULT_WINDOW_DAYS: i64 = 30;

/// One (team, from_stage) entry in the stale-threshold table (SEN-2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageThreshold {
    /// Team identifier (e.g. `FPCRM`). May be `null` when the upstream
    /// webhook didn't carry the team object.
    pub team: Option<String>,
    /// The stage tickets are leaving (`Code Review`, `In Progress`, …).
    pub from_stage: String,
    /// Number of paired transitions used to derive the percentiles.
    pub sample_count: usize,
    /// 50th percentile of hours-in-`from_stage` before transitioning out.
    pub p50_hours: f64,
    /// 75th percentile (the operational stale floor).
    pub p75_hours: f64,
    /// 90th percentile (the operational stale ceiling — anything above
    /// this is genuinely stuck).
    pub p90_hours: f64,
}

/// Summary file written to `stage-thresholds-summary.json` (SEN-2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageThresholdSummary {
    pub generated_at: String,
    pub window_days: i64,
    pub events_scanned: u64,
    pub pairs_used: u64,
    pub thresholds: Vec<StageThreshold>,
}

/// One (team, stage) entry in the per-stage cycle time breakdown (SEN-17).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageBreakdown {
    pub team: Option<String>,
    /// The stage being measured. `hours` is time **spent in** this stage
    /// before exiting — same shape as SEN-2 but keyed by `from_stage`.
    pub stage: String,
    pub sample_count: usize,
    pub mean_hours: f64,
    pub p50_hours: f64,
    pub p75_hours: f64,
}

/// Summary file written to `per-stage-cycle-time-summary.json` (SEN-17).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerStageBreakdownSummary {
    pub generated_at: String,
    pub window_days: i64,
    pub events_scanned: u64,
    pub pairs_used: u64,
    pub per_stage: Vec<StageBreakdown>,
}

/// Read every event from `cycle-time.jsonl`. Missing file → empty vec;
/// malformed lines are logged + skipped.
///
/// # Errors
/// IO errors other than file-not-found.
pub fn read_events(path: &Path) -> Result<Vec<CycleTimeEvent>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let mut out = Vec::new();
    for (n, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<CycleTimeEvent>(line) {
            Ok(e) => out.push(e),
            Err(e) => {
                tracing::warn!(
                    file = %path.display(),
                    line_no = n + 1,
                    error = %e,
                    "cycle-time.jsonl: skipping malformed line"
                );
            }
        }
    }
    Ok(out)
}

/// Internal: pair adjacent transitions of the same `issue_id` to derive
/// hours-in-`from_stage`. Returns `(team, from_stage, hours)` tuples for
/// every paired transition where both timestamps parsed.
///
/// We require `events` to be re-sortable here (we re-sort by issue + ts);
/// callers can pass whatever order the JSONL was read in.
fn pair_transitions(events: &[CycleTimeEvent]) -> Vec<(Option<String>, String, f64)> {
    let mut by_issue: BTreeMap<String, Vec<&CycleTimeEvent>> = BTreeMap::new();
    for e in events {
        by_issue.entry(e.issue_id.clone()).or_default().push(e);
    }
    let mut out = Vec::new();
    for events_of_issue in by_issue.values_mut() {
        events_of_issue.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        let mut prev: Option<(&CycleTimeEvent, DateTime<Utc>)> = None;
        for ev in events_of_issue.iter() {
            let ts = match DateTime::parse_from_rfc3339(&ev.timestamp) {
                Ok(t) => t.with_timezone(&Utc),
                Err(_) => {
                    prev = None;
                    continue;
                }
            };
            if let Some((p, p_ts)) = prev {
                let diff_secs = (ts - p_ts).num_seconds().max(0);
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_sign_loss,
                    clippy::cast_possible_truncation
                )]
                let hours = (diff_secs as f64) / 3600.0;
                // Attribute hours to the stage the issue was leaving.
                // p.to_state is the state the issue was IN between the
                // two transitions, so that's the "from_stage" we credit.
                out.push((ev.team.clone(), p.to_state.clone(), hours));
            }
            prev = Some((ev, ts));
        }
    }
    out
}

/// SEN-2: per-(team, from_stage) p50/p75/p90 of dwell time, computed over
/// the last `window_days` days of `cycle-time.jsonl`. `now` is injected
/// for test determinism.
///
/// # Errors
/// Returns the read or write error if the JSONL can't be loaded or the
/// summary file can't be persisted.
pub fn compute_stage_thresholds_at(
    cycle_time_path: &Path,
    summary_path: &Path,
    now: DateTime<Utc>,
    window_days: i64,
) -> Result<StageThresholdSummary> {
    let events = read_events(cycle_time_path)?;
    let cutoff = now - Duration::days(window_days);

    let in_window: Vec<CycleTimeEvent> = events
        .iter()
        .filter(|e| {
            DateTime::parse_from_rfc3339(&e.timestamp)
                .ok()
                .is_some_and(|t| t.with_timezone(&Utc) >= cutoff)
        })
        .cloned()
        .collect();

    let pairs = pair_transitions(&in_window);

    let mut buckets: BTreeMap<(Option<String>, String), Vec<f64>> = BTreeMap::new();
    for (team, from_stage, hours) in &pairs {
        buckets
            .entry((team.clone(), from_stage.clone()))
            .or_default()
            .push(*hours);
    }

    let mut thresholds = Vec::with_capacity(buckets.len());
    for ((team, from_stage), mut samples) in buckets {
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        thresholds.push(StageThreshold {
            team,
            from_stage,
            sample_count: samples.len(),
            p50_hours: percentile(&samples, 50),
            p75_hours: percentile(&samples, 75),
            p90_hours: percentile(&samples, 90),
        });
    }

    let summary = StageThresholdSummary {
        generated_at: now.to_rfc3339(),
        window_days,
        events_scanned: u64::try_from(events.len()).unwrap_or(u64::MAX),
        pairs_used: u64::try_from(pairs.len()).unwrap_or(u64::MAX),
        thresholds,
    };

    write_summary(summary_path, &summary).context("write StageThresholdSummary")?;
    Ok(summary)
}

/// `Utc::now()` wrapper around [`compute_stage_thresholds_at`].
///
/// # Errors
/// Same as [`compute_stage_thresholds_at`].
pub fn compute_stage_thresholds(
    cycle_time_path: &Path,
    summary_path: &Path,
) -> Result<StageThresholdSummary> {
    compute_stage_thresholds_at(cycle_time_path, summary_path, Utc::now(), DEFAULT_WINDOW_DAYS)
}

/// SEN-17: per-(team, stage) mean + p50 + p75 of dwell time. Same input
/// pairing as [`compute_stage_thresholds_at`] but with different summary
/// statistics tailored to the dashboard's breakdown view.
///
/// # Errors
/// Returns the read or write error if the JSONL can't be loaded or the
/// summary file can't be persisted.
pub fn compute_per_stage_breakdown_at(
    cycle_time_path: &Path,
    summary_path: &Path,
    now: DateTime<Utc>,
    window_days: i64,
) -> Result<PerStageBreakdownSummary> {
    let events = read_events(cycle_time_path)?;
    let cutoff = now - Duration::days(window_days);
    let in_window: Vec<CycleTimeEvent> = events
        .iter()
        .filter(|e| {
            DateTime::parse_from_rfc3339(&e.timestamp)
                .ok()
                .is_some_and(|t| t.with_timezone(&Utc) >= cutoff)
        })
        .cloned()
        .collect();
    let pairs = pair_transitions(&in_window);

    let mut buckets: BTreeMap<(Option<String>, String), Vec<f64>> = BTreeMap::new();
    for (team, stage, hours) in &pairs {
        buckets
            .entry((team.clone(), stage.clone()))
            .or_default()
            .push(*hours);
    }

    let mut per_stage = Vec::with_capacity(buckets.len());
    for ((team, stage), mut samples) in buckets {
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        #[allow(clippy::cast_precision_loss)]
        let mean = if samples.is_empty() {
            0.0
        } else {
            samples.iter().sum::<f64>() / samples.len() as f64
        };
        per_stage.push(StageBreakdown {
            team,
            stage,
            sample_count: samples.len(),
            mean_hours: mean,
            p50_hours: percentile(&samples, 50),
            p75_hours: percentile(&samples, 75),
        });
    }

    let summary = PerStageBreakdownSummary {
        generated_at: now.to_rfc3339(),
        window_days,
        events_scanned: u64::try_from(events.len()).unwrap_or(u64::MAX),
        pairs_used: u64::try_from(pairs.len()).unwrap_or(u64::MAX),
        per_stage,
    };

    write_summary(summary_path, &summary).context("write PerStageBreakdownSummary")?;
    Ok(summary)
}

/// `Utc::now()` wrapper around [`compute_per_stage_breakdown_at`].
///
/// # Errors
/// Same as [`compute_per_stage_breakdown_at`].
pub fn compute_per_stage_breakdown(
    cycle_time_path: &Path,
    summary_path: &Path,
) -> Result<PerStageBreakdownSummary> {
    compute_per_stage_breakdown_at(cycle_time_path, summary_path, Utc::now(), DEFAULT_WINDOW_DAYS)
}

/// Inclusive-rank percentile over a pre-sorted slice. Empty → 0.0.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
fn percentile(sorted: &[f64], p: u32) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let rank = ((n - 1) as f64 * f64::from(p) / 100.0).round() as usize;
    sorted[rank.min(n - 1)]
}

fn write_summary<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(value).context("serialize summary")?;
    fs::write(path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cycle_time::append_to;
    use chrono::TimeZone;

    fn ev(issue: &str, team: Option<&str>, from: Option<&str>, to: &str, ts: &str) -> CycleTimeEvent {
        CycleTimeEvent {
            issue_id: issue.to_string(),
            team: team.map(str::to_string),
            from_state: from.map(str::to_string),
            to_state: to.to_string(),
            timestamp: ts.to_string(),
            estimate: None,
            priority: None,
            labels: vec![],
        }
    }

    #[test]
    fn read_missing_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("missing.jsonl");
        assert!(read_events(&p).unwrap().is_empty());
    }

    #[test]
    fn read_skips_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("cycle-time.jsonl");
        append_to(&p, &ev("X-1", Some("X"), Some("Todo"), "In Progress", "2026-05-01T10:00:00Z")).unwrap();
        std::fs::write(
            &p,
            format!(
                "{}\n{}\n{}\n",
                serde_json::to_string(&ev("X-1", Some("X"), Some("Todo"), "In Progress", "2026-05-01T10:00:00Z")).unwrap(),
                "{not-json",
                serde_json::to_string(&ev("X-1", Some("X"), Some("In Progress"), "Code Review", "2026-05-01T12:00:00Z")).unwrap(),
            ),
        )
        .unwrap();
        let got = read_events(&p).unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn pair_transitions_diffs_adjacent_events_per_issue() {
        // Two events for X-1: enter In Progress at T, enter Code Review at T+3h.
        // Pair attributes 3h to "In Progress".
        let events = vec![
            ev("X-1", Some("X"), Some("Todo"), "In Progress", "2026-05-01T10:00:00Z"),
            ev("X-1", Some("X"), Some("In Progress"), "Code Review", "2026-05-01T13:00:00Z"),
        ];
        let pairs = pair_transitions(&events);
        assert_eq!(pairs.len(), 1);
        let (team, stage, hours) = &pairs[0];
        assert_eq!(team.as_deref(), Some("X"));
        assert_eq!(stage, "In Progress");
        assert!((hours - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pair_transitions_isolates_per_issue() {
        // Two issues, each with two events. Pair count should be 2.
        let events = vec![
            ev("X-1", Some("X"), Some("Todo"), "In Progress", "2026-05-01T10:00:00Z"),
            ev("Y-1", Some("Y"), Some("Todo"), "In Progress", "2026-05-01T11:00:00Z"),
            ev("X-1", Some("X"), Some("In Progress"), "Code Review", "2026-05-01T12:00:00Z"),
            ev("Y-1", Some("Y"), Some("In Progress"), "Code Review", "2026-05-01T14:00:00Z"),
        ];
        let pairs = pair_transitions(&events);
        assert_eq!(pairs.len(), 2);
        // Each pair attributes 2h (X) and 3h (Y) to "In Progress".
        let x = pairs.iter().find(|(t, _, _)| t.as_deref() == Some("X")).unwrap();
        let y = pairs.iter().find(|(t, _, _)| t.as_deref() == Some("Y")).unwrap();
        assert!((x.2 - 2.0).abs() < f64::EPSILON);
        assert!((y.2 - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_stage_thresholds_empty_input() {
        let tmp = tempfile::tempdir().unwrap();
        let in_path = tmp.path().join("cycle-time.jsonl");
        let out_path = tmp.path().join("summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        let s = compute_stage_thresholds_at(&in_path, &out_path, now, 30).unwrap();
        assert_eq!(s.events_scanned, 0);
        assert_eq!(s.pairs_used, 0);
        assert!(s.thresholds.is_empty());
    }

    #[test]
    fn compute_stage_thresholds_buckets_by_team_and_stage() {
        let tmp = tempfile::tempdir().unwrap();
        let in_path = tmp.path().join("cycle-time.jsonl");
        let out_path = tmp.path().join("summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        // Three X tickets, each going Todo → In Progress → Code Review.
        // Hours-in-In Progress: 1, 2, 4 → p50=2, p75=4, p90=4.
        let base = now - Duration::days(5);
        for (i, dwell) in [1, 2, 4].iter().enumerate() {
            let id = format!("X-{}", i + 1);
            let t0 = (base + Duration::hours(i64::try_from(i).unwrap() * 10)).to_rfc3339();
            let t1 = (base + Duration::hours(i64::try_from(i).unwrap() * 10 + dwell)).to_rfc3339();
            append_to(&in_path, &ev(&id, Some("X"), Some("Todo"), "In Progress", &t0)).unwrap();
            append_to(&in_path, &ev(&id, Some("X"), Some("In Progress"), "Code Review", &t1)).unwrap();
        }
        let s = compute_stage_thresholds_at(&in_path, &out_path, now, 30).unwrap();
        let in_prog = s
            .thresholds
            .iter()
            .find(|t| t.from_stage == "In Progress" && t.team.as_deref() == Some("X"))
            .unwrap();
        assert_eq!(in_prog.sample_count, 3);
        // Sorted [1, 2, 4]: p50 index round(2*0.50)=1 → 2; p75 round(2*0.75)=2 → 4; p90 round(2*0.90)=2 → 4.
        assert!((in_prog.p50_hours - 2.0).abs() < f64::EPSILON);
        assert!((in_prog.p75_hours - 4.0).abs() < f64::EPSILON);
        assert!((in_prog.p90_hours - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_stage_thresholds_filters_out_of_window() {
        let tmp = tempfile::tempdir().unwrap();
        let in_path = tmp.path().join("cycle-time.jsonl");
        let out_path = tmp.path().join("summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        // Old transitions (45 days ago): excluded.
        let old0 = (now - Duration::days(45)).to_rfc3339();
        let old1 = (now - Duration::days(45) + Duration::hours(1)).to_rfc3339();
        append_to(&in_path, &ev("X-1", Some("X"), Some("Todo"), "In Progress", &old0)).unwrap();
        append_to(&in_path, &ev("X-1", Some("X"), Some("In Progress"), "Code Review", &old1)).unwrap();
        // Recent: included.
        let new0 = (now - Duration::days(3)).to_rfc3339();
        let new1 = (now - Duration::days(3) + Duration::hours(5)).to_rfc3339();
        append_to(&in_path, &ev("X-2", Some("X"), Some("Todo"), "In Progress", &new0)).unwrap();
        append_to(&in_path, &ev("X-2", Some("X"), Some("In Progress"), "Code Review", &new1)).unwrap();
        let s = compute_stage_thresholds_at(&in_path, &out_path, now, 30).unwrap();
        let in_prog = s
            .thresholds
            .iter()
            .find(|t| t.from_stage == "In Progress")
            .unwrap();
        assert_eq!(in_prog.sample_count, 1);
        assert!((in_prog.p50_hours - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_per_stage_breakdown_computes_mean_and_percentiles() {
        let tmp = tempfile::tempdir().unwrap();
        let in_path = tmp.path().join("cycle-time.jsonl");
        let out_path = tmp.path().join("summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        let base = now - Duration::days(5);
        // Dwells in "Code Review": 2, 4, 6, 8 hours.
        // Mean = 5; p50 = sorted[round(3*0.5)=2] = 6; p75 = sorted[round(3*0.75)=2] = 6.
        for (i, dwell) in [2, 4, 6, 8].iter().enumerate() {
            let id = format!("X-{}", i + 1);
            let t0 = (base + Duration::hours(i64::try_from(i).unwrap() * 20)).to_rfc3339();
            let t1 = (base + Duration::hours(i64::try_from(i).unwrap() * 20 + dwell)).to_rfc3339();
            append_to(&in_path, &ev(&id, Some("X"), Some("In Progress"), "Code Review", &t0)).unwrap();
            append_to(&in_path, &ev(&id, Some("X"), Some("Code Review"), "QA Testing", &t1)).unwrap();
        }
        let s = compute_per_stage_breakdown_at(&in_path, &out_path, now, 30).unwrap();
        let review = s
            .per_stage
            .iter()
            .find(|t| t.stage == "Code Review" && t.team.as_deref() == Some("X"))
            .unwrap();
        assert_eq!(review.sample_count, 4);
        assert!((review.mean_hours - 5.0).abs() < f64::EPSILON);
        assert!((review.p50_hours - 6.0).abs() < f64::EPSILON);
        assert!((review.p75_hours - 6.0).abs() < f64::EPSILON);
    }

    #[test]
    fn percentile_handles_single_element() {
        assert!((percentile(&[42.0], 50) - 42.0).abs() < f64::EPSILON);
        assert!((percentile(&[42.0], 90) - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn percentile_empty_is_zero() {
        assert!((percentile(&[], 50) - 0.0).abs() < f64::EPSILON);
    }
}
