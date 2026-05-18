//! Change Failure Rate + MTTR (SEN-11, DORA core metrics #3 + #4).
//!
//! Tracks production incidents and joins them to the deploy stream
//! (`deploys.jsonl`, SEN-9) to compute:
//!
//!   * **CFR** = `failed_deploys / deploys` over the 30d window. A deploy
//!     is "failed" when at least one incident has `created_at` within 24h
//!     after the deploy and matches the same `(repo, env)`.
//!   * **MTTR** = median of `(completed_at - created_at) / 1h` over
//!     incidents in the window with both timestamps present.
//!
//! On-disk:
//!   * Input: `~/.claude/sentinel/metrics/incidents.jsonl` (one
//!     [`IncidentRecord`] per line, append-only).
//!   * Input: `~/.claude/sentinel/metrics/deploys.jsonl` (SEN-9).
//!   * Output: `~/.claude/sentinel/metrics/change-failure-summary.json`.
//!
//! Producers (when wired): GitHub `check_run.completed`, Vercel/Railway
//! `deployment.error`, Linear webhook for issues tagged `incident:sevN`.
//! This module exposes the [`append_incident`] seam; the actual webhook
//! plumbing is a follow-up.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use crate::deploy_freq::{read_deploys, DoraTier};

/// Window after a deploy during which an incident is attributed to it.
const FAILURE_ATTRIBUTION_WINDOW: Duration = Duration::hours(24);

/// One incident appended to `incidents.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentRecord {
    /// When the incident was opened (RFC3339 UTC).
    pub created_at: String,
    /// Repository the incident affects.
    pub repo: String,
    /// Environment (`prod`, `staging`, `preview`).
    pub env: String,
    /// Severity (`sev1`..`sev4`).
    pub severity: String,
    /// Optional Linear ticket id (`FPCRM-329`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ticket_id: Option<String>,
    /// When the incident was resolved (RFC3339 UTC). `None` while still open.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub completed_at: Option<String>,
    /// Commit suspected to have caused the incident.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub root_cause_commit: Option<String>,
    /// Commit that restored prod (the recovery deploy).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub recovery_commit: Option<String>,
}

/// Per-(repo, env) CFR + MTTR aggregate over the 30-day window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeFailureAggregate {
    pub repo: String,
    pub env: String,
    pub deploys_count: u64,
    pub failed_deploys_count: u64,
    pub incidents_count: u64,
    pub recovered_count: u64,
    pub cfr: f64,
    pub mttr_hours: f64,
    pub cfr_tier: DoraTier,
    pub mttr_tier: DoraTier,
}

/// Summary file shape, written to `change-failure-summary.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeFailureSummary {
    pub generated_at: String,
    pub deploys_scanned: u64,
    pub incidents_scanned: u64,
    pub per_repo_env: Vec<ChangeFailureAggregate>,
}

/// Append an incident record. Creates parents if missing. Atomic per line.
///
/// # Errors
/// IO / serde failures.
pub fn append_incident(path: &Path, record: &IncidentRecord) -> Result<()> {
    ensure_parent(path)?;
    let line = serde_json::to_string(record).context("serialize IncidentRecord")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open append {}", path.display()))?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

/// Read every record from `incidents.jsonl`. Missing file → empty vec.
/// Malformed lines are logged and skipped.
///
/// # Errors
/// IO errors other than file-not-found.
pub fn read_incidents(path: &Path) -> Result<Vec<IncidentRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (n, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("read line {n} of {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<IncidentRecord>(&line) {
            Ok(r) => out.push(r),
            Err(e) => {
                tracing::warn!(
                    file = %path.display(),
                    line_no = n + 1,
                    error = %e,
                    "incidents.jsonl: skipping malformed line"
                );
            }
        }
    }
    Ok(out)
}

