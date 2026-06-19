//! Per-developer scorecards (the "who actually shipped" engine).
//!
//! Joins git-derived delivery stats with the Linear issue cache to score
//! each developer on throughput, first-pass QA, and consistency, then folds
//! those into a single 0-100 composite. It also runs an
//! **attribution-divergence** check that surfaces the merge-reassign bug
//! which hid Rene's output: high git delivery but near-zero Linear-assignee
//! completions means the credit landed on the wrong person.
//!
//! ## Inputs
//!
//! Two JSON files, both permissive about missing fields:
//!
//! 1. **Git stats** at `~/.claude/sentinel/dev-git-stats.json` — the module
//!    can't run git from a pure-application crate, so the caller (a cron)
//!    precomputes:
//!    ```json
//!    {"devs":[{"name":"Rene","commits":102,"active_days":17,
//!              "merged_prs":44,"delivered_tickets":["FPCRM-520", ...],
//!              "linear_assignee_completed":0}]}
//!    ```
//!    `linear_assignee_completed` is optional — when present it powers the
//!    attribution-divergence check.
//! 2. **Linear cache** at `~/.claude/sentinel/linear-assigned.json` — the
//!    same `{ "issues": [...] }`-or-bare-array file the rest of the suite
//!    reads. Used to resolve the current state of each delivered ticket so
//!    we can compute a first-pass QA rate.
//!
//! ## The composite score
//!
//! For each dev we compute three normalized sub-scores and weight them:
//!
//! * **throughput** ([`W_THROUGHPUT`]) — commits-per-active-day scaled
//!   against [`THROUGHPUT_TARGET_CPD`] and clamped to 1.0.
//! * **first-pass QA** ([`W_FIRST_PASS`]) — `clean / (clean + bounced)`
//!   across the dev's delivered tickets, where a ticket is "clean" if its
//!   current Linear state is Completed or a QA lane (and not "QA Failed"),
//!   and "bounced" if its state name is exactly "QA Failed". A dev with no
//!   resolvable tickets gets the neutral [`NEUTRAL_QA_RATE`].
//! * **consistency** ([`W_CONSISTENCY`]) — `active_days` scaled against
//!   [`CONSISTENCY_TARGET_DAYS`] and clamped; rewards sustained delivery
//!   over a single big burst.
//!
//! `score = 100 * (W_THROUGHPUT*tp + W_FIRST_PASS*qa + W_CONSISTENCY*cons)`.
//!
//! Output is written to `~/.claude/sentinel/metrics/dev-scorecard.json`
//! (summary) and `…-scorecard.jsonl` (one row per developer), idempotently.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

/// Commits-per-active-day that maps to a full throughput sub-score of 1.0.
/// Above this the score saturates — we don't reward commit-count gaming.
pub const THROUGHPUT_TARGET_CPD: f64 = 6.0;

/// Active days that map to a full consistency sub-score of 1.0.
pub const CONSISTENCY_TARGET_DAYS: f64 = 15.0;

/// `prs_per_week` display value is clamped here so a short-but-intense
/// window (e.g. 10 PRs in 1 active day) doesn't print an absurd rate.
pub const PRS_PER_WEEK_DISPLAY_CAP: f64 = 40.0;

/// QA rate assigned to a dev whose delivered tickets can't be resolved in
/// the cache (neutral — neither rewarded nor punished on missing data).
pub const NEUTRAL_QA_RATE: f64 = 0.85;

/// Minimum git-delivered ticket count before the attribution-divergence
/// check fires. Below this, a zero Linear-assignee count is unremarkable.
pub const ATTRIBUTION_MIN_DELIVERED: usize = 5;

/// Composite weights — must sum to 1.0 (asserted in tests).
pub const W_THROUGHPUT: f64 = 0.4;
pub const W_FIRST_PASS: f64 = 0.4;
pub const W_CONSISTENCY: f64 = 0.2;

/// Raw git-derived stats for one developer, parsed out of the git-stats JSON.
#[derive(Debug, Clone)]
struct DevGitStats {
    name: String,
    commits: u64,
    active_days: u64,
    merged_prs: u64,
    delivered_tickets: Vec<String>,
    /// Optional — count of tickets where this dev is the *current* Linear
    /// assignee on a Completed issue. Drives attribution divergence.
    linear_assignee_completed: Option<u64>,
}

