//! Linear PM-enforcement audit (the "good PM is good software" engine).
//!
//! Runs the four PM-discipline analyses proven by hand against the
//! Firefly Pro Beta initiative, plus estimate hygiene, over a Linear
//! issue cache. It is the offline/report half of the enforcement system;
//! the [`crate::hooks::linear_pm_gate`] `PreToolUse` gate is the live half
//! that hard-blocks the worst violations.
//!
//! ## Input
//!
//! A Linear issue cache JSON at `~/.claude/sentinel/linear-assigned.json`
//! (the same file [`crate::cost_per_point`] reads). The shape is
//! permissive — either a top-level array of issue objects or
//! `{ "issues": [...] }`. Each issue may carry:
//!
//! * `identifier` (e.g. `"FPCRM-606"`) — required to be audited
//! * `estimate` — story points (Fibonacci 1/2/3/5/8/13 expected)
//! * `state` — `{ "name": "...", "type": "..." }` (Linear state)
//! * `startedAt` / `completedAt` — ISO timestamps for cycle-time
//! * `title` — for human-readable flags
//!
//! Issues missing `state` are still counted for hygiene (missing/non-Fib
//! estimate) but cannot be bucketed by workflow state.
//!
//! ## The five checks
//!
//! 1. **Estimate hygiene** — every issue must carry a Fibonacci estimate.
//!    Missing or off-scale (e.g. `4`, `6`, `7`) estimates are flagged.
//! 2. **Oversized tickets** — issues with `estimate >= OVERSIZED_POINTS`
//!    that are still `started`/`backlog`/`unstarted` (i.e. not yet done)
//!    are decomposition candidates: a single opaque 8-pt block hides risk.
//! 3. **QA-Failed analysis** — issues currently in a `QA Failed` state are
//!    "built but bouncing": the clearest near-term threat to a date.
//! 4. **Velocity burndown** — given a measured points-per-week velocity
//!    and a target date, can the remaining (not-done) points realistically
//!    close? Reports the implied weeks-needed vs weeks-available.
//! 5. **Estimate-vs-actual calibration** — for completed issues with both
//!    `startedAt` and `completedAt`, compare story points to actual
//!    calendar cycle-time. Per-estimate-bucket median days surface
//!    systematically over-/under-sized buckets (a non-monotonic curve =
//!    sizing has drifted; mirrors `cost_per_point`'s drift alarm).
//!
//! Output is written to `~/.claude/sentinel/metrics/linear-pm-audit.json`
//! (summary) and `…-pm-audit.jsonl` (one row per flagged issue), idempotently.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

/// Valid Linear Fibonacci estimate values. Anything else is "off-scale".
pub const FIBONACCI: &[u8] = &[1, 2, 3, 5, 8, 13, 21];

/// Estimate at or above which an unfinished ticket is a decomposition
/// candidate. 8 is the conventional "break this down" line.
pub const OVERSIZED_POINTS: f64 = 8.0;

/// State `type` values Linear uses (lowercased) that count as "done".
const DONE_TYPES: &[&str] = &["completed"];
/// State `type` values that count as "still open work" (not done, not canceled).
const OPEN_TYPES: &[&str] = &["backlog", "unstarted", "started", "triage"];

/// A normalized issue parsed out of the cache.
#[derive(Debug, Clone)]
struct Issue {
    identifier: String,
    title: String,
    estimate: Option<f64>,
    state_name: String,
    state_type: String,
    started_at: Option<String>,
    completed_at: Option<String>,
    /// Human-readable blocked reason, or `None` if startable. Mirrors the
    /// [`crate::hooks::linear_pm_gate`] blocked rule (open blocked-by relation,
    /// Blocked state, or blocked/blocker label) so the report and the gate
    /// agree on what "blocked" means.
    blocked_reason: Option<String>,
    /// `true` when the issue's project uses milestones but the issue has none —
    /// untracked work. Mirrors the gate's milestone rule.
    needs_milestone: bool,
}