/// Aggregate CFR + MTTR per (repo, env) over the 30-day window ending at
/// `now`. Joins deploys.jsonl (SEN-9) to incidents.jsonl by (repo, env)
/// and a 24h attribution window.
///
/// # Errors
/// Returns the read or write error if either JSONL can't be loaded or the
/// summary file can't be persisted.
#[allow(clippy::cast_precision_loss)]
pub fn aggregate_at(
    deploys_path: &Path,
    incidents_path: &Path,
    summary_path: &Path,
    now: DateTime<Utc>,
) -> Result<ChangeFailureSummary> {
    let deploys = read_deploys(deploys_path)?;
    let incidents = read_incidents(incidents_path)?;
    let cutoff = now - Duration::days(30);

    // Group deploys by (repo, env) within window, retaining timestamps so we
    // can attribute incidents to them.
    type Key = (String, String);
    let mut deploys_by_key: BTreeMap<Key, Vec<DateTime<Utc>>> = BTreeMap::new();
    for d in &deploys {
        let ts = match DateTime::parse_from_rfc3339(&d.timestamp) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => continue,
        };
        if ts < cutoff {
            continue;
        }
        deploys_by_key
            .entry((d.repo.clone(), d.env.clone()))
            .or_default()
            .push(ts);
    }

    // Group incidents the same way, capturing both timestamps.
    let mut incidents_by_key: BTreeMap<Key, Vec<(DateTime<Utc>, Option<DateTime<Utc>>)>> =
        BTreeMap::new();
    for i in &incidents {
        let created = match DateTime::parse_from_rfc3339(&i.created_at) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => continue,
        };
        if created < cutoff {
            continue;
        }
        let completed = i
            .completed_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        incidents_by_key
            .entry((i.repo.clone(), i.env.clone()))
            .or_default()
            .push((created, completed));
    }

    // Union of keys — a key with deploys but no incidents still produces a
    // 0% CFR row; a key with incidents but no deploys reports CFR = 0 (no
    // denominator) but the incident count is preserved for surfacing.
    let mut all_keys: std::collections::BTreeSet<Key> = std::collections::BTreeSet::new();
    all_keys.extend(deploys_by_key.keys().cloned());
    all_keys.extend(incidents_by_key.keys().cloned());

    let mut per_repo_env = Vec::with_capacity(all_keys.len());
    for key in all_keys {
        let deploys_for_key = deploys_by_key.get(&key).cloned().unwrap_or_default();
        let incidents_for_key = incidents_by_key.get(&key).cloned().unwrap_or_default();

        // CFR: count deploys with at least one incident in the 24h after.
        let mut failed_deploys: u64 = 0;
        for deploy_ts in &deploys_for_key {
            let window_end = *deploy_ts + FAILURE_ATTRIBUTION_WINDOW;
            let attributed = incidents_for_key
                .iter()
                .any(|(created, _)| *created >= *deploy_ts && *created <= window_end);
            if attributed {
                failed_deploys += 1;
            }
        }
        let deploys_count = u64::try_from(deploys_for_key.len()).unwrap_or(u64::MAX);
        let cfr = if deploys_count == 0 {
            0.0
        } else {
            failed_deploys as f64 / deploys_count as f64
        };

        // MTTR: median over recovered incidents (both timestamps present).
        let mut recovery_hours: Vec<f64> = incidents_for_key
            .iter()
            .filter_map(|(created, completed)| {
                completed.map(|c| {
                    let secs = (c - *created).num_seconds();
                    f64::from(i32::try_from(secs.clamp(0, i64::from(i32::MAX))).unwrap_or(0))
                        / 3600.0
                })
            })
            .collect();
        recovery_hours.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mttr_hours = median(&recovery_hours);

        per_repo_env.push(ChangeFailureAggregate {
            repo: key.0.clone(),
            env: key.1.clone(),
            deploys_count,
            failed_deploys_count: failed_deploys,
            incidents_count: u64::try_from(incidents_for_key.len()).unwrap_or(u64::MAX),
            recovered_count: u64::try_from(recovery_hours.len()).unwrap_or(u64::MAX),
            cfr,
            mttr_hours,
            cfr_tier: DoraTier::from_change_failure_rate(cfr),
            mttr_tier: DoraTier::from_mttr_hours(mttr_hours),
        });
    }

    let summary = ChangeFailureSummary {
        generated_at: now.to_rfc3339(),
        deploys_scanned: u64::try_from(deploys.len()).unwrap_or(u64::MAX),
        incidents_scanned: u64::try_from(incidents.len()).unwrap_or(u64::MAX),
        per_repo_env,
    };

    if let Some(parent) = summary_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&summary).context("serialize ChangeFailureSummary")?;
    fs::write(summary_path, json)
        .with_context(|| format!("write {}", summary_path.display()))?;
    Ok(summary)
}

