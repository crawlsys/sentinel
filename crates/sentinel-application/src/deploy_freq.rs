//! Deploy Frequency Tracker (SEN-9, DORA core metric #2).
//!
//! Tracks deployments to staging and production environments across firefly
//! repos. The on-disk store is `~/.claude/sentinel/metrics/deploys.jsonl` —
//! one [`DeployRecord`] per line, append-only. Producers are:
//!
//!   * Hookdeck webhook handlers for `deployment.success` from Vercel + Railway
//!     (the wiring lives in `channel_events` / `webhook_replay`; this module
//!     only exposes the [`append_deploy`] seam).
//!   * Manual CLI entry via `sentinel deploy-freq record` (smoke testing,
//!     90-day backfill imports).
//!
//! Consumers are:
//!
//!   * `sentinel deploy-freq aggregate` — rolling 7d / 30d deploys-per-day
//!     plus DORA classification, written to `deploys-summary.json`.
//!   * SENTINEL-10 (lead time) and SENTINEL-11 (CFR + MTTR) downstream tasks
//!     join this stream by `commit` + `timestamp`.
//!
//! ## Schema
//!
//! ```json
//! {
//!   "timestamp": "2026-05-15T19:12:00Z",
//!   "repo": "firefly-pro-crm",
//!   "env": "prod",
//!   "commit": "abc1234",
//!   "duration_s": 47
//! }
//! ```
//!
//! `timestamp` is RFC3339 UTC; `env` is one of `prod` / `staging` / `preview`;
//! `duration_s` is best-effort and may be `None` when the webhook doesn't carry it.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// One deploy event appended to `deploys.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployRecord {
    /// When the deploy completed (RFC3339 UTC).
    pub timestamp: String,
    /// Repository identifier (e.g. `firefly-pro-crm`).
    pub repo: String,
    /// Environment: `prod`, `staging`, or `preview`.
    pub env: String,
    /// Git commit SHA the deploy shipped.
    pub commit: String,
    /// Pipeline duration in seconds. `None` when the source webhook omits it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration_s: Option<u64>,
}

/// DORA performance tier for deploy frequency. The thresholds follow the
/// Accelerate / DORA State of DevOps report:
///
/// | Tier   | Frequency           |
/// |--------|---------------------|
/// | Elite  | Multiple per day    |
/// | High   | Daily–weekly        |
/// | Medium | Weekly–monthly      |
/// | Low    | < Monthly           |
///
/// We classify on the 30-day deploys-per-day rate so a single bursty day
/// doesn't flip the tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DoraTier {
    Elite,
    High,
    Medium,
    Low,
}

impl DoraTier {
    /// Classify a deploys-per-day rate computed over a 30-day window.
    ///
    /// Boundaries (deploys/day):
    ///   * `>= 1.0`   → Elite (multiple per day on average)
    ///   * `>= 1/7`   → High (~daily to weekly)
    ///   * `>= 1/30`  → Medium (~weekly to monthly)
    ///   * else       → Low
    #[must_use]
    pub fn from_rate_per_day(rate: f64) -> Self {
        if rate >= 1.0 {
            Self::Elite
        } else if rate >= 1.0 / 7.0 {
            Self::High
        } else if rate >= 1.0 / 30.0 {
            Self::Medium
        } else {
            Self::Low
        }
    }

    /// Classify lead time for changes (DORA #1, SEN-10). Boundaries follow
    /// the Accelerate / DORA State of DevOps report and **must** match the
    /// dashboard's `apps/dashboard/src/domain/dora.ts::tierFor("lead_time")`
    /// so the Rust collectors and the TS UI agree on tier labels.
    ///
    /// Boundaries (hours):
    ///   * `< 24`    → Elite (less than a day)
    ///   * `< 168`   → High  (less than a week)
    ///   * `< 720`   → Medium (less than ~a month)
    ///   * else      → Low
    ///
    /// Non-finite or negative inputs fall through to `Low` rather than
    /// panicking — collectors can be lossy about historical data and a
    /// bogus row shouldn't take the aggregator down.
    #[must_use]
    pub fn from_lead_time_hours(hours: f64) -> Self {
        if !hours.is_finite() || hours < 0.0 {
            return Self::Low;
        }
        if hours < 24.0 {
            Self::Elite
        } else if hours < 168.0 {
            Self::High
        } else if hours < 720.0 {
            Self::Medium
        } else {
            Self::Low
        }
    }

