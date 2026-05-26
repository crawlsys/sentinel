//! SLA Breach Detection Engine (SEN-12).
//!
//! Pure matching + aging logic for service-level-agreement breach detection.
//! Configurable via `~/.claude/sentinel/config/slas.toml`; the live polling
//! adapters (Linear / GitHub / deploy-stream) are separate modules that
//! produce a JSONL of [`Subject`] records and feed them into [`check_rules`].
//!
//! This slice ships the data model and detection logic. Out of scope here:
//!
//!   * Live Linear / GitHub state polling (cron-driven adapter, follow-up).
//!   * Push notification routing on breach (uses the existing
//!     `channel_events` infrastructure, follow-up).
//!   * Auto-comment back to the ticket on breach (Linear adapter, follow-up).
//!
//! ## TOML schema
//!
//! ```toml
//! [[sla]]
//! name = "P0 ticket time-to-pickup"
//! target_minutes = 240             # 4 hours
//! subject_kind = "ticket"
//! require_priority = "urgent"
//! require_state = ["Backlog", "Todo"]
//!
//! [[sla]]
//! name = "PR time-to-first-review"
//! target_minutes = 240
//! subject_kind = "pr"
//! require_state = ["open"]
//! require_no_reviews = true
//! ```
//!
//! ## Subject schema (JSONL input)
//!
//! ```json
//! {"kind":"ticket","id":"FPCRM-329","created_at":"2026-05-13T01:00:00Z",
//!  "state":"Backlog","priority":"urgent","labels":[]}
//! {"kind":"pr","id":"firefly-pro-crm#342","created_at":"...",
//!  "state":"open","reviews":0}
//! ```
//!
//! ## Breach output (JSONL)
//!
//! ```json
//! {"sla":"P0 ticket time-to-pickup","subject_id":"FPCRM-329",
//!  "subject_kind":"ticket","detected_at":"...",
//!  "actual_minutes":520,"target_minutes":240,"overdue_by_minutes":280}
//! ```

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// One SLA rule from `slas.toml`. A breach fires when a [`Subject`] matches
/// every `require_*` filter AND its age exceeds `target_minutes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlaRule {
    /// Human-readable identifier surfaced in breach records + dashboards.
    /// Not required to be unique — duplicates fire independently.
    pub name: String,
    /// Allowed time-in-state before this rule trips, in minutes.
    pub target_minutes: u64,
    /// Subject kind this rule applies to: `ticket`, `pr`, or `deploy`.
    pub subject_kind: String,
    /// Optional priority filter (e.g. `"urgent"`). When `None`, any priority matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_priority: Option<String>,
    /// Optional state allowlist (e.g. `["Backlog", "Todo"]`). When empty,
    /// any state matches.
    #[serde(default)]
    pub require_state: Vec<String>,
    /// Optional label requirement — subject must carry every label listed
    /// here (AND, not OR). When empty, no label filtering.
    #[serde(default)]
    pub require_labels: Vec<String>,
    /// When true, only matches subjects whose `reviews` count is zero.
    /// Used for the "PR time-to-first-review" SLA. Defaults to false.
    #[serde(default)]
    pub require_no_reviews: bool,
}

/// Top-level config shape — a list of SLA rules.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SlaConfig {
    #[serde(default, rename = "sla")]
    pub rules: Vec<SlaRule>,
}

/// A flat subject input. Optional fields are set per kind; the rule's
/// `require_*` filters skip subjects that don't supply the relevant field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subject {
    /// `ticket`, `pr`, or `deploy`. Must match a rule's `subject_kind`.
    pub kind: String,
    /// Stable identifier for the subject (e.g. `FPCRM-329`, `repo#42`).
    pub id: String,
    /// RFC3339 UTC timestamp when the subject entered the state being aged.
    /// For tickets this is typically the most recent state transition; for
    /// PRs it's `opened_at`. The adapter is responsible for picking the
    /// right timestamp.
    pub created_at: String,
    /// Current state (`Backlog`, `In Progress`, `open`, etc).
    #[serde(default)]
    pub state: Option<String>,
    /// Priority label (`urgent`, `high`, `medium`, `low`, `none`).
    #[serde(default)]
    pub priority: Option<String>,
    /// Labels attached to the subject.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Number of code reviews submitted (PR subjects only). `None` is
    /// treated the same as `Some(0)` by `require_no_reviews`.
    #[serde(default)]
    pub reviews: Option<u32>,
}