/// `Utc::now()` wrapper around [`aggregate_at`].
///
/// # Errors
/// Same as [`aggregate_at`].
pub fn aggregate(
    deploys_path: &Path,
    incidents_path: &Path,
    summary_path: &Path,
) -> Result<ChangeFailureSummary> {
    aggregate_at(deploys_path, incidents_path, summary_path, Utc::now())
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n.is_multiple_of(2) {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    }
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy_freq::{append_deploy, DeployRecord};
    use chrono::TimeZone;

    fn incident(
        created: &str,
        repo: &str,
        env: &str,
        completed: Option<&str>,
    ) -> IncidentRecord {
        IncidentRecord {
            created_at: created.to_string(),
            repo: repo.to_string(),
            env: env.to_string(),
            severity: "sev2".to_string(),
            ticket_id: None,
            completed_at: completed.map(str::to_string),
            root_cause_commit: None,
            recovery_commit: None,
        }
    }

    fn deploy(ts: &str, repo: &str, env: &str) -> DeployRecord {
        DeployRecord {
            timestamp: ts.to_string(),
            repo: repo.to_string(),
            env: env.to_string(),
            commit: "abc1234".to_string(),
            duration_s: Some(30),
        }
    }

    #[test]
    fn append_then_read_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("incidents.jsonl");
        let r = incident("2026-05-10T10:00:00Z", "sentinel", "prod", Some("2026-05-10T12:00:00Z"));
        append_incident(&path, &r).unwrap();
        let got = read_incidents(&path).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], r);
    }

    #[test]
    fn read_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing.jsonl");
        assert!(read_incidents(&path).unwrap().is_empty());
    }

    #[test]
    fn cfr_with_zero_deploys_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let incidents = tmp.path().join("incidents.jsonl");
        let summary = tmp.path().join("change-failure-summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        // Incident only — no deploys.
        let ts = (now - Duration::days(5)).to_rfc3339();
        append_incident(&incidents, &incident(&ts, "r", "prod", None)).unwrap();
        let s = aggregate_at(&deploys, &incidents, &summary, now).unwrap();
        assert_eq!(s.per_repo_env.len(), 1);
        let a = &s.per_repo_env[0];
        assert_eq!(a.deploys_count, 0);
        assert_eq!(a.failed_deploys_count, 0);
        assert!((a.cfr - 0.0).abs() < f64::EPSILON);
        assert_eq!(a.cfr_tier, DoraTier::Elite);
    }

    #[test]
    fn cfr_links_incident_to_recent_deploy() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let incidents = tmp.path().join("incidents.jsonl");
        let summary = tmp.path().join("change-failure-summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        let deploy_ts = now - Duration::days(2);
        let incident_ts = deploy_ts + Duration::hours(1);
        append_deploy(&deploys, &deploy(&deploy_ts.to_rfc3339(), "sentinel", "prod")).unwrap();
        append_incident(
            &incidents,
            &incident(&incident_ts.to_rfc3339(), "sentinel", "prod", None),
        )
        .unwrap();
        let s = aggregate_at(&deploys, &incidents, &summary, now).unwrap();
        let a = &s.per_repo_env[0];
        assert_eq!(a.deploys_count, 1);
        assert_eq!(a.failed_deploys_count, 1);
        assert!((a.cfr - 1.0).abs() < f64::EPSILON);
        assert_eq!(a.cfr_tier, DoraTier::Low);
    }

    #[test]
    fn cfr_does_not_link_incident_outside_24h_window() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let incidents = tmp.path().join("incidents.jsonl");
        let summary = tmp.path().join("change-failure-summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        let deploy_ts = now - Duration::days(2);
        // Incident 25h after the deploy → outside window.
        let incident_ts = deploy_ts + Duration::hours(25);
        append_deploy(&deploys, &deploy(&deploy_ts.to_rfc3339(), "r", "prod")).unwrap();
        append_incident(
            &incidents,
            &incident(&incident_ts.to_rfc3339(), "r", "prod", None),
        )
        .unwrap();
        let s = aggregate_at(&deploys, &incidents, &summary, now).unwrap();
        let a = &s.per_repo_env[0];
        assert_eq!(a.deploys_count, 1);
        assert_eq!(a.failed_deploys_count, 0);
        assert!((a.cfr - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cfr_ignores_repo_env_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let incidents = tmp.path().join("incidents.jsonl");
        let summary = tmp.path().join("change-failure-summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        let deploy_ts = now - Duration::days(1);
        let incident_ts = deploy_ts + Duration::hours(1);
        // Deploy to "a/prod", incident on "b/prod" — different repo, not linked.
        append_deploy(&deploys, &deploy(&deploy_ts.to_rfc3339(), "a", "prod")).unwrap();
        append_incident(
            &incidents,
            &incident(&incident_ts.to_rfc3339(), "b", "prod", None),
        )
        .unwrap();
        let s = aggregate_at(&deploys, &incidents, &summary, now).unwrap();
        // Two rows: one for "a/prod" with 0 incidents, one for "b/prod"
        // with 0 deploys.
        let a = s.per_repo_env.iter().find(|x| x.repo == "a").unwrap();
        assert_eq!(a.deploys_count, 1);
        assert_eq!(a.failed_deploys_count, 0);
    }

    #[test]
    fn mttr_excludes_unrecovered_incidents() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let incidents = tmp.path().join("incidents.jsonl");
        let summary = tmp.path().join("change-failure-summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        let base = now - Duration::days(3);
        // Recovered in 2h.
        append_incident(
            &incidents,
            &incident(
                &base.to_rfc3339(),
                "sentinel",
                "prod",
                Some(&(base + Duration::hours(2)).to_rfc3339()),
            ),
        )
        .unwrap();
        // Recovered in 4h.
        let base2 = now - Duration::days(2);
        append_incident(
            &incidents,
            &incident(
                &base2.to_rfc3339(),
                "sentinel",
                "prod",
                Some(&(base2 + Duration::hours(4)).to_rfc3339()),
            ),
        )
        .unwrap();
        // Unrecovered — excluded from median.
        let base3 = now - Duration::days(1);
        append_incident(&incidents, &incident(&base3.to_rfc3339(), "sentinel", "prod", None))
            .unwrap();
        let s = aggregate_at(&deploys, &incidents, &summary, now).unwrap();
        let a = &s.per_repo_env[0];
        assert_eq!(a.incidents_count, 3);
        assert_eq!(a.recovered_count, 2);
        // Median of [2.0, 4.0] = 3.0.
        assert!((a.mttr_hours - 3.0).abs() < f64::EPSILON);
        assert_eq!(a.mttr_tier, DoraTier::High);
    }

    #[test]
    fn mttr_zero_with_no_recovered_incidents() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let incidents = tmp.path().join("incidents.jsonl");
        let summary = tmp.path().join("change-failure-summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        append_incident(
            &incidents,
            &incident(&(now - Duration::days(1)).to_rfc3339(), "r", "prod", None),
        )
        .unwrap();
        let s = aggregate_at(&deploys, &incidents, &summary, now).unwrap();
        let a = &s.per_repo_env[0];
        assert!((a.mttr_hours - 0.0).abs() < f64::EPSILON);
        // 0 hours classifies as Elite.
        assert_eq!(a.mttr_tier, DoraTier::Elite);
    }

    #[test]
    fn read_skips_malformed_incident_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("incidents.jsonl");
        append_incident(&path, &incident("2026-05-10T10:00:00Z", "r", "prod", None)).unwrap();
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"{not-json\n").unwrap();
        }
        append_incident(&path, &incident("2026-05-10T11:00:00Z", "r", "prod", None)).unwrap();
        let got = read_incidents(&path).unwrap();
        assert_eq!(got.len(), 2);
    }
}