/// One developer's computed scorecard, written as a JSONL row.
#[derive(Debug, Clone, Serialize)]
pub struct DevScore {
    pub name: String,
    pub commits: u64,
    pub active_days: u64,
    pub merged_prs: u64,
    pub commits_per_active_day: f64,
    /// Merged PRs per week, scaled from `active_days` and clamped to
    /// [`PRS_PER_WEEK_DISPLAY_CAP`].
    pub prs_per_week: f64,
    pub delivered_tickets: usize,
    /// Delivered tickets resolved to a "clean" current state.
    pub clean_tickets: usize,
    /// Delivered tickets currently in a "QA Failed" state.
    pub bounced_tickets: usize,
    /// `clean / (clean + bounced)`, or [`NEUTRAL_QA_RATE`] when neither.
    pub first_pass_qa_rate: f64,
    /// Composite 0-100 score (throughput + first-pass QA + consistency).
    pub score: f64,
    /// `true` when git shows real delivery (`>= ATTRIBUTION_MIN_DELIVERED`)
    /// but the dev's current Linear-assignee completions are ~0 — the
    /// merge-reassign bug that hides real contributors.
    pub attribution_divergence: bool,
}

/// The full scorecard summary written to `dev-scorecard.json`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ScorecardSummary {
    pub devs_total: usize,
    pub commits_total: u64,
    pub merged_prs_total: u64,
    /// Count of devs flagged with attribution divergence.
    pub attribution_divergences: usize,
    /// Highest composite score across all devs (0.0 when no devs).
    pub top_score: f64,
    /// Name of the top-scoring dev (empty when no devs).
    pub top_dev: String,
    /// Per-dev rows, score-descending.
    pub devs: Vec<DevScore>,
}

/// Run the scorecard over `git_stats` + `linear_cache`, write
/// `output_summary` (JSON) and its `.jsonl` sibling (per-dev rows), return
/// the summary.
pub fn scan_dev_scorecard(
    git_stats: &Path,
    linear_cache: &Path,
    output_summary: &Path,
) -> Result<ScorecardSummary> {
    let devs = load_git_stats(git_stats)
        .with_context(|| format!("load git stats {}", git_stats.display()))?;
    // ticket-identifier -> current state-name (lowercased for matching).
    let states = load_ticket_states(linear_cache)
        .with_context(|| format!("load linear cache {}", linear_cache.display()))?;

    let mut summary = ScorecardSummary::default();
    summary.devs_total = devs.len();

    for d in &devs {
        summary.commits_total += d.commits;
        summary.merged_prs_total += d.merged_prs;

        let cpd = if d.active_days == 0 {
            0.0
        } else {
            d.commits as f64 / d.active_days as f64
        };
        let prs_per_week = if d.active_days == 0 {
            0.0
        } else {
            (d.merged_prs as f64 / d.active_days as f64 * 7.0).min(PRS_PER_WEEK_DISPLAY_CAP)
        };

        // First-pass QA: classify each delivered ticket by its current state.
        let mut clean = 0usize;
        let mut bounced = 0usize;
        for t in &d.delivered_tickets {
            match states.get(&t.to_uppercase()) {
                Some(state) if is_qa_failed(state) => bounced += 1,
                Some(state) if is_clean(state) => clean += 1,
                _ => {}
            }
        }
        let qa_rate = if clean + bounced == 0 {
            NEUTRAL_QA_RATE
        } else {
            clean as f64 / (clean + bounced) as f64
        };

        // Sub-scores, each clamped to [0, 1].
        let tp = (cpd / THROUGHPUT_TARGET_CPD).clamp(0.0, 1.0);
        let cons = (d.active_days as f64 / CONSISTENCY_TARGET_DAYS).clamp(0.0, 1.0);
        let weighted =
            W_CONSISTENCY.mul_add(cons, W_THROUGHPUT.mul_add(tp, W_FIRST_PASS * qa_rate));
        let score = 100.0 * weighted;

        // Attribution divergence: real git delivery but ~0 Linear-assignee
        // completions. Only fires when the optional count is supplied.
        let divergence = d
            .linear_assignee_completed
            .is_some_and(|c| d.delivered_tickets.len() >= ATTRIBUTION_MIN_DELIVERED && c == 0);
        if divergence {
            summary.attribution_divergences += 1;
        }

        summary.devs.push(DevScore {
            name: d.name.clone(),
            commits: d.commits,
            active_days: d.active_days,
            merged_prs: d.merged_prs,
            commits_per_active_day: round2(cpd),
            prs_per_week: round2(prs_per_week),
            delivered_tickets: d.delivered_tickets.len(),
            clean_tickets: clean,
            bounced_tickets: bounced,
            first_pass_qa_rate: round2(qa_rate),
            score: round1(score),
            attribution_divergence: divergence,
        });
    }

    // Rank score-descending; record the leader.
    summary.devs.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(top) = summary.devs.first() {
        summary.top_score = top.score;
        summary.top_dev = top.name.clone();
    }

    write_outputs(&summary, output_summary)?;
    Ok(summary)
}

