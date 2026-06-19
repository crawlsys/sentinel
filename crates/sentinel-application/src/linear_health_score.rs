//! Composite Linear health score (the "is the board actually healthy" gauge).
//!
//! Folds the Linear issue cache into a single 0-100 score across four
//! weighted dimensions, mirroring the by-hand 72/100 scored against the
//! Firefly Pro Beta initiative. Each dimension has a hard cap; the total is
//! their sum, rounded, plus a grade band.
//!
//! ## Input
//!
//! The Linear cache at `~/.claude/sentinel/linear-assigned.json` — the same
//! permissive `{ "issues": [...] }`-or-bare-array file the rest of the suite
//! reads. Per issue we use `estimate` and `state` (name + type).
//!
//! ## The four dimensions
//!
//! * **hygiene** (max [`MAX_HYGIENE`] = 30) — fraction of issues carrying a
//!   valid Fibonacci estimate, times 30. Missing/off-scale estimates are the
//!   single biggest signal that planning hasn't happened.
//! * **structure** (max [`MAX_STRUCTURE`] = 20) — fraction of issues with a
//!   non-empty state, times 20. A proxy for "tracked properly" (an issue
//!   with no workflow state is floating outside the process).
//! * `data_quality` (max [`MAX_DATA_QUALITY`] = 15) — starts at 15 and
//!   subtracts [`DATA_QUALITY_PENALTY_PER_FAIL`] for every "QA Failed" issue
//!   beyond [`DATA_QUALITY_FREE_FAILS`] (rework signal), floored at 0.
//!   **Heuristic** — QA-Failed count is a rough proxy for churn/rework, not a
//!   precise data-quality measure; tune the constants as the board evolves.
//! * **flow** (max [`MAX_FLOW`] = 35) — penalizes QA-lane congestion. We
//!   compute the fraction of open (non-done) story points sitting in any QA
//!   state and score `35 * (1 - qa_congestion_fraction)`. A board where most
//!   in-flight points are stuck waiting on QA is not flowing.
//!
//! Output is written to `~/.claude/sentinel/metrics/linear-health.json`
//! (summary). There is no per-row `.jsonl` here — the score is a single
//! board-level verdict, not a list of flagged items.

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::{self};
use std::path::Path;

/// Valid Linear Fibonacci estimate values. Anything else is "off-scale".
pub const FIBONACCI: &[u8] = &[1, 2, 3, 5, 8, 13, 21];

/// Per-dimension caps. Sum is 100 (asserted in tests).
pub const MAX_HYGIENE: f64 = 30.0;
pub const MAX_STRUCTURE: f64 = 20.0;
pub const MAX_DATA_QUALITY: f64 = 15.0;
pub const MAX_FLOW: f64 = 35.0;

/// Number of "QA Failed" issues tolerated before `data_quality` starts
/// losing points. A board is allowed a little rework without penalty.
pub const DATA_QUALITY_FREE_FAILS: usize = 1;

/// Points subtracted from `data_quality` per "QA Failed" issue above the
/// free allowance.
pub const DATA_QUALITY_PENALTY_PER_FAIL: f64 = 3.0;

/// State `type` values (lowercased) that count as "done".
const DONE_TYPES: &[&str] = &["completed", "canceled"];

/// Grade band thresholds.
pub const GRADE_HEALTHY_MIN: f64 = 85.0;
pub const GRADE_OK_MIN: f64 = 70.0;

/// A normalized issue parsed out of the cache.
#[derive(Debug, Clone)]
struct Issue {
    estimate: Option<f64>,
    state_name: String,
    state_type: String,
}

impl Issue {
    fn is_done(&self) -> bool {
        DONE_TYPES.contains(&self.state_type.as_str())
    }
    fn has_state(&self) -> bool {
        !self.state_name.is_empty() || !self.state_type.is_empty()
    }
    fn is_qa_lane(&self) -> bool {
        self.state_name.to_lowercase().contains("qa")
    }
    fn is_qa_failed(&self) -> bool {
        self.state_name.eq_ignore_ascii_case("QA Failed")
    }
}

/// The full health summary written to `linear-health.json`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct HealthSummary {
    pub issues_total: usize,
    /// Composite 0-100 score (sum of the four dimensions, rounded).
    pub total_score: u32,
    pub hygiene_score: f64,
    pub structure_score: f64,
    pub data_quality_score: f64,
    pub flow_score: f64,
    /// Fraction of open story points sitting in a QA lane (the flow penalty
    /// driver). 0.0 when there are no open points.
    pub qa_congestion_fraction: f64,
    /// Count of issues currently in a "QA Failed" state (`data_quality` driver).
    pub qa_failed_count: usize,
    /// One of `healthy` (>=85), `ok` (70-84), `needs-work` (<70).
    pub grade: String,
}