impl Issue {
    fn is_done(&self) -> bool {
        DONE_TYPES.contains(&self.state_type.as_str())
    }
    fn is_open(&self) -> bool {
        OPEN_TYPES.contains(&self.state_type.as_str())
    }
    fn is_qa_failed(&self) -> bool {
        self.state_name.eq_ignore_ascii_case("QA Failed")
    }
}

/// One flagged issue, written as a JSONL row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PmFlag {
    pub identifier: String,
    pub title: String,
    /// One of: `missing-estimate`, `non-fibonacci`, `oversized`, `qa-failed`.
    pub category: String,
    pub estimate: Option<f64>,
    pub state: String,
    pub detail: String,
}

/// Per-estimate-bucket calibration: how long completed tickets of this
/// size actually took, in calendar days.
#[derive(Debug, Clone, Serialize, Default)]
pub struct BucketCalibration {
    pub n: usize,
    pub median_days: f64,
    /// `true` when the sample is too thin or too coarse to trust — fewer
    /// than [`CALIBRATION_MIN_N`] completed tickets, or a degenerate spread
    /// (all samples ≤ 1 day, which usually means `startedAt` and
    /// `completedAt` were batch-stamped rather than reflecting real work
    /// duration). Consumers should present these as indicative, not as a
    /// sizing verdict.
    pub low_confidence: bool,
}

/// Minimum completed-ticket sample per bucket for the calibration figure to
/// be considered trustworthy.
pub const CALIBRATION_MIN_N: usize = 3;

/// Velocity-vs-remaining burndown projection.
#[derive(Debug, Clone, Serialize)]
pub struct Burndown {
    pub remaining_points: f64,
    pub velocity_points_per_week: f64,
    pub weeks_needed: f64,
    pub weeks_available: f64,
    /// `true` when `weeks_needed <= weeks_available` (date is reachable).
    pub on_track: bool,
}

/// The full audit summary written to `linear-pm-audit.json`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PmAuditSummary {
    pub issues_total: usize,
    pub issues_done: usize,
    pub issues_open: usize,
    pub points_total: f64,
    pub points_done: f64,
    pub points_remaining: f64,
    // Check 1: hygiene
    pub missing_estimate: usize,
    pub non_fibonacci: usize,
    // Check 2: oversized
    pub oversized_open: usize,
    // Blocked: open tickets that are blocked (a relation, state, or label).
    pub blocked_open: usize,
    // Untracked: open tickets with no milestone, where the project uses them.
    pub no_milestone_open: usize,
    // Check 3: QA-failed
    pub qa_failed: usize,
    pub qa_failed_points: f64,
    // Check 4: burndown (None if no velocity/target supplied)
    pub burndown: Option<Burndown>,
    // Check 5: calibration by estimate bucket
    pub calibration: BTreeMap<u8, BucketCalibration>,
    /// Convenience: total flagged issues across hygiene+oversized+qa-failed.
    pub total_flags: usize,
    /// `true` when any hard-gate-worthy violation exists (oversized-open or
    /// missing-estimate on an open ticket). Drives the cron's alarm wording.
    pub hard_violations: bool,
}

/// Scanner output used by graph-backed callers that need both the board-level
/// summary and the exact per-ticket PM discipline flags from the same pass.
#[derive(Debug, Clone, Serialize)]
pub struct PmAuditReport {
    pub summary: PmAuditSummary,
    pub flags: Vec<PmFlag>,
}

/// Optional burndown inputs. When `velocity_pts_per_week` and
/// `weeks_available` are both `Some`, check 4 runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct BurndownInputs {
    pub velocity_pts_per_week: Option<f64>,
    pub weeks_available: Option<f64>,
}

/// Run the full PM audit over `linear_cache`, write `output_summary`
/// (JSON) and its `.jsonl` sibling (flag rows), return the summary.
pub fn scan_pm_audit(
    linear_cache: &Path,
    output_summary: &Path,
    burndown: BurndownInputs,
) -> Result<PmAuditSummary> {
    Ok(scan_pm_audit_report(linear_cache, output_summary, burndown)?.summary)
}