/// Parse the git-stats JSON into normalized per-dev rows. A missing file is
/// an empty list (not an error) — mirrors the cache-missing behavior of the
/// PM audit so the CLI can print a helpful message instead of crashing.
fn load_git_stats(path: &Path) -> Result<Vec<DevGitStats>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse JSON {}", path.display()))?;
    let arr: &[serde_json::Value] = if let Some(a) = value.as_array() {
        a
    } else if let Some(a) = value.get("devs").and_then(serde_json::Value::as_array) {
        a
    } else {
        return Ok(Vec::new());
    };

    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let Some(name) = v
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let delivered_tickets = v
            .get("delivered_tickets")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        out.push(DevGitStats {
            name,
            commits: v
                .get("commits")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            active_days: v
                .get("active_days")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            merged_prs: v
                .get("merged_prs")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            delivered_tickets,
            linear_assignee_completed: v
                .get("linear_assignee_completed")
                .and_then(serde_json::Value::as_u64),
        });
    }
    Ok(out)
}

/// Read the Linear cache into an `IDENTIFIER -> state-name` map (identifier
/// upper-cased for case-insensitive lookup). Missing cache = empty map.
fn load_ticket_states(path: &Path) -> Result<HashMap<String, String>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse JSON {}", path.display()))?;
    let arr: &[serde_json::Value] = if let Some(a) = value.as_array() {
        a
    } else if let Some(a) = value.get("issues").and_then(serde_json::Value::as_array) {
        a
    } else {
        return Ok(HashMap::new());
    };

    let mut out = HashMap::with_capacity(arr.len());
    for v in arr {
        let Some(id) = v.get("identifier").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let state_name = v
            .get("state")
            .and_then(|s| s.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        out.insert(id.to_uppercase(), state_name);
    }
    Ok(out)
}

/// Is this state name exactly the "QA Failed" bounce lane?
fn is_qa_failed(state_name: &str) -> bool {
    state_name.eq_ignore_ascii_case("QA Failed")
}

/// Is this a "clean" delivery state — Completed, or any QA lane that isn't
/// the failed bounce? We approximate "didn't bounce" with current state
/// because the cache doesn't carry full transition history.
fn is_clean(state_name: &str) -> bool {
    if is_qa_failed(state_name) {
        return false;
    }
    let lower = state_name.to_lowercase();
    lower.contains("complet") || lower.contains("done") || lower.contains("qa")
}

fn write_outputs(summary: &ScorecardSummary, output_summary: &Path) -> Result<()> {
    if let Some(parent) = output_summary.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create metrics dir {}", parent.display()))?;
    }
    let jsonl = output_summary.with_extension("jsonl");
    let mut f = File::create(&jsonl).with_context(|| format!("create {}", jsonl.display()))?;
    for row in &summary.devs {
        f.write_all(serde_json::to_string(row)?.as_bytes())?;
        f.write_all(b"\n")?;
    }
    fs::write(output_summary, serde_json::to_string_pretty(summary)?)
        .with_context(|| format!("write {}", output_summary.display()))?;
    Ok(())
}

