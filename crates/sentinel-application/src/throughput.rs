//! Throughput Health + First-Time-Pass — SEN-5 + SEN-16.
//!
//! Two metrics over the cycle-time event stream (SEN-1, `cycle-time.jsonl`):
//!
//! * **SEN-5 — Throughput.** Tickets-per-week, tickets-per-month, and a
//!   60-day daily sparkline of "completions per day". Feeds the dashboard's
//!   `ThroughputPanel` (an SEN-19-side organism).
//!
//! * **SEN-16 — First-Time-Pass rate + rework cost.** A ticket is a
//!   "first-time pass" when its journey goes
//!   `... → Code Review → QA Testing → Completed` without ever bouncing
//!   back to `In Progress`. The rework loop is the (`QA Failed` →
//!   `In Progress`) bounce. FTP rate = `first_pass / completed`. Rework
//!   cost (tokens / dollars) is computed by joining each rework-loop
//!   issue against `tokens-per-ticket.jsonl` (SEN-7) when available.
//!
//! Both functions are pure and operate on already-read event slices, so
//! callers can pipe in test fixtures without touching disk.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use crate::cycle_time::CycleTimeEvent;
use crate::cycle_time_analytics::read_events;

/// Default analysis window for both metrics: 30 days.
pub const DEFAULT_WINDOW_DAYS: i64 = 30;

// ===================================================================
// SEN-5 — Throughput
// ===================================================================

/// One day in the throughput sparkline (60-day series).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThroughputPoint {
    pub date: String,
    pub completions: u64,
}

/// Per-team throughput aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamThroughput {
    pub team: Option<String>,
    pub completed_30d: u64,
    pub completed_7d: u64,
    pub completed_per_week: f64,
    pub completed_per_month: f64,
}

/// Summary file shape, written to `throughput-summary.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThroughputSummary {
    pub generated_at: String,
    pub window_days: i64,
    pub events_scanned: u64,
    pub completions_scanned: u64,
    pub per_team: Vec<TeamThroughput>,
    /// 60-day daily completion counts across all teams. Used for the
    /// sparkline on the dashboard.
    pub daily_points: Vec<ThroughputPoint>,
}

/// Compute throughput stats from a cycle-time event slice. `now` is
/// injected for test determinism.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn compute_throughput(
    events: &[CycleTimeEvent],
    now: DateTime<Utc>,
    window_days: i64,
) -> ThroughputSummary {
    let cutoff = now - Duration::days(window_days);
    let cutoff_7d = now - Duration::days(7);
    let cutoff_60d = now - Duration::days(60);

    let mut per_team_30d: BTreeMap<Option<String>, u64> = BTreeMap::new();
    let mut per_team_7d: BTreeMap<Option<String>, u64> = BTreeMap::new();
    let mut all_teams: BTreeSet<Option<String>> = BTreeSet::new();
    let mut daily: BTreeMap<NaiveDate, u64> = BTreeMap::new();
    let mut completions_total: u64 = 0;

    for e in events {
        if e.to_state != "Completed" {
            continue;
        }
        let ts = match DateTime::parse_from_rfc3339(&e.timestamp) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => continue,
        };
        all_teams.insert(e.team.clone());
        if ts >= cutoff {
            completions_total += 1;
            *per_team_30d.entry(e.team.clone()).or_insert(0) += 1;
            if ts >= cutoff_7d {
                *per_team_7d.entry(e.team.clone()).or_insert(0) += 1;
            }
        }
        if ts >= cutoff_60d {
            *daily.entry(ts.date_naive()).or_insert(0) += 1;
        }
    }

    let mut per_team = Vec::with_capacity(all_teams.len());
    for team in &all_teams {
        let d30 = per_team_30d.get(team).copied().unwrap_or(0);
        let d7 = per_team_7d.get(team).copied().unwrap_or(0);
        per_team.push(TeamThroughput {
            team: team.clone(),
            completed_30d: d30,
            completed_7d: d7,
            completed_per_week: d30 as f64 / (window_days as f64 / 7.0),
            completed_per_month: d30 as f64 * (30.0 / window_days as f64),
        });
    }

    let daily_points: Vec<ThroughputPoint> = daily
        .into_iter()
        .map(|(d, n)| ThroughputPoint {
            date: d.to_string(),
            completions: n,
        })
        .collect();

    ThroughputSummary {
        generated_at: now.to_rfc3339(),
        window_days,
        events_scanned: u64::try_from(events.len()).unwrap_or(u64::MAX),
        completions_scanned: completions_total,
        per_team,
        daily_points,
    }
}