    /// Classify change failure rate (DORA #3, SEN-11). Input is a ratio in
    /// `[0, 1]`; mirrors `tierFor("change_failure_rate")` in the dashboard.
    ///
    /// Boundaries (ratio):
    ///   * `<= 0.15` → Elite
    ///   * `<= 0.30` → High
    ///   * `<= 0.45` → Medium
    ///   * else      → Low
    ///
    /// Out-of-range / non-finite inputs clamp into `[0, 1]` first; collectors
    /// can emit floating-point overshoot near boundary values and we'd
    /// rather classify than refuse to report.
    #[must_use]
    pub fn from_change_failure_rate(rate: f64) -> Self {
        let r = if rate.is_finite() { rate.clamp(0.0, 1.0) } else { 0.0 };
        if r <= 0.15 {
            Self::Elite
        } else if r <= 0.30 {
            Self::High
        } else if r <= 0.45 {
            Self::Medium
        } else {
            Self::Low
        }
    }

    /// Classify Mean Time To Recover (DORA #4, SEN-11). Mirrors
    /// `tierFor("mttr")` in the dashboard.
    ///
    /// Boundaries (hours):
    ///   * `< 1`    → Elite (less than an hour)
    ///   * `< 24`   → High  (less than a day)
    ///   * `< 168`  → Medium (less than a week)
    ///   * else     → Low
    #[must_use]
    pub fn from_mttr_hours(hours: f64) -> Self {
        if !hours.is_finite() || hours < 0.0 {
            return Self::Low;
        }
        if hours < 1.0 {
            Self::Elite
        } else if hours < 24.0 {
            Self::High
        } else if hours < 168.0 {
            Self::Medium
        } else {
            Self::Low
        }
    }
}

/// Per-(repo, env) aggregate written into the summary JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEnvAggregate {
    pub repo: String,
    pub env: String,
    pub deploys_7d: u64,
    pub deploys_30d: u64,
    pub rate_per_day_7d: f64,
    pub rate_per_day_30d: f64,
    pub tier: DoraTier,
    /// First and last deploy timestamps within the 30-day window. Useful for
    /// sparkline cropping. `None` when the window had zero deploys.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_in_window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_in_window: Option<String>,
}

/// One day's deploy count per (repo, env). Used to drive a 60-day sparkline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyPoint {
    pub date: String,
    pub repo: String,
    pub env: String,
    pub deploys: u64,
}

/// Summary file shape, written to `deploys-summary.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploySummary {
    pub generated_at: String,
    pub records_scanned: u64,
    pub aggregates: Vec<RepoEnvAggregate>,
    pub daily_points: Vec<DailyPoint>,
}

/// Append a deploy record to `deploys.jsonl`. Creates the file and parent
/// directory if missing. Atomic per line (one `write_all` of a single JSON
/// object + newline).
///
/// # Errors
/// Returns the underlying IO / serde error when the file cannot be opened,
/// the record can't be serialized, or the write fails.
pub fn append_deploy(path: &Path, record: &DeployRecord) -> Result<()> {
    ensure_parent(path)?;
    let line = serde_json::to_string(record).context("serialize DeployRecord")?;
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

/// Read all records from `deploys.jsonl`.
///
/// Skips lines that fail to parse so a single corrupt entry doesn't poison
/// the whole aggregation — corrupted lines are logged via `tracing::warn`
/// for the operator to investigate. Returns an empty vec when the file
/// doesn't exist (first-run case).
///
/// # Errors
/// Returns IO errors other than file-not-found (which is treated as empty).
pub fn read_deploys(path: &Path) -> Result<Vec<DeployRecord>> {
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
        match serde_json::from_str::<DeployRecord>(&line) {
            Ok(rec) => out.push(rec),
            Err(e) => {
                tracing::warn!(
                    file = %path.display(),
                    line_no = n + 1,
                    error = %e,
                    "deploys.jsonl: skipping malformed line"
                );
            }
        }
    }
    Ok(out)
}