/// Run the full PM audit and return the exact flag rows alongside the summary.
pub fn scan_pm_audit_report(
    linear_cache: &Path,
    output_summary: &Path,
    burndown: BurndownInputs,
) -> Result<PmAuditReport> {
    let issues = load_issues(linear_cache)
        .with_context(|| format!("load linear cache {}", linear_cache.display()))?;

    let mut flags: Vec<PmFlag> = Vec::new();
    let mut summary = PmAuditSummary::default();
    summary.issues_total = issues.len();

    // Cycle-time samples per estimate bucket (check 5).
    let mut bucket_days: BTreeMap<u8, Vec<f64>> = BTreeMap::new();

    for iss in &issues {
        if iss.is_done() {
            summary.issues_done += 1;
        } else if iss.is_open() {
            summary.issues_open += 1;
        }
        let pts = iss.estimate.unwrap_or(0.0);
        summary.points_total += pts;
        if iss.is_done() {
            summary.points_done += pts;
        } else if iss.is_open() {
            summary.points_remaining += pts;
        }

        // Check 1: estimate hygiene.
        match iss.estimate {
            None => {
                summary.missing_estimate += 1;
                flags.push(PmFlag {
                    identifier: iss.identifier.clone(),
                    title: iss.title.clone(),
                    category: "missing-estimate".into(),
                    estimate: None,
                    state: iss.state_name.clone(),
                    detail: "no story-point estimate".into(),
                });
            }
            Some(e) if !is_fibonacci(e) => {
                summary.non_fibonacci += 1;
                flags.push(PmFlag {
                    identifier: iss.identifier.clone(),
                    title: iss.title.clone(),
                    category: "non-fibonacci".into(),
                    estimate: Some(e),
                    state: iss.state_name.clone(),
                    detail: format!("estimate {e} is off the Fibonacci scale"),
                });
            }
            _ => {}
        }

        // Check 2: oversized & still open → decomposition candidate.
        if iss.is_open() && pts >= OVERSIZED_POINTS {
            summary.oversized_open += 1;
            flags.push(PmFlag {
                identifier: iss.identifier.clone(),
                title: iss.title.clone(),
                category: "oversized".into(),
                estimate: Some(pts),
                state: iss.state_name.clone(),
                detail: format!("{pts}-pt ticket still open — decompose into sub-issues"),
            });
        }

        // Check 2b: blocked & still open → must not be picked up.
        if iss.is_open() {
            if let Some(reason) = &iss.blocked_reason {
                summary.blocked_open += 1;
                flags.push(PmFlag {
                    identifier: iss.identifier.clone(),
                    title: iss.title.clone(),
                    category: "blocked".into(),
                    estimate: iss.estimate,
                    state: iss.state_name.clone(),
                    detail: format!("{reason} — do not start until the blocker clears"),
                });
            }
        }

        // Check 2c: untracked work — open ticket with no milestone, where the
        // project uses them.
        if iss.is_open() && iss.needs_milestone {
            summary.no_milestone_open += 1;
            flags.push(PmFlag {
                identifier: iss.identifier.clone(),
                title: iss.title.clone(),
                category: "no-milestone".into(),
                estimate: iss.estimate,
                state: iss.state_name.clone(),
                detail: "no milestone, but the project uses them — assign one before starting"
                    .into(),
            });
        }

        // Check 3: QA-failed.
        if iss.is_qa_failed() {
            summary.qa_failed += 1;
            summary.qa_failed_points += pts;
            flags.push(PmFlag {
                identifier: iss.identifier.clone(),
                title: iss.title.clone(),
                category: "qa-failed".into(),
                estimate: iss.estimate,
                state: iss.state_name.clone(),
                detail: "bounced QA — built but failing, the near-term risk".into(),
            });
        }

        // Check 5: calibration sample (completed with both timestamps).
        if iss.is_done() {
            if let (Some(s), Some(c), Some(e)) = (&iss.started_at, &iss.completed_at, iss.estimate)
            {
                if let Some(days) = days_between(s, c) {
                    if days >= 0.0 {
                        bucket_days.entry(nearest_fib(e)).or_default().push(days);
                    }
                }
            }
        }
    }

    // Finalize calibration: median days per bucket. Flag low-confidence
    // buckets (thin sample, or a degenerate ≤1-day spread that signals
    // batch-stamped timestamps rather than real work duration).
    for (bucket, mut days) in bucket_days {
        days.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = days.len();
        let med = median(&days);
        // A ≤1-day median signals batch-stamped timestamps (tickets marked
        // started+completed the same day) rather than real measured duration —
        // not a trustworthy sizing signal regardless of sample size.
        let degenerate = med <= 1.0;
        summary.calibration.insert(
            bucket,
            BucketCalibration {
                n,
                median_days: med,
                low_confidence: n < CALIBRATION_MIN_N || degenerate,
            },
        );
    }

    // Check 4: burndown.
    if let (Some(v), Some(wa)) = (burndown.velocity_pts_per_week, burndown.weeks_available) {
        if v > 0.0 {
            let weeks_needed = summary.points_remaining / v;
            summary.burndown = Some(Burndown {
                remaining_points: summary.points_remaining,
                velocity_points_per_week: v,
                weeks_needed,
                weeks_available: wa,
                on_track: weeks_needed <= wa,
            });
        }
    }

    summary.total_flags = flags.len();
    summary.hard_violations = summary.oversized_open > 0
        || summary.blocked_open > 0
        || summary.no_milestone_open > 0
        || flags.iter().any(|f| f.category == "missing-estimate");

    write_outputs(&flags, &summary, output_summary)?;
    Ok(PmAuditReport { summary, flags })
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
        let Some(identifier) = v
            .get("identifier")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
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
            identifier,
            title: v
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            estimate,
            state_name,
            state_type,
            started_at: v
                .get("startedAt")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            completed_at: v
                .get("completedAt")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            blocked_reason: blocked_reason(v),
            needs_milestone: {
                let uses = v
                    .get("projectHasMilestones")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let has = v
                    .get("projectMilestone")
                    .or_else(|| v.get("milestone"))
                    .map(|m| !m.is_null())
                    .unwrap_or(false);
                uses && !has
            },
        });
    }
    Ok(out)
}