/// Convenience: read events from `cycle_time_path`, compute throughput,
/// write the summary file.
///
/// # Errors
/// Read or write IO error.
pub fn compute_throughput_at(
    cycle_time_path: &Path,
    summary_path: &Path,
    now: DateTime<Utc>,
    window_days: i64,
) -> Result<ThroughputSummary> {
    let events = read_events(cycle_time_path)?;
    let summary = compute_throughput(&events, now, window_days);
    write_summary(summary_path, &summary).context("write ThroughputSummary")?;
    Ok(summary)
}

// ===================================================================
// SEN-16 — First-Time-Pass + rework cost
// ===================================================================

/// Per-ticket FTP outcome. `had_rework_loop` is true when the issue
/// bounced back to `In Progress` after entering `Code Review` or later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketOutcome {
    pub issue_id: String,
    pub team: Option<String>,
    pub completed_at: Option<String>,
    pub had_rework_loop: bool,
}

/// Per-team FTP aggregate. `rework_cost_usd` is summed across the
/// tickets with rework loops when token data is supplied; `None` when
/// no `tokens-per-ticket.jsonl` was provided.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamFtp {
    pub team: Option<String>,
    pub completed: u64,
    pub first_time_pass: u64,
    pub rework: u64,
    pub ftp_rate: f64,
    pub rework_cost_usd: Option<f64>,
}

/// Summary file shape, written to `first-time-pass-summary.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtpSummary {
    pub generated_at: String,
    pub window_days: i64,
    pub events_scanned: u64,
    pub tickets_completed: u64,
    pub per_team: Vec<TeamFtp>,
    pub tickets: Vec<TicketOutcome>,
}

/// One row from `tokens-per-ticket.jsonl` (SEN-7). Re-declared as a
/// minimal local type to avoid coupling SEN-16 to the full SEN-7 module.
#[derive(Debug, Clone, Deserialize)]
struct TokensPerTicketRow {
    ticket: String,
    #[serde(default)]
    cost_usd: f64,
}

/// Read tokens-per-ticket rows. Missing file → empty map.
///
/// # Errors
/// IO errors other than file-not-found.
pub fn read_token_costs(path: &Path) -> Result<HashMap<String, f64>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut out = HashMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<TokensPerTicketRow>(line) {
            Ok(r) => {
                out.insert(r.ticket, r.cost_usd);
            }
            Err(e) => {
                tracing::warn!(error = %e, "tokens-per-ticket.jsonl: skipping malformed line")
            }
        }
    }
    Ok(out)
}