/// Run the health score over `linear_cache`, write `output_summary` (JSON),
/// return the summary.
pub fn scan_health_score(linear_cache: &Path, output_summary: &Path) -> Result<HealthSummary> {
    let issues = load_issues(linear_cache)
        .with_context(|| format!("load linear cache {}", linear_cache.display()))?;

    let mut summary = HealthSummary::default();
    summary.issues_total = issues.len();

    if issues.is_empty() {
        summary.grade = grade_band(0.0).to_string();
        write_output(&summary, output_summary)?;
        return Ok(summary);
    }

    let n = issues.len() as f64;

    // Dimension 1: hygiene — fraction with a valid Fibonacci estimate.
    let fib_count = issues
        .iter()
        .filter(|i| i.estimate.is_some_and(is_fibonacci))
        .count() as f64;
    summary.hygiene_score = round2((fib_count / n) * MAX_HYGIENE);

    // Dimension 2: structure — fraction with a non-empty state.
    let stated = issues.iter().filter(|i| i.has_state()).count() as f64;
    summary.structure_score = round2((stated / n) * MAX_STRUCTURE);

    // Dimension 3: data_quality — 15 minus penalties for QA-Failed rework
    // beyond the free allowance, floored at 0. Heuristic; see module docs.
    let qa_failed = issues.iter().filter(|i| i.is_qa_failed()).count();
    summary.qa_failed_count = qa_failed;
    let excess_fails = qa_failed.saturating_sub(DATA_QUALITY_FREE_FAILS);
    summary.data_quality_score = round2(
        (excess_fails as f64)
            .mul_add(-DATA_QUALITY_PENALTY_PER_FAIL, MAX_DATA_QUALITY)
            .max(0.0),
    );

    // Dimension 4: flow — penalize QA-lane congestion among open points.
    // open points = points on non-done issues; congested = those in a QA
    // lane. With no open points the board can't be congested → full flow.
    let mut open_points = 0.0;
    let mut qa_points = 0.0;
    for i in &issues {
        if i.is_done() {
            continue;
        }
        let pts = i.estimate.unwrap_or(0.0);
        open_points += pts;
        if i.is_qa_lane() {
            qa_points += pts;
        }
    }
    let congestion = if open_points > 0.0 {
        qa_points / open_points
    } else {
        0.0
    };
    summary.qa_congestion_fraction = round2(congestion);
    summary.flow_score = round2(MAX_FLOW * (1.0 - congestion));

    let total = summary.hygiene_score
        + summary.structure_score
        + summary.data_quality_score
        + summary.flow_score;
    summary.total_score = total.round() as u32;
    summary.grade = grade_band(total).to_string();

    write_output(&summary, output_summary)?;
    Ok(summary)
}

/// Map a 0-100 score to its grade band.
fn grade_band(score: f64) -> &'static str {
    if score >= GRADE_HEALTHY_MIN {
        "healthy"
    } else if score >= GRADE_OK_MIN {
        "ok"
    } else {
        "needs-work"
    }
}

/// Parse the permissive cache into normalized issues.
fn load_issues(path: &Path) -> Result<Vec<Issue>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse JSON {}", path.display()))?;
    let arr: &[serde_json::Value] = if let Some(a) = value.as_array() {
        a
    } else if let Some(a) = value.get("issues").and_then(serde_json::Value::as_array) {
        a
    } else {
        return Ok(Vec::new());
    };

    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        // An issue must at least have an identifier to count (mirrors the
        // PM audit's parse so the two stay consistent).
        if v.get("identifier")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            continue;
        }
        let estimate = v
            .get("estimate")
            .and_then(serde_json::Value::as_f64)
            .filter(|e| e.is_finite() && *e > 0.0);
        let (state_name, state_type) = v
            .get("state")
            .map(|s| {
                (
                    s.get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    s.get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_lowercase(),
                )
            })
            .unwrap_or_default();
        out.push(Issue {
            estimate,
            state_name,
            state_type,
        });
    }
    Ok(out)
}

fn write_output(summary: &HealthSummary, output_summary: &Path) -> Result<()> {
    if let Some(parent) = output_summary.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create metrics dir {}", parent.display()))?;
    }
    fs::write(output_summary, serde_json::to_string_pretty(summary)?)
        .with_context(|| format!("write {}", output_summary.display()))?;
    Ok(())
}

/// Is `e` exactly a Fibonacci estimate value?
fn is_fibonacci(e: f64) -> bool {
    FIBONACCI.iter().any(|&f| (e - f64::from(f)).abs() < 1e-6)
}

