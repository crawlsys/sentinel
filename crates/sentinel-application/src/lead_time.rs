//! Lead Time For Changes (SEN-10, DORA core metric #1).
//!
//! Tracks the elapsed time from first-commit on a change → prod deploy.
//! On-disk store is `~/.claude/sentinel/metrics/lead_times.jsonl`, one
//! [`LeadTimeRecord`] per line, append-only. Producers are:
//!
//!   * GitHub `pull_request` webhook handler that joins the PR's merge
//!     commit to the deploy stream (SEN-9). Wiring is a follow-up; this
//!     module exposes the [`append_lead_time`] seam.
//!   * Manual CLI entry via `sentinel lead-time record` for smoke testing
//!     and historical backfill.
//!
//! Consumers:
//!
//!   * `sentinel lead-time aggregate` — rolling 30-day p50/p75/p95 per
//!     (repo, env) plus DORA tier classification via
//!     [`deploy_freq::DoraTier::from_lead_time_hours`].
//!   * SENTINEL-19's operational summary.
//!   * Local DORA clients can read the underlying JSONL once lines
//!     start appearing.
//!
//! ## Schema
//!
//! ```json
//! {
//!   "timestamp": "2026-05-15T19:30:00Z",
//!   "repo": "firefly-pro-crm",
//!   "env": "prod",
//!   "commit": "abc1234",
//!   "first_commit_at": "2026-05-12T08:15:00Z",
//!   "deploy_at":       "2026-05-15T19:30:00Z",
//!   "lead_time_hours": 83.25
//! }
//! ```
//!
//! `timestamp` is the event-recording time (typically equal to `deploy_at`).
//! `first_commit_at` and `deploy_at` are RFC3339 UTC; `lead_time_hours` is
//! their difference, computed by the producer.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use crate::deploy_freq::DoraTier;

/// One lead-time event appended to `lead_times.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeadTimeRecord {
    /// Event-recording time (RFC3339 UTC). Typically equal to `deploy_at`.
    pub timestamp: String,
    /// Repository identifier (e.g. `firefly-pro-crm`).
    pub repo: String,
    /// Environment the change shipped to. Aggregation filters to `prod`
    /// for the canonical DORA metric; other envs are recorded too.
    pub env: String,
    /// Deploy commit SHA.
    pub commit: String,
    /// When the first commit on the change was authored (RFC3339 UTC).
    pub first_commit_at: String,
    /// When the change reached the target env (RFC3339 UTC).
    pub deploy_at: String,
    /// Pre-computed lead time in hours. Producers must compute this; the
    /// aggregator trusts it (re-deriving from `first_commit_at` /
    /// `deploy_at` would couple aggregation to RFC3339 parsing for every
    /// line at read time).
    pub lead_time_hours: f64,
}

/// Per-(repo, env) lead-time aggregate over the 30-day window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeadTimeAggregate {
    pub repo: String,
    pub env: String,
    pub sample_count: usize,
    pub p50_hours: f64,
    pub p75_hours: f64,
    pub p95_hours: f64,
    /// Tier derived from p50 (the standard DORA reporting convention).
    pub tier: DoraTier,
}

/// Summary file shape, written to `lead-times-summary.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeadTimeSummary {
    pub generated_at: String,
    pub records_scanned: u64,
    pub per_repo_env: Vec<LeadTimeAggregate>,
}

/// Append a lead-time record. Creates parents if missing. Atomic per line.
///
/// # Errors
/// Returns the underlying IO / serde error when the file cannot be opened,
/// the record can't be serialized, or the write fails.
pub fn append_lead_time(path: &Path, record: &LeadTimeRecord) -> Result<()> {
    ensure_parent(path)?;
    let line = serde_json::to_string(record).context("serialize LeadTimeRecord")?;
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

/// Read every record from `lead_times.jsonl`. Returns `Ok(vec![])` for
/// missing files; logs and skips individual malformed lines so a single
/// corrupt entry doesn't poison aggregation.
///
/// # Errors
/// Returns IO errors other than file-not-found.
pub fn read_lead_times(path: &Path) -> Result<Vec<LeadTimeRecord>> {
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
        match serde_json::from_str::<LeadTimeRecord>(&line) {
            Ok(r) => out.push(r),
            Err(e) => {
                tracing::warn!(
                    file = %path.display(),
                    line_no = n + 1,
                    error = %e,
                    "lead_times.jsonl: skipping malformed line"
                );
            }
        }
    }
    Ok(out)
}

/// Aggregate `lead_times.jsonl` into rolling 30-day per-(repo, env)
/// percentiles + DORA tier. `now` is injected for test determinism;
/// production callers pass `Utc::now()`.
///
/// # Errors
/// Returns the read or write error if the JSONL can't be loaded or the
/// summary file can't be persisted.
pub fn aggregate_at(
    lead_times_path: &Path,
    summary_path: &Path,
    now: DateTime<Utc>,
) -> Result<LeadTimeSummary> {
    let records = read_lead_times(lead_times_path)?;
    let cutoff = now - Duration::days(30);

    let mut buckets: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    for r in &records {
        let ts = match DateTime::parse_from_rfc3339(&r.timestamp) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => continue,
        };
        if ts < cutoff {
            continue;
        }
        if !r.lead_time_hours.is_finite() || r.lead_time_hours < 0.0 {
            continue;
        }
        buckets
            .entry((r.repo.clone(), r.env.clone()))
            .or_default()
            .push(r.lead_time_hours);
    }

    let mut per_repo_env = Vec::with_capacity(buckets.len());
    for ((repo, env), mut samples) in buckets {
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p50 = percentile(&samples, 50);
        let p75 = percentile(&samples, 75);
        let p95 = percentile(&samples, 95);
        per_repo_env.push(LeadTimeAggregate {
            repo,
            env,
            sample_count: samples.len(),
            p50_hours: p50,
            p75_hours: p75,
            p95_hours: p95,
            tier: DoraTier::from_lead_time_hours(p50),
        });
    }

    let summary = LeadTimeSummary {
        generated_at: now.to_rfc3339(),
        records_scanned: u64::try_from(records.len()).unwrap_or(u64::MAX),
        per_repo_env,
    };

    if let Some(parent) = summary_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&summary).context("serialize LeadTimeSummary")?;
    fs::write(summary_path, json).with_context(|| format!("write {}", summary_path.display()))?;
    Ok(summary)
}