/// Human-readable reason an issue is blocked, or `None` if startable. Checks
/// the same three signals as the gate: an open `blockedBy` relation, a Blocked
/// workflow state, or a blocked/blocker label. Permissive on shape.
fn blocked_reason(v: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    // 1. Open blocked-by relation (a related issue not completed/canceled).
    if let Some(arr) = v.get("blockedBy").and_then(Value::as_array) {
        for rel in arr {
            let ty = rel
                .get("state")
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            if ty != "completed" && ty != "canceled" {
                let id = rel
                    .get("identifier")
                    .and_then(Value::as_str)
                    .unwrap_or("an open issue");
                return Some(format!("blocked by {id}"));
            }
        }
    }
    // 2. Blocked workflow state — bare "Blocked" or Firefly "QA Blocked".
    if let Some(state) = v.get("state") {
        let name = state.get("name").and_then(Value::as_str).unwrap_or("");
        let ty = state.get("type").and_then(Value::as_str).unwrap_or("");
        if name.to_lowercase().contains("blocked") || ty.eq_ignore_ascii_case("blocked") {
            return Some(format!("{name} state"));
        }
    }
    // 3. Blocked / blocker label.
    if let Some(arr) = v.get("labels").and_then(Value::as_array) {
        for l in arr {
            let label = l
                .as_str()
                .or_else(|| l.get("name").and_then(Value::as_str))
                .unwrap_or("")
                .to_lowercase();
            if label == "blocked" || label == "blocker" {
                return Some("'blocked' label".into());
            }
        }
    }
    None
}

fn write_outputs(flags: &[PmFlag], summary: &PmAuditSummary, output_summary: &Path) -> Result<()> {
    if let Some(parent) = output_summary.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create metrics dir {}", parent.display()))?;
    }
    let jsonl = output_summary.with_extension("jsonl");
    let mut f = File::create(&jsonl).with_context(|| format!("create {}", jsonl.display()))?;
    for flag in flags {
        f.write_all(serde_json::to_string(flag)?.as_bytes())?;
        f.write_all(b"\n")?;
    }
    fs::write(output_summary, serde_json::to_string_pretty(summary)?)
        .with_context(|| format!("write {}", output_summary.display()))?;
    Ok(())
}