/// One detected breach, written to `sla-breaches.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreachRecord {
    /// The SLA rule's `name`. Duplicates allowed (matches multiple rules).
    pub sla: String,
    /// The breaching subject's id (e.g. `FPCRM-329`).
    pub subject_id: String,
    /// The subject's kind, for filtering downstream.
    pub subject_kind: String,
    /// When the breach was detected (RFC3339 UTC).
    pub detected_at: String,
    /// Minutes the subject had been in its state at detection time.
    pub actual_minutes: u64,
    /// The rule's `target_minutes`.
    pub target_minutes: u64,
    /// `actual_minutes - target_minutes`. Always > 0 for a real breach.
    pub overdue_by_minutes: u64,
}

/// Parse [`SlaConfig`] from a TOML string.
///
/// # Errors
/// Returns the underlying TOML parse error.
pub fn parse_config(toml_str: &str) -> Result<SlaConfig> {
    toml::from_str(toml_str).context("parse sla config TOML")
}

/// Load [`SlaConfig`] from a file on disk. Returns an empty config when the
/// file doesn't exist (first-run case) rather than erroring — callers can
/// distinguish "no rules configured" from "broken config" via the rule count.
///
/// # Errors
/// Returns the IO or TOML error on a real parse failure (not file-not-found).
pub fn load_config(path: &Path) -> Result<SlaConfig> {
    if !path.exists() {
        return Ok(SlaConfig::default());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("read sla config {}", path.display()))?;
    parse_config(&content)
}

/// Check a single [`SlaRule`] against a single [`Subject`] at time `now`.
///
/// Returns `Some(BreachRecord)` when the subject matches every filter AND
/// has aged past the rule's target. Returns `None` otherwise — including
/// for unparseable `created_at` timestamps (logged via `tracing::warn`).
#[must_use]
pub fn check(rule: &SlaRule, subject: &Subject, now: DateTime<Utc>) -> Option<BreachRecord> {
    if rule.subject_kind != subject.kind {
        return None;
    }
    if let Some(required) = &rule.require_priority {
        if subject.priority.as_deref() != Some(required.as_str()) {
            return None;
        }
    }
    if !rule.require_state.is_empty() {
        let s = subject.state.as_deref().unwrap_or("");
        if !rule.require_state.iter().any(|x| x == s) {
            return None;
        }
    }
    if !rule.require_labels.is_empty() {
        for label in &rule.require_labels {
            if !subject.labels.iter().any(|l| l == label) {
                return None;
            }
        }
    }
    if rule.require_no_reviews && subject.reviews.unwrap_or(0) > 0 {
        return None;
    }

    let created = match DateTime::parse_from_rfc3339(&subject.created_at) {
        Ok(t) => t.with_timezone(&Utc),
        Err(e) => {
            tracing::warn!(
                subject_id = %subject.id,
                created_at = %subject.created_at,
                error = %e,
                "sla: skipping subject with unparseable created_at"
            );
            return None;
        }
    };

    let age_minutes = (now - created).num_minutes();
    if age_minutes <= 0 {
        return None; // future-dated subject — treat as not-aged
    }
    #[allow(clippy::cast_sign_loss)]
    let actual = age_minutes as u64;
    if actual <= rule.target_minutes {
        return None;
    }

    Some(BreachRecord {
        sla: rule.name.clone(),
        subject_id: subject.id.clone(),
        subject_kind: subject.kind.clone(),
        detected_at: now.to_rfc3339(),
        actual_minutes: actual,
        target_minutes: rule.target_minutes,
        overdue_by_minutes: actual - rule.target_minutes,
    })
}