/// Thin `Utc::now()` wrapper around [`aggregate_at`].
///
/// # Errors
/// Same as [`aggregate_at`].
pub fn aggregate(lead_times_path: &Path, summary_path: &Path) -> Result<LeadTimeSummary> {
    aggregate_at(lead_times_path, summary_path, Utc::now())
}

/// Inclusive-rank percentile over a pre-sorted slice. Returns 0.0 for an
/// empty slice. `p` is in `[0, 100]`.
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
    use chrono::TimeZone;

    fn rec(ts: &str, repo: &str, env: &str, hours: f64) -> LeadTimeRecord {
        LeadTimeRecord {
            timestamp: ts.to_string(),
            repo: repo.to_string(),
            env: env.to_string(),
            commit: "abc1234".to_string(),
            first_commit_at: ts.to_string(),
            deploy_at: ts.to_string(),
            lead_time_hours: hours,
        }
    }

    #[test]
    fn append_then_read_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lead_times.jsonl");
        let r = rec("2026-05-10T12:00:00Z", "sentinel", "prod", 4.5);
        append_lead_time(&path, &r).unwrap();
        let got = read_lead_times(&path).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], r);
    }

    #[test]
    fn read_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing.jsonl");
        assert!(read_lead_times(&path).unwrap().is_empty());
    }

    #[test]
    fn read_skips_malformed_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lead_times.jsonl");
        append_lead_time(&path, &rec("2026-05-10T12:00:00Z", "r", "prod", 1.0)).unwrap();
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"{not-json\n").unwrap();
        }
        append_lead_time(&path, &rec("2026-05-10T13:00:00Z", "r", "prod", 2.0)).unwrap();
        let got = read_lead_times(&path).unwrap();
        assert_eq!(got.len(), 2);
        assert!((got[0].lead_time_hours - 1.0).abs() < f64::EPSILON);
        assert!((got[1].lead_time_hours - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn aggregate_empty_input() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = tmp.path().join("lead_times.jsonl");
        let summary = tmp.path().join("summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        let s = aggregate_at(&lead, &summary, now).unwrap();
        assert_eq!(s.records_scanned, 0);
        assert!(s.per_repo_env.is_empty());
    }

    #[test]
    fn aggregate_computes_percentiles_and_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = tmp.path().join("lead_times.jsonl");
        let summary = tmp.path().join("summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        // Five samples within the 30d window: 4, 8, 12, 18, 96 hours.
        // p50 = sample at index round(4 * 0.50) = 2 → 12.
        // p75 = sample at index round(4 * 0.75) = 3 → 18.
        // p95 = sample at index round(4 * 0.95) = 4 → 96.
        for (i, h) in [4.0, 8.0, 12.0, 18.0, 96.0].iter().enumerate() {
            let ts = (now - Duration::days(i64::try_from(i).unwrap() + 1)).to_rfc3339();
            append_lead_time(&lead, &rec(&ts, "sentinel", "prod", *h)).unwrap();
        }
        let s = aggregate_at(&lead, &summary, now).unwrap();
        assert_eq!(s.per_repo_env.len(), 1);
        let a = &s.per_repo_env[0];
        assert_eq!(a.sample_count, 5);
        assert!((a.p50_hours - 12.0).abs() < f64::EPSILON);
        assert!((a.p75_hours - 18.0).abs() < f64::EPSILON);
        assert!((a.p95_hours - 96.0).abs() < f64::EPSILON);
        // p50 = 12 → Elite (< 24).
        assert_eq!(a.tier, DoraTier::Elite);
    }

    #[test]
    fn aggregate_filters_out_of_window() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = tmp.path().join("lead_times.jsonl");
        let summary = tmp.path().join("summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        // Inside window
        let inside = (now - Duration::days(5)).to_rfc3339();
        append_lead_time(&lead, &rec(&inside, "r", "prod", 12.0)).unwrap();
        // Outside (45 days ago)
        let outside = (now - Duration::days(45)).to_rfc3339();
        append_lead_time(&lead, &rec(&outside, "r", "prod", 200.0)).unwrap();
        let s = aggregate_at(&lead, &summary, now).unwrap();
        assert_eq!(s.records_scanned, 2);
        assert_eq!(s.per_repo_env[0].sample_count, 1);
        assert!((s.per_repo_env[0].p50_hours - 12.0).abs() < f64::EPSILON);
    }

    #[test]
    fn aggregate_skips_garbage_lead_time() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = tmp.path().join("lead_times.jsonl");
        let summary = tmp.path().join("summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        let ts = (now - Duration::days(1)).to_rfc3339();
        append_lead_time(&lead, &rec(&ts, "r", "prod", -1.0)).unwrap();
        append_lead_time(&lead, &rec(&ts, "r", "prod", f64::NAN)).unwrap();
        append_lead_time(&lead, &rec(&ts, "r", "prod", 6.0)).unwrap();
        let s = aggregate_at(&lead, &summary, now).unwrap();
        assert_eq!(s.per_repo_env[0].sample_count, 1);
    }
}