/// Is `e` exactly a Fibonacci estimate value?
fn is_fibonacci(e: f64) -> bool {
    // Estimates are integers in practice; compare with a small epsilon.
    FIBONACCI.iter().any(|&f| (e - f64::from(f)).abs() < 1e-6)
}

/// Round `e` to the nearest Fibonacci bucket (ties round up).
fn nearest_fib(e: f64) -> u8 {
    let mut best = FIBONACCI[0];
    let mut best_d = f64::MAX;
    for &f in FIBONACCI {
        let d = (e - f64::from(f)).abs();
        if d < best_d || (d - best_d).abs() < 1e-9 {
            best = f;
            best_d = d;
        }
    }
    best
}

/// Calendar days between two ISO-8601 timestamps (`completed - started`).
/// Returns `None` if either fails to parse. Parses the leading
/// `YYYY-MM-DD` (sufficient for day-granularity calibration) so we avoid a
/// chrono dependency if the crate doesn't already pull one.
fn days_between(start: &str, end: &str) -> Option<f64> {
    let s = parse_ymd_to_days(start)?;
    let e = parse_ymd_to_days(end)?;
    Some(e - s)
}

/// Convert the `YYYY-MM-DD` prefix of an ISO timestamp into a day count
/// since a fixed epoch (proleptic-ish; good enough for differences within
/// a few years, which is all calibration needs).
fn parse_ymd_to_days(ts: &str) -> Option<f64> {
    let date = ts.get(0..10)?;
    let mut it = date.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    #[allow(clippy::cast_precision_loss)]
    Some(days as f64)
}