/// Round to one decimal place (scores).
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Round to two decimal places (rates).
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(json: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn weights_sum_to_one() {
        assert!((W_THROUGHPUT + W_FIRST_PASS + W_CONSISTENCY - 1.0).abs() < 1e-9);
    }

    #[test]
    fn high_throughput_and_qa_scores_well() {
        // 102 commits over 17 active days = 6 cpd (saturates throughput),
        // 17 active days (saturates consistency). 7 of 8 delivered tickets
        // clean, 1 bounced → 87.5% first-pass QA.
        let git = tmp(
            r#"{"devs":[{"name":"Rene","commits":102,"active_days":17,"merged_prs":44,
                "delivered_tickets":["T-1","T-2","T-3","T-4","T-5","T-6","T-7","T-8"]}]}"#,
        );
        let cache = tmp(r#"{"issues":[
                {"identifier":"T-1","state":{"name":"Completed","type":"completed"}},
                {"identifier":"T-2","state":{"name":"Completed","type":"completed"}},
                {"identifier":"T-3","state":{"name":"Completed","type":"completed"}},
                {"identifier":"T-4","state":{"name":"Completed","type":"completed"}},
                {"identifier":"T-5","state":{"name":"Completed","type":"completed"}},
                {"identifier":"T-6","state":{"name":"Completed","type":"completed"}},
                {"identifier":"T-7","state":{"name":"QA Testing","type":"started"}},
                {"identifier":"T-8","state":{"name":"QA Failed","type":"started"}}
            ]}"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_dev_scorecard(git.path(), cache.path(), out.path()).unwrap();
        assert_eq!(s.devs_total, 1);
        let rene = &s.devs[0];
        assert_eq!(rene.clean_tickets, 7);
        assert_eq!(rene.bounced_tickets, 1);
        assert!((rene.first_pass_qa_rate - 0.88).abs() < 0.01);
        // All three sub-scores near max → composite high.
        assert!(rene.score > 90.0, "score was {}", rene.score);
        assert!(!rene.attribution_divergence);
        assert_eq!(s.top_dev, "Rene");
    }

    #[test]
    fn flags_attribution_divergence() {
        // Git shows 6 delivered tickets but 0 current Linear-assignee
        // completions → the merge-reassign bug that hid Rene.
        let git = tmp(
            r#"{"devs":[{"name":"Rene","commits":60,"active_days":12,"merged_prs":20,
                "delivered_tickets":["T-1","T-2","T-3","T-4","T-5","T-6"],
                "linear_assignee_completed":0}]}"#,
        );
        let cache = tmp(r#"{"issues":[]}"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_dev_scorecard(git.path(), cache.path(), out.path()).unwrap();
        assert!(s.devs[0].attribution_divergence);
        assert_eq!(s.attribution_divergences, 1);
    }

    #[test]
    fn no_divergence_when_assignee_count_absent() {
        // Without the optional linear_assignee_completed field the check
        // must NOT fire — we don't punish on missing data.
        let git = tmp(
            r#"{"devs":[{"name":"Sam","commits":30,"active_days":6,"merged_prs":10,
                "delivered_tickets":["T-1","T-2","T-3","T-4","T-5","T-6"]}]}"#,
        );
        let cache = tmp(r#"{"issues":[]}"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_dev_scorecard(git.path(), cache.path(), out.path()).unwrap();
        assert!(!s.devs[0].attribution_divergence);
        assert_eq!(s.attribution_divergences, 0);
    }

    #[test]
    fn low_throughput_scores_below_high_throughput() {
        let git = tmp(r#"{"devs":[
                {"name":"Fast","commits":90,"active_days":15,"merged_prs":30,
                 "delivered_tickets":["T-1"]},
                {"name":"Slow","commits":4,"active_days":2,"merged_prs":1,
                 "delivered_tickets":["T-1"]}
            ]}"#);
        let cache = tmp(
            r#"{"issues":[{"identifier":"T-1","state":{"name":"Completed","type":"completed"}}]}"#,
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_dev_scorecard(git.path(), cache.path(), out.path()).unwrap();
        // Ranked descending → Fast first.
        assert_eq!(s.devs[0].name, "Fast");
        assert!(s.devs[0].score > s.devs[1].score);
    }

    #[test]
    fn missing_git_stats_is_empty_not_error() {
        let cache = tmp(r#"{"issues":[]}"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_dev_scorecard(
            Path::new("/nonexistent/git-stats.json"),
            cache.path(),
            out.path(),
        )
        .unwrap();
        assert_eq!(s.devs_total, 0);
        assert_eq!(s.top_dev, "");
    }
}