/// Aggregate deploys.jsonl into rolling 7d / 30d counts per (repo, env), plus
/// a 60-day daily sparkline series. Writes `deploys-summary.json` and returns
/// the structure.
///
/// `now` is injected so tests can pin the window; production callers pass
/// `Utc::now()`.
///
/// # Errors
/// Returns the read or write error if the JSONL cannot be loaded or the
/// summary file cannot be persisted.
type Key = (String, String);

pub fn aggregate_at(
    deploys_path: &Path,
    summary_path: &Path,
    now: DateTime<Utc>,
) -> Result<DeploySummary> {
    let records = read_deploys(deploys_path)?;

    let cutoff_7d = now - Duration::days(7);
    let cutoff_30d = now - Duration::days(30);
    let cutoff_60d = now - Duration::days(60);

    let mut buckets_7d: BTreeMap<Key, u64> = BTreeMap::new();
    let mut buckets_30d: BTreeMap<Key, u64> = BTreeMap::new();
    let mut first_in_30d: BTreeMap<Key, DateTime<Utc>> = BTreeMap::new();
    let mut last_in_30d: BTreeMap<Key, DateTime<Utc>> = BTreeMap::new();
    let mut daily: BTreeMap<(NaiveDate, String, String), u64> = BTreeMap::new();

    for rec in &records {
        let ts = match DateTime::parse_from_rfc3339(&rec.timestamp) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => continue,
        };
        let key = (rec.repo.clone(), rec.env.clone());

        if ts >= cutoff_30d {
            *buckets_30d.entry(key.clone()).or_insert(0) += 1;
            first_in_30d
                .entry(key.clone())
                .and_modify(|t| {
                    if ts < *t {
                        *t = ts;
                    }
                })
                .or_insert(ts);
            last_in_30d
                .entry(key.clone())
                .and_modify(|t| {
                    if ts > *t {
                        *t = ts;
                    }
                })
                .or_insert(ts);
        }
        if ts >= cutoff_7d {
            *buckets_7d.entry(key.clone()).or_insert(0) += 1;
        }
        if ts >= cutoff_60d {
            let date = ts.date_naive();
            *daily
                .entry((date, rec.repo.clone(), rec.env.clone()))
                .or_insert(0) += 1;
        }
    }

    let mut all_keys: std::collections::BTreeSet<Key> = std::collections::BTreeSet::new();
    all_keys.extend(buckets_30d.keys().cloned());
    all_keys.extend(buckets_7d.keys().cloned());

    let mut aggregates = Vec::with_capacity(all_keys.len());
    for key in all_keys {
        let d7 = buckets_7d.get(&key).copied().unwrap_or(0);
        let d30 = buckets_30d.get(&key).copied().unwrap_or(0);
        #[allow(clippy::cast_precision_loss)]
        let rate_7 = d7 as f64 / 7.0;
        #[allow(clippy::cast_precision_loss)]
        let rate_30 = d30 as f64 / 30.0;
        aggregates.push(RepoEnvAggregate {
            repo: key.0.clone(),
            env: key.1.clone(),
            deploys_7d: d7,
            deploys_30d: d30,
            rate_per_day_7d: rate_7,
            rate_per_day_30d: rate_30,
            tier: DoraTier::from_rate_per_day(rate_30),
            first_in_window: first_in_30d.get(&key).map(DateTime::to_rfc3339),
            last_in_window: last_in_30d.get(&key).map(DateTime::to_rfc3339),
        });
    }

    let daily_points: Vec<DailyPoint> = daily
        .into_iter()
        .map(|((d, r, e), n)| DailyPoint {
            date: d.to_string(),
            repo: r,
            env: e,
            deploys: n,
        })
        .collect();

    let summary = DeploySummary {
        generated_at: now.to_rfc3339(),
        records_scanned: records.len() as u64,
        aggregates,
        daily_points,
    };

    ensure_parent(summary_path)?;
    let mut f =
        File::create(summary_path).with_context(|| format!("create {}", summary_path.display()))?;
    f.write_all(serde_json::to_string_pretty(&summary)?.as_bytes())?;
    f.flush()?;

    Ok(summary)
}