/// Median of a pre-sorted slice.
fn median(sorted: &[f64]) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        f64::midpoint(sorted[n / 2 - 1], sorted[n / 2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(json: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn flags_missing_and_non_fibonacci_estimates() {
        let c = cache(
            r#"{"issues":[
                {"identifier":"A-1","estimate":3,"state":{"name":"Todo","type":"backlog"}},
                {"identifier":"A-2","state":{"name":"Todo","type":"backlog"}},
                {"identifier":"A-3","estimate":4,"state":{"name":"Todo","type":"backlog"}}
            ]}"#,
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_pm_audit(c.path(), out.path(), BurndownInputs::default()).unwrap();
        assert_eq!(s.issues_total, 3);
        assert_eq!(s.missing_estimate, 1);
        assert_eq!(s.non_fibonacci, 1);
    }

    #[test]
    fn flags_oversized_open_not_done() {
        let c = cache(
            r#"[
                {"identifier":"B-1","estimate":8,"state":{"name":"Backlog","type":"backlog"}},
                {"identifier":"B-2","estimate":8,"state":{"name":"Done","type":"completed"}}
            ]"#,
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_pm_audit(c.path(), out.path(), BurndownInputs::default()).unwrap();
        // Only the open 8-pt ticket is an oversized flag; the completed one isn't.
        assert_eq!(s.oversized_open, 1);
        assert!(s.hard_violations);
    }

    #[test]
    fn counts_qa_failed() {
        let c = cache(
            r#"[
                {"identifier":"C-1","estimate":3,"state":{"name":"QA Failed","type":"started"}},
                {"identifier":"C-2","estimate":5,"state":{"name":"QA Failed","type":"started"}}
            ]"#,
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_pm_audit(c.path(), out.path(), BurndownInputs::default()).unwrap();
        assert_eq!(s.qa_failed, 2);
        assert!((s.qa_failed_points - 8.0).abs() < 1e-9);
    }

    #[test]
    fn flags_blocked_open_tickets() {
        let c = cache(
            r#"[
                {"identifier":"K-1","estimate":3,"state":{"name":"Todo","type":"backlog"},
                 "blockedBy":[{"identifier":"K-9","state":{"type":"started"}}]},
                {"identifier":"K-2","estimate":2,"state":{"name":"Blocked","type":"started"}},
                {"identifier":"K-3","estimate":2,"state":{"name":"Todo","type":"backlog"},"labels":["blocker"]},
                {"identifier":"K-4","estimate":2,"state":{"name":"Todo","type":"backlog"},
                 "blockedBy":[{"identifier":"K-8","state":{"type":"completed"}}]}
            ]"#,
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_pm_audit(c.path(), out.path(), BurndownInputs::default()).unwrap();
        // K-1 (open relation), K-2 (Blocked state), K-3 (label) = 3 blocked;
        // K-4's blocker is completed → not blocked.
        assert_eq!(s.blocked_open, 3);
        assert!(s.hard_violations);
    }

    #[test]
    fn flags_open_tickets_missing_milestone() {
        let c = cache(
            r#"[
                {"identifier":"N-1","estimate":3,"state":{"name":"Todo","type":"backlog"},
                 "projectHasMilestones":true,"projectMilestone":null},
                {"identifier":"N-2","estimate":3,"state":{"name":"Todo","type":"backlog"},
                 "projectHasMilestones":true,"projectMilestone":{"id":"x","name":"M1"}},
                {"identifier":"N-3","estimate":3,"state":{"name":"Todo","type":"backlog"},
                 "projectHasMilestones":false,"projectMilestone":null}
            ]"#,
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_pm_audit(c.path(), out.path(), BurndownInputs::default()).unwrap();
        // N-1 missing (project uses them); N-2 has one; N-3 project exempt.
        assert_eq!(s.no_milestone_open, 1);
        assert!(s.hard_violations);
    }

    #[test]
    fn burndown_on_track_and_behind() {
        let c = cache(
            r#"[
                {"identifier":"D-1","estimate":8,"state":{"name":"Backlog","type":"backlog"}},
                {"identifier":"D-2","estimate":5,"state":{"name":"Backlog","type":"backlog"}}
            ]"#,
        );
        // 13 remaining pts, 10/wk, 2 weeks available → needs 1.3 wk → on track.
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_pm_audit(
            c.path(),
            out.path(),
            BurndownInputs {
                velocity_pts_per_week: Some(10.0),
                weeks_available: Some(2.0),
            },
        )
        .unwrap();
        let b = s.burndown.unwrap();
        assert!(b.on_track);
        assert!((b.weeks_needed - 1.3).abs() < 0.01);

        // Same scope, slower velocity, less time → behind.
        let out2 = tempfile::NamedTempFile::new().unwrap();
        let s2 = scan_pm_audit(
            c.path(),
            out2.path(),
            BurndownInputs {
                velocity_pts_per_week: Some(3.0),
                weeks_available: Some(2.0),
            },
        )
        .unwrap();
        assert!(!s2.burndown.unwrap().on_track);
    }

    #[test]
    fn calibration_buckets_completed_cycle_time() {
        let c = cache(
            r#"[
                {"identifier":"E-1","estimate":3,"state":{"name":"Done","type":"completed"},
                 "startedAt":"2026-06-01T00:00:00Z","completedAt":"2026-06-05T00:00:00Z"},
                {"identifier":"E-2","estimate":3,"state":{"name":"Done","type":"completed"},
                 "startedAt":"2026-06-01T00:00:00Z","completedAt":"2026-06-03T00:00:00Z"}
            ]"#,
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_pm_audit(c.path(), out.path(), BurndownInputs::default()).unwrap();
        let b3 = s.calibration.get(&3).unwrap();
        assert_eq!(b3.n, 2);
        // median of {2,4} days = 3.0
        assert!((b3.median_days - 3.0).abs() < 1e-9);
        // n=2 (< CALIBRATION_MIN_N) → low-confidence
        assert!(b3.low_confidence);
    }

    #[test]
    fn missing_cache_is_empty_not_error() {
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_pm_audit(
            Path::new("/nonexistent/cache.json"),
            out.path(),
            BurndownInputs::default(),
        )
        .unwrap();
        assert_eq!(s.issues_total, 0);
    }
}