/// Pure FTP computation over an event slice. `token_costs` is optional;
/// when `None`, `rework_cost_usd` will be `None` on every team row.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn compute_first_time_pass(
    events: &[CycleTimeEvent],
    now: DateTime<Utc>,
    window_days: i64,
    token_costs: Option<&HashMap<String, f64>>,
) -> FtpSummary {
    let cutoff = now - Duration::days(window_days);

    // Group events by issue_id, sort by timestamp ascending.
    let mut by_issue: BTreeMap<String, Vec<&CycleTimeEvent>> = BTreeMap::new();
    for e in events {
        by_issue.entry(e.issue_id.clone()).or_default().push(e);
    }

    let mut tickets: Vec<TicketOutcome> = Vec::new();
    for (issue, mut history) in by_issue {
        history.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

        let completion = history.iter().find(|e| e.to_state == "Completed");
        let Some(completion) = completion else {
            continue;
        };
        let completed_ts = match DateTime::parse_from_rfc3339(&completion.timestamp) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => continue,
        };
        if completed_ts < cutoff {
            continue;
        }

        // Rework loop = any transition INTO "In Progress" that came
        // AFTER the issue had previously entered "Code Review" or later.
        let mut entered_review_or_later = false;
        let mut had_rework = false;
        for ev in &history {
            match ev.to_state.as_str() {
                "Code Review" | "QA Testing" | "QA Failed" => entered_review_or_later = true,
                "In Progress" if entered_review_or_later => {
                    had_rework = true;
                    break;
                }
                _ => {}
            }
        }

        tickets.push(TicketOutcome {
            issue_id: issue,
            team: completion.team.clone(),
            completed_at: Some(completion.timestamp.clone()),
            had_rework_loop: had_rework,
        });
    }

    // Aggregate by team.
    let mut buckets: BTreeMap<Option<String>, (u64, u64, f64)> = BTreeMap::new();
    for t in &tickets {
        let entry = buckets.entry(t.team.clone()).or_insert((0, 0, 0.0));
        entry.0 += 1;
        if t.had_rework_loop {
            entry.1 += 1;
            if let Some(costs) = token_costs {
                if let Some(c) = costs.get(&t.issue_id) {
                    entry.2 += *c;
                }
            }
        }
    }

    let mut per_team = Vec::with_capacity(buckets.len());
    for (team, (completed, rework, cost)) in buckets {
        let first_pass = completed - rework;
        let ftp_rate = if completed == 0 {
            0.0
        } else {
            first_pass as f64 / completed as f64
        };
        per_team.push(TeamFtp {
            team,
            completed,
            first_time_pass: first_pass,
            rework,
            ftp_rate,
            rework_cost_usd: token_costs.map(|_| cost),
        });
    }

    let completed_total = u64::try_from(tickets.len()).unwrap_or(u64::MAX);
    FtpSummary {
        generated_at: now.to_rfc3339(),
        window_days,
        events_scanned: u64::try_from(events.len()).unwrap_or(u64::MAX),
        tickets_completed: completed_total,
        per_team,
        tickets,
    }
}