/// Production entry point — uses `Utc::now()` as the window anchor.
///
/// # Errors
/// Forwards errors from [`aggregate_at`].
pub fn aggregate(deploys_path: &Path, summary_path: &Path) -> Result<DeploySummary> {
    aggregate_at(deploys_path, summary_path, Utc::now())
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

    fn rec(ts: &str, repo: &str, env: &str) -> DeployRecord {
        DeployRecord {
            timestamp: ts.to_string(),
            repo: repo.to_string(),
            env: env.to_string(),
            commit: "abc1234".to_string(),
            duration_s: Some(42),
        }
    }

    #[test]
    fn tier_elite_at_one_per_day() {
        assert_eq!(DoraTier::from_rate_per_day(1.0), DoraTier::Elite);
        assert_eq!(DoraTier::from_rate_per_day(5.0), DoraTier::Elite);
    }

    #[test]
    fn tier_high_at_weekly_or_better() {
        assert_eq!(DoraTier::from_rate_per_day(1.0 / 7.0), DoraTier::High);
        assert_eq!(DoraTier::from_rate_per_day(0.5), DoraTier::High);
    }

    #[test]
    fn tier_medium_at_monthly_or_better() {
        assert_eq!(DoraTier::from_rate_per_day(1.0 / 30.0), DoraTier::Medium);
        assert_eq!(DoraTier::from_rate_per_day(0.1), DoraTier::Medium);
    }

    #[test]
    fn tier_low_below_monthly() {
        assert_eq!(DoraTier::from_rate_per_day(0.01), DoraTier::Low);
        assert_eq!(DoraTier::from_rate_per_day(0.0), DoraTier::Low);
    }

    #[test]
    fn tier_boundary_exact() {
        assert_eq!(DoraTier::from_rate_per_day(1.0), DoraTier::Elite);
        assert_eq!(DoraTier::from_rate_per_day(1.0 / 7.0), DoraTier::High);
        assert_eq!(DoraTier::from_rate_per_day(1.0 / 30.0), DoraTier::Medium);
    }

    #[test]
    fn append_creates_file_and_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("deploys.jsonl");
        let r = rec("2026-05-15T19:00:00Z", "firefly-pro-crm", "prod");
        append_deploy(&path, &r).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("firefly-pro-crm"));
        assert!(content.contains("prod"));
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn append_multiple_lines_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deploys.jsonl");
        for i in 0..3 {
            let r = rec(
                &format!("2026-05-{:02}T12:00:00Z", 10 + i),
                "firefly-pro-crm",
                "prod",
            );
            append_deploy(&path, &r).unwrap();
        }
        let records = read_deploys(&path).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].repo, "firefly-pro-crm");
        assert_eq!(records[2].timestamp, "2026-05-12T12:00:00Z");
    }

    #[test]
    fn read_skips_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deploys.jsonl");
        let good = serde_json::to_string(&rec("2026-05-15T19:00:00Z", "x", "prod")).unwrap();
        let content = format!("{good}\nnot json\n{good}\n");
        std::fs::write(&path, content).unwrap();
        let records = read_deploys(&path).unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn read_nonexistent_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("absent.jsonl");
        let records = read_deploys(&path).unwrap();
        assert!(records.is_empty());
    }

    #[test]
    fn aggregate_groups_by_repo_and_env() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let summary = tmp.path().join("deploys-summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();

        for d in 1..=5 {
            let ts = (now - Duration::days(d)).to_rfc3339();
            append_deploy(&deploys, &rec(&ts, "firefly-pro-crm", "prod")).unwrap();
        }
        for d in 1..=2 {
            let ts = (now - Duration::days(d)).to_rfc3339();
            append_deploy(&deploys, &rec(&ts, "firefly-pro-crm", "staging")).unwrap();
        }
        let ts = (now - Duration::days(20)).to_rfc3339();
        append_deploy(&deploys, &rec(&ts, "firefly-pro-app", "prod")).unwrap();

        let s = aggregate_at(&deploys, &summary, now).unwrap();
        assert_eq!(s.records_scanned, 8);
        assert_eq!(s.aggregates.len(), 3);

        let crm_prod = s
            .aggregates
            .iter()
            .find(|a| a.repo == "firefly-pro-crm" && a.env == "prod")
            .expect("crm/prod aggregate");
        assert_eq!(crm_prod.deploys_7d, 5);
        assert_eq!(crm_prod.deploys_30d, 5);

        let app_prod = s
            .aggregates
            .iter()
            .find(|a| a.repo == "firefly-pro-app" && a.env == "prod")
            .expect("app/prod aggregate");
        assert_eq!(app_prod.deploys_7d, 0);
        assert_eq!(app_prod.deploys_30d, 1);
    }

    #[test]
    fn aggregate_excludes_records_outside_30d() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let summary = tmp.path().join("deploys-summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();

        let ts = (now - Duration::days(40)).to_rfc3339();
        append_deploy(&deploys, &rec(&ts, "r1", "prod")).unwrap();
        let ts = (now - Duration::days(5)).to_rfc3339();
        append_deploy(&deploys, &rec(&ts, "r1", "prod")).unwrap();

        let s = aggregate_at(&deploys, &summary, now).unwrap();
        assert_eq!(s.records_scanned, 2);
        let r1 = &s.aggregates[0];
        assert_eq!(r1.deploys_7d, 1);
        assert_eq!(r1.deploys_30d, 1, "40-day-old record must not count");
    }

    #[test]
    fn aggregate_writes_summary_json() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let summary = tmp.path().join("deploys-summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        let ts = (now - Duration::days(1)).to_rfc3339();
        append_deploy(&deploys, &rec(&ts, "r", "prod")).unwrap();

        aggregate_at(&deploys, &summary, now).unwrap();
        assert!(summary.exists());
        let content = std::fs::read_to_string(&summary).unwrap();
        let parsed: DeploySummary = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.records_scanned, 1);
        assert_eq!(parsed.aggregates.len(), 1);
    }

    #[test]
    fn aggregate_classifies_tier_on_30d_rate() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let summary = tmp.path().join("summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();

        for d in 0..30 {
            let ts = (now - Duration::days(d) - Duration::hours(1)).to_rfc3339();
            append_deploy(&deploys, &rec(&ts, "r", "prod")).unwrap();
        }
        let s = aggregate_at(&deploys, &summary, now).unwrap();
        assert_eq!(s.aggregates[0].tier, DoraTier::Elite);
    }

    #[test]
    fn aggregate_handles_empty_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let summary = tmp.path().join("summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();

        let s = aggregate_at(&deploys, &summary, now).unwrap();
        assert_eq!(s.records_scanned, 0);
        assert!(s.aggregates.is_empty());
        assert!(s.daily_points.is_empty());
        assert!(summary.exists());
    }

    #[test]
    fn aggregate_emits_daily_points_within_60d() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let summary = tmp.path().join("summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();

        for d in [1_i64, 5, 20] {
            let ts = (now - Duration::days(d)).to_rfc3339();
            append_deploy(&deploys, &rec(&ts, "r", "prod")).unwrap();
        }
        let ts = (now - Duration::days(70)).to_rfc3339();
        append_deploy(&deploys, &rec(&ts, "r", "prod")).unwrap();

        let s = aggregate_at(&deploys, &summary, now).unwrap();
        assert_eq!(s.daily_points.len(), 3, "70-day-old point excluded");
    }

    #[test]
    fn aggregate_skips_unparseable_timestamps() {
        let tmp = tempfile::tempdir().unwrap();
        let deploys = tmp.path().join("deploys.jsonl");
        let summary = tmp.path().join("summary.json");
        let now = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();

        append_deploy(&deploys, &rec("not-a-timestamp", "r", "prod")).unwrap();
        let ts = (now - Duration::days(1)).to_rfc3339();
        append_deploy(&deploys, &rec(&ts, "r", "prod")).unwrap();

        let s = aggregate_at(&deploys, &summary, now).unwrap();
        assert_eq!(s.records_scanned, 2);
        assert_eq!(s.aggregates[0].deploys_7d, 1);
    }

    // --- SEN-10 lead time classifier boundaries -------------------------

    #[test]
    fn from_lead_time_hours_classifies_at_boundaries() {
        assert_eq!(DoraTier::from_lead_time_hours(0.0), DoraTier::Elite);
        assert_eq!(DoraTier::from_lead_time_hours(23.99), DoraTier::Elite);
        assert_eq!(DoraTier::from_lead_time_hours(24.0), DoraTier::High);
        assert_eq!(DoraTier::from_lead_time_hours(167.99), DoraTier::High);
        assert_eq!(DoraTier::from_lead_time_hours(168.0), DoraTier::Medium);
        assert_eq!(DoraTier::from_lead_time_hours(719.99), DoraTier::Medium);
        assert_eq!(DoraTier::from_lead_time_hours(720.0), DoraTier::Low);
        assert_eq!(DoraTier::from_lead_time_hours(10_000.0), DoraTier::Low);
    }

    #[test]
    fn from_lead_time_hours_handles_garbage_input() {
        assert_eq!(DoraTier::from_lead_time_hours(-1.0), DoraTier::Low);
        assert_eq!(DoraTier::from_lead_time_hours(f64::NAN), DoraTier::Low);
        assert_eq!(DoraTier::from_lead_time_hours(f64::INFINITY), DoraTier::Low);
    }

    // --- SEN-11 CFR + MTTR classifier boundaries ------------------------

    #[test]
    fn from_change_failure_rate_classifies_at_boundaries() {
        assert_eq!(DoraTier::from_change_failure_rate(0.0), DoraTier::Elite);
        assert_eq!(DoraTier::from_change_failure_rate(0.15), DoraTier::Elite);
        assert_eq!(DoraTier::from_change_failure_rate(0.150001), DoraTier::High);
        assert_eq!(DoraTier::from_change_failure_rate(0.30), DoraTier::High);
        assert_eq!(
            DoraTier::from_change_failure_rate(0.30001),
            DoraTier::Medium
        );
        assert_eq!(DoraTier::from_change_failure_rate(0.45), DoraTier::Medium);
        assert_eq!(DoraTier::from_change_failure_rate(0.46), DoraTier::Low);
        assert_eq!(DoraTier::from_change_failure_rate(1.0), DoraTier::Low);
    }

    #[test]
    fn from_change_failure_rate_clamps_garbage_input() {
        assert_eq!(DoraTier::from_change_failure_rate(-0.5), DoraTier::Elite);
        assert_eq!(DoraTier::from_change_failure_rate(2.5), DoraTier::Low);
        assert_eq!(
            DoraTier::from_change_failure_rate(f64::NAN),
            DoraTier::Elite
        );
    }

    #[test]
    fn from_mttr_hours_classifies_at_boundaries() {
        assert_eq!(DoraTier::from_mttr_hours(0.0), DoraTier::Elite);
        assert_eq!(DoraTier::from_mttr_hours(0.99), DoraTier::Elite);
        assert_eq!(DoraTier::from_mttr_hours(1.0), DoraTier::High);
        assert_eq!(DoraTier::from_mttr_hours(23.99), DoraTier::High);
        assert_eq!(DoraTier::from_mttr_hours(24.0), DoraTier::Medium);
        assert_eq!(DoraTier::from_mttr_hours(167.99), DoraTier::Medium);
        assert_eq!(DoraTier::from_mttr_hours(168.0), DoraTier::Low);
    }
}