/// Round to two decimal places.
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn tmp(json: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn dimension_caps_sum_to_100() {
        assert!((MAX_HYGIENE + MAX_STRUCTURE + MAX_DATA_QUALITY + MAX_FLOW - 100.0).abs() < 1e-9);
    }

    #[test]
    fn clean_board_scores_high() {
        // All estimated (Fibonacci), all stated, no QA-Failed, and the only
        // open point is NOT in a QA lane → every dimension near/at cap.
        let cache = tmp(r#"{"issues":[
                {"identifier":"A-1","estimate":3,"state":{"name":"Completed","type":"completed"}},
                {"identifier":"A-2","estimate":5,"state":{"name":"Completed","type":"completed"}},
                {"identifier":"A-3","estimate":2,"state":{"name":"In Progress","type":"started"}}
            ]}"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_health_score(cache.path(), out.path()).unwrap();
        // hygiene 30 + structure 20 + data_quality 15 + flow 35 = 100.
        assert_eq!(s.hygiene_score, 30.0);
        assert_eq!(s.structure_score, 20.0);
        assert_eq!(s.data_quality_score, 15.0);
        assert_eq!(s.flow_score, 35.0);
        assert_eq!(s.total_score, 100);
        assert_eq!(s.grade, "healthy");
    }

    #[test]
    fn qa_failed_and_congestion_lower_the_score() {
        // Two QA-Failed (1 free → 1 penalized = -3 data_quality), and the
        // open points are heavily stuck in QA lanes → flow penalized.
        let cache = tmp(r#"{"issues":[
                {"identifier":"B-1","estimate":5,"state":{"name":"QA Failed","type":"started"}},
                {"identifier":"B-2","estimate":5,"state":{"name":"QA Failed","type":"started"}},
                {"identifier":"B-3","estimate":5,"state":{"name":"QA Testing","type":"started"}},
                {"identifier":"B-4","estimate":3,"state":{"name":"In Progress","type":"started"}}
            ]}"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_health_score(cache.path(), out.path()).unwrap();
        // data_quality: 15 - (2-1)*3 = 12.
        assert_eq!(s.data_quality_score, 12.0);
        assert_eq!(s.qa_failed_count, 2);
        // Open points = 5+5+5+3 = 18; QA-lane points = 5+5+5 = 15 → ~0.83.
        assert!(s.qa_congestion_fraction > 0.8);
        // flow well below its 35 cap.
        assert!(s.flow_score < 10.0, "flow was {}", s.flow_score);
        assert!(s.total_score < 85);
    }

    #[test]
    fn missing_estimates_drop_hygiene() {
        // Half estimated, half not → hygiene at half of its 30 cap.
        let cache = tmp(r#"[
                {"identifier":"C-1","estimate":3,"state":{"name":"Backlog","type":"backlog"}},
                {"identifier":"C-2","state":{"name":"Backlog","type":"backlog"}}
            ]"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_health_score(cache.path(), out.path()).unwrap();
        assert_eq!(s.hygiene_score, 15.0);
        // Both have a state → structure full.
        assert_eq!(s.structure_score, 20.0);
    }

    #[test]
    fn dimension_caps_are_respected() {
        // Even a pathological board can't exceed any per-dimension cap.
        let cache = tmp(
            r#"[{"identifier":"D-1","estimate":3,"state":{"name":"Completed","type":"completed"}}]"#,
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_health_score(cache.path(), out.path()).unwrap();
        assert!(s.hygiene_score <= MAX_HYGIENE);
        assert!(s.structure_score <= MAX_STRUCTURE);
        assert!(s.data_quality_score <= MAX_DATA_QUALITY);
        assert!(s.flow_score <= MAX_FLOW);
        assert!(s.total_score <= 100);
    }

    #[test]
    fn data_quality_floors_at_zero() {
        // Many QA-Failed issues → penalty would go negative; must floor at 0.
        let cache = tmp(r#"[
                {"identifier":"E-1","estimate":3,"state":{"name":"QA Failed","type":"started"}},
                {"identifier":"E-2","estimate":3,"state":{"name":"QA Failed","type":"started"}},
                {"identifier":"E-3","estimate":3,"state":{"name":"QA Failed","type":"started"}},
                {"identifier":"E-4","estimate":3,"state":{"name":"QA Failed","type":"started"}},
                {"identifier":"E-5","estimate":3,"state":{"name":"QA Failed","type":"started"}},
                {"identifier":"E-6","estimate":3,"state":{"name":"QA Failed","type":"started"}},
                {"identifier":"E-7","estimate":3,"state":{"name":"QA Failed","type":"started"}}
            ]"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_health_score(cache.path(), out.path()).unwrap();
        assert_eq!(s.data_quality_score, 0.0);
    }

    #[test]
    fn missing_cache_is_empty_not_error() {
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_health_score(Path::new("/nonexistent/cache.json"), out.path()).unwrap();
        assert_eq!(s.issues_total, 0);
        assert_eq!(s.total_score, 0);
        assert_eq!(s.grade, "needs-work");
    }
}