/// Apply every rule in `config` against every subject in `subjects` at time
/// `now`. Returns the flat list of all breaches. Order: rules outer,
/// subjects inner — so a subject matching three rules produces three
/// breaches, all adjacent.
#[must_use]
pub fn check_rules(
    config: &SlaConfig,
    subjects: &[Subject],
    now: DateTime<Utc>,
) -> Vec<BreachRecord> {
    let mut out = Vec::new();
    for rule in &config.rules {
        for subject in subjects {
            if let Some(b) = check(rule, subject, now) {
                out.push(b);
            }
        }
    }
    out
}

/// Append a [`BreachRecord`] to `sla-breaches.jsonl`. Creates the file and
/// parent directory if missing.
///
/// # Errors
/// Returns the IO or serde error.
pub fn append_breach(path: &Path, record: &BreachRecord) -> Result<()> {
    ensure_parent(path)?;
    let line = serde_json::to_string(record).context("serialize BreachRecord")?;
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

/// Read all breach records from `sla-breaches.jsonl`. Skips malformed lines
/// with a `tracing::warn`. Empty vec when the file doesn't exist.
///
/// # Errors
/// Returns IO errors other than file-not-found.
pub fn read_breaches(path: &Path) -> Result<Vec<BreachRecord>> {
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
        match serde_json::from_str::<BreachRecord>(&line) {
            Ok(rec) => out.push(rec),
            Err(e) => {
                tracing::warn!(
                    file = %path.display(),
                    line_no = n + 1,
                    error = %e,
                    "sla-breaches.jsonl: skipping malformed line"
                );
            }
        }
    }
    Ok(out)
}

/// Per-SLA breach summary written to `sla-breaches-summary.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlaAggregate {
    /// SLA rule name (matches [`BreachRecord::sla`]).
    pub sla: String,
    pub breaches_24h: u64,
    pub breaches_7d: u64,
    pub breaches_30d: u64,
    /// The most recent breach for this SLA in the 30-day window, RFC3339.
    /// `None` when the SLA had no breaches in the window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub most_recent: Option<String>,
}

/// Summary file shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreachesSummary {
    pub generated_at: String,
    pub records_scanned: u64,
    pub aggregates: Vec<SlaAggregate>,
}

/// Aggregate breach counts per SLA in 24h / 7d / 30d windows. `now` is
/// injected for testability.
///
/// # Errors
/// Returns read or write errors.
pub fn aggregate_at(
    breaches_path: &Path,
    summary_path: &Path,
    now: DateTime<Utc>,
) -> Result<BreachesSummary> {
    let records = read_breaches(breaches_path)?;

    let cutoff_24h = now - Duration::hours(24);
    let cutoff_7d = now - Duration::days(7);
    let cutoff_30d = now - Duration::days(30);

    // SLA name → (24h, 7d, 30d, latest)
    let mut buckets: BTreeMap<String, (u64, u64, u64, Option<DateTime<Utc>>)> = BTreeMap::new();

    for rec in &records {
        let detected = match DateTime::parse_from_rfc3339(&rec.detected_at) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => continue,
        };
        let entry = buckets.entry(rec.sla.clone()).or_insert((0, 0, 0, None));
        if detected >= cutoff_30d {
            entry.2 += 1;
            entry.3 = Some(match entry.3 {
                Some(prev) if prev > detected => prev,
                _ => detected,
            });
        }
        if detected >= cutoff_7d {
            entry.1 += 1;
        }
        if detected >= cutoff_24h {
            entry.0 += 1;
        }
    }

    let aggregates: Vec<SlaAggregate> = buckets
        .into_iter()
        .map(|(name, (h24, d7, d30, latest))| SlaAggregate {
            sla: name,
            breaches_24h: h24,
            breaches_7d: d7,
            breaches_30d: d30,
            most_recent: latest.map(|t| t.to_rfc3339()),
        })
        .collect();

    let summary = BreachesSummary {
        generated_at: now.to_rfc3339(),
        records_scanned: records.len() as u64,
        aggregates,
    };

    ensure_parent(summary_path)?;
    let mut f =
        File::create(summary_path).with_context(|| format!("create {}", summary_path.display()))?;
    f.write_all(serde_json::to_string_pretty(&summary)?.as_bytes())?;
    f.flush()?;

    Ok(summary)
}