/// Convenience: read cycle-time + token-cost files, compute FTP, write
/// the summary file.
///
/// # Errors
/// Read or write IO errors.
pub fn compute_first_time_pass_at(
    cycle_time_path: &Path,
    tokens_path: Option<&Path>,
    summary_path: &Path,
    now: DateTime<Utc>,
    window_days: i64,
) -> Result<FtpSummary> {
    let events = read_events(cycle_time_path)?;
    let token_costs = match tokens_path {
        Some(p) => Some(read_token_costs(p)?),
        None => None,
    };
    let summary = compute_first_time_pass(&events, now, window_days, token_costs.as_ref());
    write_summary(summary_path, &summary).context("write FtpSummary")?;
    Ok(summary)
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
    use chrono::TimeZone;

    fn ev(issue: &str, team: Option<&str>, to: &str, ts: &str) -> CycleTimeEvent {
        CycleTimeEvent {
            issue_id: issue.to_string(),
            team: team.map(str::to_string),
            from_state: None,
            to_state: to.to_string(),
            timestamp: ts.to_string(),
            estimate: None,
            priority: None,
            labels: vec![],
        }
    }

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap()
    }

    // --- Throughput ----------------------------------------------------

    #[test]
    fn throughput_empty_input() {
        let s = compute_throughput(&[], fixed_now(), 30);
        assert_eq!(s.completions_scanned, 0);
        assert!(s.per_team.is_empty());
        assert!(s.daily_points.is_empty());
    }

    #[test]
    fn throughput_counts_only_completion_transitions() {
        let now = fixed_now();
        let ts_in = (now - Duration::days(3)).to_rfc3339();
        let events = vec![
            ev("X-1", Some("X"), "In Progress", &ts_in),
            ev("X-1", Some("X"), "Completed", &ts_in),
            ev("X-2", Some("X"), "Code Review", &ts_in), // not completed → excluded
        ];
        let s = compute_throughput(&events, now, 30);
        assert_eq!(s.completions_scanned, 1);
        assert_eq!(s.per_team[0].completed_30d, 1);
    }

    #[test]
    fn throughput_buckets_by_team() {
        let now = fixed_now();
        let ts = (now - Duration::days(2)).to_rfc3339();
        let events = vec![
            ev("X-1", Some("X"), "Completed", &ts),
            ev("X-2", Some("X"), "Completed", &ts),
            ev("Y-1", Some("Y"), "Completed", &ts),
        ];
        let s = compute_throughput(&events, now, 30);
        let x = s
            .per_team
            .iter()
            .find(|t| t.team.as_deref() == Some("X"))
            .unwrap();
        let y = s
            .per_team
            .iter()
            .find(|t| t.team.as_deref() == Some("Y"))
            .unwrap();
        assert_eq!(x.completed_30d, 2);
        assert_eq!(y.completed_30d, 1);
    }

    #[test]
    fn throughput_filters_out_of_window() {
        let now = fixed_now();
        let old = (now - Duration::days(45)).to_rfc3339();
        let new = (now - Duration::days(2)).to_rfc3339();
        let events = vec![
            ev("X-1", Some("X"), "Completed", &old),
            ev("X-2", Some("X"), "Completed", &new),
        ];
        let s = compute_throughput(&events, now, 30);
        assert_eq!(s.completions_scanned, 1);
    }

    #[test]
    fn throughput_daily_sparkline_covers_60d_only() {
        let now = fixed_now();
        let in60 = (now - Duration::days(30)).to_rfc3339();
        let out60 = (now - Duration::days(70)).to_rfc3339();
        let events = vec![
            ev("X-1", Some("X"), "Completed", &in60),
            ev("X-2", Some("X"), "Completed", &out60),
        ];
        let s = compute_throughput(&events, now, 30);
        assert_eq!(s.daily_points.len(), 1);
    }

    // --- First-Time-Pass -----------------------------------------------

    #[test]
    fn ftp_empty_input() {
        let s = compute_first_time_pass(&[], fixed_now(), 30, None);
        assert_eq!(s.tickets_completed, 0);
        assert!(s.tickets.is_empty());
        assert!(s.per_team.is_empty());
    }

    #[test]
    fn ftp_clean_path_is_first_time_pass() {
        let now = fixed_now();
        let base = now - Duration::days(3);
        let events = vec![
            ev("X-1", Some("X"), "In Progress", &base.to_rfc3339()),
            ev(
                "X-1",
                Some("X"),
                "Code Review",
                &(base + Duration::hours(1)).to_rfc3339(),
            ),
            ev(
                "X-1",
                Some("X"),
                "QA Testing",
                &(base + Duration::hours(2)).to_rfc3339(),
            ),
            ev(
                "X-1",
                Some("X"),
                "Completed",
                &(base + Duration::hours(3)).to_rfc3339(),
            ),
        ];
        let s = compute_first_time_pass(&events, now, 30, None);
        assert_eq!(s.tickets_completed, 1);
        assert!(!s.tickets[0].had_rework_loop);
        assert_eq!(s.per_team[0].first_time_pass, 1);
        assert_eq!(s.per_team[0].rework, 0);
        assert!((s.per_team[0].ftp_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn ftp_bounce_back_to_in_progress_counts_as_rework() {
        let now = fixed_now();
        let base = now - Duration::days(3);
        // Review-fail loop: In Progress → Code Review → In Progress → Code Review → Completed.
        let events = vec![
            ev("X-1", Some("X"), "In Progress", &base.to_rfc3339()),
            ev(
                "X-1",
                Some("X"),
                "Code Review",
                &(base + Duration::hours(1)).to_rfc3339(),
            ),
            ev(
                "X-1",
                Some("X"),
                "In Progress",
                &(base + Duration::hours(2)).to_rfc3339(),
            ),
            ev(
                "X-1",
                Some("X"),
                "Code Review",
                &(base + Duration::hours(3)).to_rfc3339(),
            ),
            ev(
                "X-1",
                Some("X"),
                "Completed",
                &(base + Duration::hours(4)).to_rfc3339(),
            ),
        ];
        let s = compute_first_time_pass(&events, now, 30, None);
        assert!(s.tickets[0].had_rework_loop);
        assert_eq!(s.per_team[0].rework, 1);
        assert!((s.per_team[0].ftp_rate - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn ftp_qa_failed_then_in_progress_counts_as_rework() {
        let now = fixed_now();
        let base = now - Duration::days(3);
        let events = vec![
            ev("X-1", Some("X"), "In Progress", &base.to_rfc3339()),
            ev(
                "X-1",
                Some("X"),
                "Code Review",
                &(base + Duration::hours(1)).to_rfc3339(),
            ),
            ev(
                "X-1",
                Some("X"),
                "QA Testing",
                &(base + Duration::hours(2)).to_rfc3339(),
            ),
            ev(
                "X-1",
                Some("X"),
                "QA Failed",
                &(base + Duration::hours(3)).to_rfc3339(),
            ),
            ev(
                "X-1",
                Some("X"),
                "In Progress",
                &(base + Duration::hours(4)).to_rfc3339(),
            ),
            ev(
                "X-1",
                Some("X"),
                "QA Testing",
                &(base + Duration::hours(5)).to_rfc3339(),
            ),
            ev(
                "X-1",
                Some("X"),
                "Completed",
                &(base + Duration::hours(6)).to_rfc3339(),
            ),
        ];
        let s = compute_first_time_pass(&events, now, 30, None);
        assert!(s.tickets[0].had_rework_loop);
    }

    #[test]
    fn ftp_initial_in_progress_does_not_count_as_rework() {
        let now = fixed_now();
        let base = now - Duration::days(3);
        // Backlog → In Progress → Completed is NOT a rework loop. The
        // first In Progress is the natural starting point.
        let events = vec![
            ev("X-1", Some("X"), "In Progress", &base.to_rfc3339()),
            ev(
                "X-1",
                Some("X"),
                "Completed",
                &(base + Duration::hours(1)).to_rfc3339(),
            ),
        ];
        let s = compute_first_time_pass(&events, now, 30, None);
        assert!(!s.tickets[0].had_rework_loop);
    }

    #[test]
    fn ftp_skips_uncompleted_tickets() {
        let now = fixed_now();
        let base = now - Duration::days(3);
        let events = vec![
            ev("X-1", Some("X"), "In Progress", &base.to_rfc3339()),
            ev(
                "X-1",
                Some("X"),
                "Code Review",
                &(base + Duration::hours(1)).to_rfc3339(),
            ),
            // never completed → skipped
        ];
        let s = compute_first_time_pass(&events, now, 30, None);
        assert_eq!(s.tickets_completed, 0);
    }

    #[test]
    fn ftp_rework_cost_sums_token_costs() {
        let now = fixed_now();
        let base = now - Duration::days(3);
        let events = vec![
            // X-1: rework loop, $30 in tokens.
            ev("X-1", Some("X"), "In Progress", &base.to_rfc3339()),
            ev(
                "X-1",
                Some("X"),
                "Code Review",
                &(base + Duration::hours(1)).to_rfc3339(),
            ),
            ev(
                "X-1",
                Some("X"),
                "In Progress",
                &(base + Duration::hours(2)).to_rfc3339(),
            ),
            ev(
                "X-1",
                Some("X"),
                "Completed",
                &(base + Duration::hours(3)).to_rfc3339(),
            ),
            // X-2: clean pass, $10 in tokens (not counted in rework_cost).
            ev("X-2", Some("X"), "In Progress", &base.to_rfc3339()),
            ev(
                "X-2",
                Some("X"),
                "Completed",
                &(base + Duration::hours(1)).to_rfc3339(),
            ),
        ];
        let mut costs = HashMap::new();
        costs.insert("X-1".to_string(), 30.0);
        costs.insert("X-2".to_string(), 10.0);
        let s = compute_first_time_pass(&events, now, 30, Some(&costs));
        let x = &s.per_team[0];
        assert_eq!(x.rework, 1);
        assert_eq!(x.rework_cost_usd, Some(30.0));
    }

    #[test]
    fn ftp_rework_cost_is_none_when_no_token_data() {
        let now = fixed_now();
        let base = now - Duration::days(3);
        let events = vec![
            ev("X-1", Some("X"), "In Progress", &base.to_rfc3339()),
            ev(
                "X-1",
                Some("X"),
                "Completed",
                &(base + Duration::hours(1)).to_rfc3339(),
            ),
        ];
        let s = compute_first_time_pass(&events, now, 30, None);
        assert_eq!(s.per_team[0].rework_cost_usd, None);
    }

    #[test]
    fn ftp_filters_out_of_window() {
        let now = fixed_now();
        let old = (now - Duration::days(45)).to_rfc3339();
        let events = vec![
            ev("X-1", Some("X"), "In Progress", &old),
            ev("X-1", Some("X"), "Completed", &old),
        ];
        let s = compute_first_time_pass(&events, now, 30, None);
        assert_eq!(s.tickets_completed, 0);
    }
}