/// Production aggregator — uses `Utc::now()` as the anchor.
///
/// # Errors
/// Forwards [`aggregate_at`] errors.
pub fn aggregate(breaches_path: &Path, summary_path: &Path) -> Result<BreachesSummary> {
    aggregate_at(breaches_path, summary_path, Utc::now())
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

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap()
    }

    fn rule_p0_pickup() -> SlaRule {
        SlaRule {
            name: "P0 ticket time-to-pickup".to_string(),
            target_minutes: 240,
            subject_kind: "ticket".to_string(),
            require_priority: Some("urgent".to_string()),
            require_state: vec!["Backlog".into(), "Todo".into()],
            require_labels: vec![],
            require_no_reviews: false,
        }
    }

    fn ticket(state: &str, priority: &str, age_minutes: i64) -> Subject {
        let created = now() - Duration::minutes(age_minutes);
        Subject {
            kind: "ticket".to_string(),
            id: "FPCRM-1".to_string(),
            created_at: created.to_rfc3339(),
            state: Some(state.to_string()),
            priority: Some(priority.to_string()),
            labels: vec![],
            reviews: None,
        }
    }

    #[test]
    fn parse_config_from_toml() {
        let toml_str = r#"
[[sla]]
name = "P0 pickup"
target_minutes = 240
subject_kind = "ticket"
require_priority = "urgent"
require_state = ["Backlog", "Todo"]

[[sla]]
name = "QA dwell"
target_minutes = 1440
subject_kind = "ticket"
require_state = ["QA Testing"]
"#;
        let cfg = parse_config(toml_str).unwrap();
        assert_eq!(cfg.rules.len(), 2);
        assert_eq!(cfg.rules[0].name, "P0 pickup");
        assert_eq!(cfg.rules[0].target_minutes, 240);
        assert_eq!(cfg.rules[0].require_priority.as_deref(), Some("urgent"));
        assert_eq!(cfg.rules[1].name, "QA dwell");
    }

    #[test]
    fn parse_config_empty() {
        let cfg = parse_config("").unwrap();
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn load_config_returns_empty_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("absent.toml");
        let cfg = load_config(&path).unwrap();
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn check_breaches_when_aged() {
        let rule = rule_p0_pickup();
        let subject = ticket("Backlog", "urgent", 500);
        let breach = check(&rule, &subject, now()).expect("must breach");
        assert_eq!(breach.sla, "P0 ticket time-to-pickup");
        assert_eq!(breach.actual_minutes, 500);
        assert_eq!(breach.target_minutes, 240);
        assert_eq!(breach.overdue_by_minutes, 260);
    }

    #[test]
    fn check_no_breach_when_young() {
        let rule = rule_p0_pickup();
        let subject = ticket("Backlog", "urgent", 60);
        assert!(check(&rule, &subject, now()).is_none());
    }

    #[test]
    fn check_skips_wrong_kind() {
        let rule = rule_p0_pickup();
        let mut subject = ticket("Backlog", "urgent", 500);
        subject.kind = "pr".to_string();
        assert!(check(&rule, &subject, now()).is_none());
    }

    #[test]
    fn check_skips_wrong_state() {
        let rule = rule_p0_pickup();
        let subject = ticket("In Progress", "urgent", 500);
        assert!(check(&rule, &subject, now()).is_none());
    }

    #[test]
    fn check_skips_wrong_priority() {
        let rule = rule_p0_pickup();
        let subject = ticket("Backlog", "low", 500);
        assert!(check(&rule, &subject, now()).is_none());
    }

    #[test]
    fn check_skips_missing_required_label() {
        let mut rule = rule_p0_pickup();
        rule.require_labels = vec!["incident".to_string()];
        let subject = ticket("Backlog", "urgent", 500);
        assert!(check(&rule, &subject, now()).is_none());
    }

    #[test]
    fn check_matches_when_all_labels_present() {
        let mut rule = rule_p0_pickup();
        rule.require_labels = vec!["incident".into(), "p1".into()];
        let mut subject = ticket("Backlog", "urgent", 500);
        subject.labels = vec!["incident".into(), "p1".into(), "extra".into()];
        assert!(check(&rule, &subject, now()).is_some());
    }

    #[test]
    fn check_no_reviews_filter_works() {
        let rule = SlaRule {
            name: "PR first review".into(),
            target_minutes: 240,
            subject_kind: "pr".into(),
            require_priority: None,
            require_state: vec!["open".into()],
            require_labels: vec![],
            require_no_reviews: true,
        };

        let created = now() - Duration::minutes(500);
        let mut pr_no_reviews = Subject {
            kind: "pr".into(),
            id: "repo#1".into(),
            created_at: created.to_rfc3339(),
            state: Some("open".into()),
            priority: None,
            labels: vec![],
            reviews: Some(0),
        };
        assert!(check(&rule, &pr_no_reviews, now()).is_some());

        pr_no_reviews.reviews = Some(1);
        assert!(check(&rule, &pr_no_reviews, now()).is_none());

        // None is treated as 0
        pr_no_reviews.reviews = None;
        assert!(check(&rule, &pr_no_reviews, now()).is_some());
    }

    #[test]
    fn check_skips_unparseable_timestamp() {
        let rule = rule_p0_pickup();
        let subject = Subject {
            kind: "ticket".into(),
            id: "X".into(),
            created_at: "not-a-date".into(),
            state: Some("Backlog".into()),
            priority: Some("urgent".into()),
            labels: vec![],
            reviews: None,
        };
        assert!(check(&rule, &subject, now()).is_none());
    }

    #[test]
    fn check_skips_future_dated_subject() {
        let rule = rule_p0_pickup();
        let future = now() + Duration::hours(1);
        let subject = Subject {
            kind: "ticket".into(),
            id: "X".into(),
            created_at: future.to_rfc3339(),
            state: Some("Backlog".into()),
            priority: Some("urgent".into()),
            labels: vec![],
            reviews: None,
        };
        assert!(check(&rule, &subject, now()).is_none());
    }

    #[test]
    fn check_rules_fires_all_matching_rules() {
        let config = SlaConfig {
            rules: vec![
                rule_p0_pickup(),
                // Second rule also matches: Backlog, no priority filter, longer target.
                SlaRule {
                    name: "Backlog dwell".into(),
                    target_minutes: 60,
                    subject_kind: "ticket".into(),
                    require_priority: None,
                    require_state: vec!["Backlog".into()],
                    require_labels: vec![],
                    require_no_reviews: false,
                },
            ],
        };
        let subject = ticket("Backlog", "urgent", 500);
        let breaches = check_rules(&config, &[subject], now());
        assert_eq!(breaches.len(), 2, "both rules must fire on same subject");
        let names: Vec<&str> = breaches.iter().map(|b| b.sla.as_str()).collect();
        assert!(names.contains(&"P0 ticket time-to-pickup"));
        assert!(names.contains(&"Backlog dwell"));
    }

    #[test]
    fn append_and_read_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("sla-breaches.jsonl");
        let rec = BreachRecord {
            sla: "P0 pickup".into(),
            subject_id: "FPCRM-1".into(),
            subject_kind: "ticket".into(),
            detected_at: now().to_rfc3339(),
            actual_minutes: 500,
            target_minutes: 240,
            overdue_by_minutes: 260,
        };
        append_breach(&path, &rec).unwrap();
        append_breach(&path, &rec).unwrap();
        let read = read_breaches(&path).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0], rec);
    }

    #[test]
    fn read_breaches_skips_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("b.jsonl");
        let good = BreachRecord {
            sla: "X".into(),
            subject_id: "Y".into(),
            subject_kind: "ticket".into(),
            detected_at: now().to_rfc3339(),
            actual_minutes: 500,
            target_minutes: 240,
            overdue_by_minutes: 260,
        };
        let good_line = serde_json::to_string(&good).unwrap();
        std::fs::write(&path, format!("{good_line}\nnot json\n{good_line}\n")).unwrap();
        assert_eq!(read_breaches(&path).unwrap().len(), 2);
    }

    #[test]
    fn aggregate_counts_per_window() {
        let tmp = tempfile::tempdir().unwrap();
        let breaches = tmp.path().join("b.jsonl");
        let summary = tmp.path().join("s.json");

        // 3 breaches of "A" — within 24h, 7d, 30d.
        for h in [1_i64, 12, 23] {
            let r = BreachRecord {
                sla: "A".into(),
                subject_id: "x".into(),
                subject_kind: "ticket".into(),
                detected_at: (now() - Duration::hours(h)).to_rfc3339(),
                actual_minutes: 500,
                target_minutes: 240,
                overdue_by_minutes: 260,
            };
            append_breach(&breaches, &r).unwrap();
        }
        // 1 breach of "A" 10 days ago — only counts in 30d.
        let r = BreachRecord {
            sla: "A".into(),
            subject_id: "x".into(),
            subject_kind: "ticket".into(),
            detected_at: (now() - Duration::days(10)).to_rfc3339(),
            actual_minutes: 500,
            target_minutes: 240,
            overdue_by_minutes: 260,
        };
        append_breach(&breaches, &r).unwrap();
        // 1 breach of "B" 2 days ago — counts in 7d + 30d, not 24h.
        let r = BreachRecord {
            sla: "B".into(),
            subject_id: "y".into(),
            subject_kind: "pr".into(),
            detected_at: (now() - Duration::days(2)).to_rfc3339(),
            actual_minutes: 500,
            target_minutes: 240,
            overdue_by_minutes: 260,
        };
        append_breach(&breaches, &r).unwrap();
        // 1 breach 40 days ago — falls out of every window.
        let r = BreachRecord {
            sla: "A".into(),
            subject_id: "x".into(),
            subject_kind: "ticket".into(),
            detected_at: (now() - Duration::days(40)).to_rfc3339(),
            actual_minutes: 500,
            target_minutes: 240,
            overdue_by_minutes: 260,
        };
        append_breach(&breaches, &r).unwrap();

        let s = aggregate_at(&breaches, &summary, now()).unwrap();
        assert_eq!(s.records_scanned, 6);

        let a = s.aggregates.iter().find(|a| a.sla == "A").unwrap();
        assert_eq!(a.breaches_24h, 3);
        assert_eq!(a.breaches_7d, 3);
        assert_eq!(a.breaches_30d, 4, "40d breach excluded, 10d included");
        assert!(a.most_recent.is_some());

        let b = s.aggregates.iter().find(|a| a.sla == "B").unwrap();
        assert_eq!(b.breaches_24h, 0);
        assert_eq!(b.breaches_7d, 1);
        assert_eq!(b.breaches_30d, 1);
    }

    #[test]
    fn aggregate_handles_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let breaches = tmp.path().join("b.jsonl");
        let summary = tmp.path().join("s.json");
        let s = aggregate_at(&breaches, &summary, now()).unwrap();
        assert_eq!(s.records_scanned, 0);
        assert!(s.aggregates.is_empty());
        assert!(summary.exists());
    }

    #[test]
    fn aggregate_picks_latest_in_window() {
        let tmp = tempfile::tempdir().unwrap();
        let breaches = tmp.path().join("b.jsonl");
        let summary = tmp.path().join("s.json");
        let older = (now() - Duration::days(5)).to_rfc3339();
        let newer = (now() - Duration::hours(2)).to_rfc3339();
        for ts in [&older, &newer] {
            let r = BreachRecord {
                sla: "X".into(),
                subject_id: "id".into(),
                subject_kind: "ticket".into(),
                detected_at: ts.clone(),
                actual_minutes: 500,
                target_minutes: 240,
                overdue_by_minutes: 260,
            };
            append_breach(&breaches, &r).unwrap();
        }
        let s = aggregate_at(&breaches, &summary, now()).unwrap();
        let x = s.aggregates.iter().find(|a| a.sla == "X").unwrap();
        assert_eq!(x.most_recent.as_deref(), Some(newer.as_str()));
    }
}
