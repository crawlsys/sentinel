//! PR review thoroughness metrics (SEN-18).
//!
//! Walks merged PRs across the firefly-pro org + sentinel via `gh` CLI,
//! extracts review depth (comments, time-to-first-review p50/p90),
//! counts Codex/CodeRabbit findings, and surfaces the
//! human-in-the-loop %.
//!
//! Output:
//! - `~/.claude/sentinel/metrics/pr-review.jsonl` (one row per PR, full
//!   overwrite each scan)
//! - `~/.claude/sentinel/metrics/pr-review-summary.json` (aggregate)
//!
//! All `gh` JSON parsing is liberal: missing fields degrade to default
//! values rather than fail the whole scan, since one stale PR with an
//! odd shape shouldn't take down the metric pipeline.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Default repos walked when the caller doesn't supply an override.
pub const DEFAULT_REPOS: &[&str] = &[
    "firefly-pro-gh/firefly-pro-crm",
    "firefly-pro-gh/firefly-pro-web-app",
    "firefly-pro-gh/firefly-pro-api-rust",
    "firefly-pro-gh/firefly-pro-routing",
    "firefly-pro-gh/firefly-pro-routing-engine",
    "firefly-pro-gh/firefly-pro-routing-api",
    "firefly-pro-gh/firefly-pro-marketing",
    "garysomerhalder/sentinel",
];

/// Bot logins recognised as automated reviewers — comments authored by
/// these are excluded from the human-review count.
const BOT_LOGINS: &[&str] = &[
    "coderabbitai",
    "coderabbitai[bot]",
    "github-actions",
    "github-actions[bot]",
    "linear",
    "linear[bot]",
    "greptile-apps",
    "greptile-apps[bot]",
    "openai-codex",
    "openai-codex[bot]",
    "codex-bot",
    "codex-bot[bot]",
    "renovate",
    "renovate[bot]",
    "dependabot",
    "dependabot[bot]",
    "vercel",
    "vercel[bot]",
];

/// Per-PR row written to the JSONL output.
#[derive(Debug, Clone, Serialize)]
pub struct PrRow {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub author: String,
    pub created_at: String,
    pub merged_at: String,
    pub review_decision: String,
    pub total_comments: u64,
    pub human_comments: u64,
    pub bot_comments: u64,
    pub total_reviews: u64,
    pub human_reviews: u64,
    pub time_to_first_review_hours: Option<f64>,
    pub codex_findings: CodexFindings,
    pub coderabbit_findings: CodeRabbitFindings,
    pub coderabbit_remediation_pct: Option<f64>,
    pub has_human_reviewer: bool,
}

/// Summary aggregates written to `pr-review-summary.json`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PrReviewReport {
    pub repos: Vec<String>,
    pub window_days: u32,
    pub total_prs: u64,
    pub avg_comments_per_pr: f64,
    pub p50_time_to_first_review_hours: f64,
    pub p90_time_to_first_review_hours: f64,
    pub codex_findings_total: u64,
    pub coderabbit_findings_total: u64,
    pub human_review_pct: f64,
    pub per_repo: Vec<PerRepo>,
}

/// Per-repo aggregate roll-up.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PerRepo {
    pub repo: String,
    pub prs: u64,
    pub avg_comments: f64,
    pub p50_ttfr_hours: f64,
    pub p90_ttfr_hours: f64,
    pub codex_findings: u64,
    pub coderabbit_findings: u64,
    pub human_review_pct: f64,
}

/// Codex severity-block counts within a single PR.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CodexFindings {
    pub critical: u64,
    pub high: u64,
    pub medium: u64,
    pub low: u64,
    pub total: u64,
}

/// CodeRabbit severity-block counts within a single PR.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CodeRabbitFindings {
    pub critical: u64,
    pub potential_issue: u64,
    pub suggestion: u64,
    pub nitpick: u64,
    pub total: u64,
}

/// Public entry: walk `repos` for merged PRs over the last `window_days`,
/// write the JSONL + summary JSON into `output_dir`, return the report.
pub fn scan_pr_reviews(
    window_days: u32,
    repos: &[&str],
    output_dir: &Path,
) -> Result<PrReviewReport> {
    fs::create_dir_all(output_dir)
        .with_context(|| format!("create_dir_all {}", output_dir.display()))?;

    let jsonl_path = output_dir.join("pr-review.jsonl");
    let summary_path = output_dir.join("pr-review-summary.json");

    let mut all_rows: Vec<PrRow> = Vec::new();
    let mut per_repo_rollups: Vec<(String, Vec<PrRow>)> = Vec::new();

    let cutoff = Utc::now() - chrono::Duration::days(i64::from(window_days));

    for repo in repos {
        let listing = match list_merged_prs(repo, window_days * 2) {
            // 2x window so PRs created just before the cutoff but merged inside
            // it still surface; we filter precisely via `merged_at` below.
            Ok(v) => v,
            Err(e) => {
                eprintln!("[pr-review] skip {repo}: {e}");
                continue;
            }
        };

        let mut repo_rows: Vec<PrRow> = Vec::new();
        for summary in listing {
            let Some(merged_at) = parse_iso(&summary.merged_at) else {
                continue;
            };
            if merged_at < cutoff {
                continue;
            }

            let detail = match fetch_pr_detail(repo, summary.number) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[pr-review] skip {repo}#{}: {e}", summary.number);
                    continue;
                }
            };

            let row = build_row(repo, &summary, &detail);
            repo_rows.push(row);
        }

        per_repo_rollups.push((((*repo).to_string()), repo_rows.clone()));
        all_rows.extend(repo_rows);
    }

    // Write JSONL (full overwrite — input is the source of truth).
    let mut jsonl_file =
        File::create(&jsonl_path).with_context(|| format!("create {}", jsonl_path.display()))?;
    for row in &all_rows {
        let line = serde_json::to_string(row)?;
        writeln!(jsonl_file, "{line}")?;
    }
    jsonl_file.flush()?;

    // Aggregate.
    let report = build_report(repos, window_days, &all_rows, &per_repo_rollups);

    // Write summary JSON.
    let summary_file = File::create(&summary_path)
        .with_context(|| format!("create {}", summary_path.display()))?;
    serde_json::to_writer_pretty(summary_file, &report)?;

    Ok(report)
}

/// Roll an aggregate report over the per-PR rows.
fn build_report(
    repos: &[&str],
    window_days: u32,
    all_rows: &[PrRow],
    per_repo_rollups: &[(String, Vec<PrRow>)],
) -> PrReviewReport {
    let total_prs = all_rows.len() as u64;
    let avg_comments = if total_prs == 0 {
        0.0
    } else {
        let sum: u64 = all_rows.iter().map(|r| r.total_comments).sum();
        #[allow(clippy::cast_precision_loss)]
        {
            sum as f64 / total_prs as f64
        }
    };

    let ttfrs: Vec<f64> = all_rows
        .iter()
        .filter_map(|r| r.time_to_first_review_hours)
        .collect();
    let (p50, p90) = (percentile(&ttfrs, 50.0), percentile(&ttfrs, 90.0));

    let codex_total: u64 = all_rows.iter().map(|r| r.codex_findings.total).sum();
    let cr_total: u64 = all_rows.iter().map(|r| r.coderabbit_findings.total).sum();

    let with_human: u64 = all_rows.iter().filter(|r| r.has_human_reviewer).count() as u64;
    let human_pct = if total_prs == 0 {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        {
            (with_human as f64 / total_prs as f64) * 100.0
        }
    };

    let per_repo: Vec<PerRepo> = per_repo_rollups
        .iter()
        .map(|(repo, rows)| build_per_repo(repo, rows))
        .collect();

    PrReviewReport {
        repos: repos.iter().map(ToString::to_string).collect(),
        window_days,
        total_prs,
        avg_comments_per_pr: round_2(avg_comments),
        p50_time_to_first_review_hours: round_2(p50),
        p90_time_to_first_review_hours: round_2(p90),
        codex_findings_total: codex_total,
        coderabbit_findings_total: cr_total,
        human_review_pct: round_2(human_pct),
        per_repo,
    }
}

fn build_per_repo(repo: &str, rows: &[PrRow]) -> PerRepo {
    let prs = rows.len() as u64;
    if prs == 0 {
        return PerRepo {
            repo: repo.to_string(),
            ..PerRepo::default()
        };
    }
    let avg_comments = {
        let sum: u64 = rows.iter().map(|r| r.total_comments).sum();
        #[allow(clippy::cast_precision_loss)]
        {
            sum as f64 / prs as f64
        }
    };
    let ttfrs: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.time_to_first_review_hours)
        .collect();
    let with_human: u64 = rows.iter().filter(|r| r.has_human_reviewer).count() as u64;
    #[allow(clippy::cast_precision_loss)]
    let human_pct = (with_human as f64 / prs as f64) * 100.0;
    PerRepo {
        repo: repo.to_string(),
        prs,
        avg_comments: round_2(avg_comments),
        p50_ttfr_hours: round_2(percentile(&ttfrs, 50.0)),
        p90_ttfr_hours: round_2(percentile(&ttfrs, 90.0)),
        codex_findings: rows.iter().map(|r| r.codex_findings.total).sum(),
        coderabbit_findings: rows.iter().map(|r| r.coderabbit_findings.total).sum(),
        human_review_pct: round_2(human_pct),
    }
}

/// Build a single `PrRow` from the listing summary + detail JSON.
fn build_row(repo: &str, summary: &PrSummaryRaw, detail: &PrDetailRaw) -> PrRow {
    let comments = &detail.comments;
    let reviews = &detail.reviews;

    let mut human_comments = 0u64;
    let mut bot_comments = 0u64;
    let mut codex = CodexFindings::default();
    let mut crab = CodeRabbitFindings::default();

    for c in comments {
        if is_bot(&c.author) {
            bot_comments += 1;
            // Aggregate findings from any bot comment body — bots may
            // change names over time but the structured signature is stable.
            accumulate_findings(&c.body, &c.author, &mut codex, &mut crab);
        } else {
            human_comments += 1;
        }
    }

    let mut human_review_authors: HashSet<String> = HashSet::new();
    let mut total_reviews = 0u64;
    let mut human_reviews = 0u64;
    for r in reviews {
        total_reviews += 1;
        if is_bot(&r.author) {
            accumulate_findings(&r.body, &r.author, &mut codex, &mut crab);
        } else {
            human_reviews += 1;
            human_review_authors.insert(r.author.clone());
        }
    }

    let ttfr = compute_ttfr_hours(&summary.created_at, reviews);

    let coderabbit_remediation_pct = compute_coderabbit_remediation(detail);

    PrRow {
        repo: repo.to_string(),
        number: summary.number,
        title: summary.title.clone(),
        author: summary.author.clone(),
        created_at: summary.created_at.clone(),
        merged_at: summary.merged_at.clone(),
        review_decision: summary.review_decision.clone(),
        total_comments: comments.len() as u64,
        human_comments,
        bot_comments,
        total_reviews,
        human_reviews,
        time_to_first_review_hours: ttfr,
        codex_findings: codex.total_set(),
        coderabbit_findings: crab.total_set(),
        coderabbit_remediation_pct,
        has_human_reviewer: !human_review_authors.is_empty(),
    }
}

impl CodexFindings {
    fn total_set(self) -> Self {
        let total = self.critical + self.high + self.medium + self.low;
        Self { total, ..self }
    }
}

impl CodeRabbitFindings {
    fn total_set(self) -> Self {
        let total = self.critical + self.potential_issue + self.suggestion + self.nitpick;
        Self { total, ..self }
    }
}

/// Return true if `login` is on the bot allow-list (case-insensitive).
#[must_use]
pub fn is_bot(login: &str) -> bool {
    let l = login.to_ascii_lowercase();
    BOT_LOGINS.iter().any(|b| b.eq_ignore_ascii_case(&l))
}

/// Count Codex severity blocks in a body. Codex emits findings in
/// `Severity: CRITICAL` / `**Severity:** HIGH` / `[CRITICAL]` /
/// `🔴 Critical` / `## Critical` style headers — we accept all common
/// shapes via `codex_severity_regex`. Each matched block bumps the
/// matching counter exactly once.
#[must_use]
pub fn count_codex_findings(body: &str) -> CodexFindings {
    let mut f = CodexFindings::default();
    for cap in codex_severity_regex().captures_iter(body) {
        let Some(level) = cap.get(1) else { continue };
        match level.as_str().to_ascii_lowercase().as_str() {
            "critical" => f.critical += 1,
            "high" => f.high += 1,
            "medium" => f.medium += 1,
            "low" => f.low += 1,
            _ => {}
        }
    }
    f
}

/// Count CodeRabbit severity blocks. CodeRabbit reviews use shapes like:
///   `_⚠️ Potential issue_ | _🔴 Critical_`
///   `_🛠️ Refactor suggestion_`
///   `_🧹 Nitpick (assertive)_`
/// The regex below picks up the four headline categories.
#[must_use]
pub fn count_coderabbit_findings(body: &str) -> CodeRabbitFindings {
    let mut f = CodeRabbitFindings::default();
    let lower = body.to_ascii_lowercase();
    // Each match bumps exactly once — distinct findings live in distinct
    // `<details>` blocks so multiple hits is the desired semantic.
    for m in coderabbit_critical_regex().find_iter(&lower) {
        let _ = m;
        f.critical += 1;
    }
    for m in coderabbit_potential_regex().find_iter(&lower) {
        let _ = m;
        f.potential_issue += 1;
    }
    for m in coderabbit_suggestion_regex().find_iter(&lower) {
        let _ = m;
        f.suggestion += 1;
    }
    for m in coderabbit_nitpick_regex().find_iter(&lower) {
        let _ = m;
        f.nitpick += 1;
    }
    f
}

/// Hours from PR open to the first non-bot review (or first review at
/// all, if no human review exists). Returns `None` when no reviews.
#[must_use]
pub fn compute_ttfr_hours(created_at: &str, reviews: &[ReviewRaw]) -> Option<f64> {
    let created = parse_iso(created_at)?;
    let first = reviews
        .iter()
        .filter_map(|r| parse_iso(&r.submitted_at))
        .min()?;
    let delta = first - created;
    let secs = delta.num_seconds();
    if secs < 0 {
        return Some(0.0);
    }
    #[allow(clippy::cast_precision_loss)]
    Some((secs as f64) / 3600.0)
}

/// Quantile over an arbitrary slice of f64. Linear interpolation.
/// Returns 0 for empty input. `q` must be in `[0, 100]`.
#[must_use]
pub fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    #[allow(clippy::cast_precision_loss)]
    let n = sorted.len() as f64;
    let pos = (q / 100.0) * (n - 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo = pos.floor() as usize;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hi = pos.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = pos - (lo as f64);
    sorted[lo] + (sorted[hi] - sorted[lo]) * frac
}

/// Heuristic remediation rate for CodeRabbit: # of CodeRabbit-authored
/// review threads that have a follow-up commit after their submitted_at.
/// Returns `None` if there are no CodeRabbit reviews to score against.
fn compute_coderabbit_remediation(detail: &PrDetailRaw) -> Option<f64> {
    let cr_reviews: Vec<&ReviewRaw> = detail
        .reviews
        .iter()
        .filter(|r| r.author.eq_ignore_ascii_case("coderabbitai"))
        .collect();
    if cr_reviews.is_empty() {
        return None;
    }

    let commit_times: Vec<DateTime<Utc>> = detail
        .commits
        .iter()
        .filter_map(|c| parse_iso(&c.committed_date))
        .collect();
    if commit_times.is_empty() {
        return Some(0.0);
    }
    let mut addressed = 0u64;
    for r in &cr_reviews {
        let Some(submitted) = parse_iso(&r.submitted_at) else {
            continue;
        };
        if commit_times.iter().any(|c| *c > submitted) {
            addressed += 1;
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let pct = (addressed as f64) / (cr_reviews.len() as f64) * 100.0;
    Some(round_2(pct))
}

fn accumulate_findings(
    body: &str,
    author: &str,
    codex: &mut CodexFindings,
    crab: &mut CodeRabbitFindings,
) {
    if author.eq_ignore_ascii_case("coderabbitai")
        || author.to_ascii_lowercase().contains("coderabbit")
    {
        let f = count_coderabbit_findings(body);
        crab.critical += f.critical;
        crab.potential_issue += f.potential_issue;
        crab.suggestion += f.suggestion;
        crab.nitpick += f.nitpick;
    } else if author.to_ascii_lowercase().contains("codex") {
        let f = count_codex_findings(body);
        codex.critical += f.critical;
        codex.high += f.high;
        codex.medium += f.medium;
        codex.low += f.low;
    } else {
        // Unknown bot — still try Codex shape since the format is
        // distinct enough not to false-positive on CodeRabbit.
        let f = count_codex_findings(body);
        codex.critical += f.critical;
        codex.high += f.high;
        codex.medium += f.medium;
        codex.low += f.low;
    }
}

fn round_2(v: f64) -> f64 {
    if !v.is_finite() {
        return 0.0;
    }
    (v * 100.0).round() / 100.0
}

fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

// ---- gh CLI shell-out + JSON parsing ----

/// Lightweight per-PR fields parsed from `gh pr list`.
#[derive(Debug, Clone)]
pub struct PrSummaryRaw {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub created_at: String,
    pub merged_at: String,
    pub review_decision: String,
}

/// Per-PR detail parsed from `gh pr view`.
#[derive(Debug, Clone, Default)]
pub struct PrDetailRaw {
    pub comments: Vec<CommentRaw>,
    pub reviews: Vec<ReviewRaw>,
    pub commits: Vec<CommitRaw>,
}

#[derive(Debug, Clone)]
pub struct CommentRaw {
    pub author: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct ReviewRaw {
    pub author: String,
    pub body: String,
    pub state: String,
    pub submitted_at: String,
}

/// Per-commit metadata parsed from `gh pr view ... --json commits`.
#[derive(Debug, Clone)]
pub struct CommitRaw {
    pub committed_date: String,
}

fn list_merged_prs(repo: &str, window_days: u32) -> Result<Vec<PrSummaryRaw>> {
    // We over-fetch here because `gh pr list` doesn't take a date filter;
    // we trim to `cutoff` in the caller.
    let limit = std::cmp::max(window_days * 6, 50).min(500);
    let output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--repo",
            repo,
            "--state",
            "merged",
            "--limit",
            &limit.to_string(),
            "--json",
            "number,title,createdAt,mergedAt,author,reviewDecision",
        ])
        .output()
        .with_context(|| format!("spawn `gh pr list` for {repo}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).to_string();
        anyhow::bail!("gh pr list failed for {repo}: {err}");
    }
    parse_pr_list(&String::from_utf8_lossy(&output.stdout))
}

/// Parse the JSON returned by `gh pr list ... --json number,title,...`.
pub fn parse_pr_list(json: &str) -> Result<Vec<PrSummaryRaw>> {
    let arr: serde_json::Value = serde_json::from_str(json).context("parse gh pr list json")?;
    let mut out = Vec::new();
    for item in arr.as_array().unwrap_or(&Vec::new()) {
        let number = item
            .get("number")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if number == 0 {
            continue;
        }
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let author = item
            .pointer("/author/login")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let created_at = item
            .get("createdAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let merged_at = item
            .get("mergedAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let review_decision = item
            .get("reviewDecision")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(PrSummaryRaw {
            number,
            title,
            author,
            created_at,
            merged_at,
            review_decision,
        });
    }
    Ok(out)
}

fn fetch_pr_detail(repo: &str, number: u64) -> Result<PrDetailRaw> {
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &number.to_string(),
            "--repo",
            repo,
            "--json",
            "comments,reviews,commits",
        ])
        .output()
        .with_context(|| format!("spawn `gh pr view` for {repo}#{number}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).to_string();
        anyhow::bail!("gh pr view failed for {repo}#{number}: {err}");
    }
    parse_pr_detail(&String::from_utf8_lossy(&output.stdout))
}

/// Parse the JSON returned by `gh pr view ... --json comments,reviews,commits`.
pub fn parse_pr_detail(json: &str) -> Result<PrDetailRaw> {
    let value: serde_json::Value = serde_json::from_str(json).context("parse gh pr view json")?;

    let mut comments = Vec::new();
    if let Some(arr) = value.get("comments").and_then(|v| v.as_array()) {
        for c in arr {
            comments.push(CommentRaw {
                author: c
                    .pointer("/author/login")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                body: c
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                created_at: c
                    .get("createdAt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }

    let mut reviews = Vec::new();
    if let Some(arr) = value.get("reviews").and_then(|v| v.as_array()) {
        for r in arr {
            reviews.push(ReviewRaw {
                author: r
                    .pointer("/author/login")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                body: r
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                state: r
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                submitted_at: r
                    .get("submittedAt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }

    let mut commits = Vec::new();
    if let Some(arr) = value.get("commits").and_then(|v| v.as_array()) {
        for c in arr {
            commits.push(CommitRaw {
                committed_date: c
                    .get("committedDate")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }

    Ok(PrDetailRaw {
        comments,
        reviews,
        commits,
    })
}

// ---- regexes ----

fn codex_severity_regex() -> &'static regex::Regex {
    // Matches lines like:
    //   "Severity: CRITICAL"
    //   "**Severity:** HIGH"
    //   "[CRITICAL]"
    //   "## Critical"
    //   "🔴 Critical"
    // Multiline mode (`(?m)`) so `^` anchors against any line start so
    // markdown headers like `## Low` are recognised mid-body.
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?im)(?:severity\s*[:=]\s*\**\s*|\[\s*|^#{1,6}\s*|🔴\s*|🟠\s*|🟡\s*|🟢\s*)(critical|high|medium|low)\b"
        )
        .expect("static regex compiles")
    })
}

fn coderabbit_critical_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"🔴\s*critical|⚠️[^\n]{0,40}critical").expect("static regex compiles")
    })
}

fn coderabbit_potential_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"⚠️\s*potential issue|potential\s*issue\b")
            .expect("static regex compiles")
    })
}

fn coderabbit_suggestion_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"🛠️\s*refactor\s*suggestion|refactor\s*suggestion\b")
            .expect("static regex compiles")
    })
}

fn coderabbit_nitpick_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"🧹\s*nitpick|\bnitpick\b").expect("static regex compiles")
    })
}

/// Default output directory: `~/.claude/sentinel/metrics/`.
#[must_use]
pub fn default_output_dir() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("sentinel")
            .join("metrics"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_block_parser_counts_severity_levels() {
        let body = "Findings:\n\
                    \n\
                    Severity: CRITICAL\n\
                    Issue with parsing.\n\
                    \n\
                    Severity: HIGH\n\
                    Race condition possible.\n\
                    \n\
                    [MEDIUM]\n\
                    Magic constant.\n\
                    \n\
                    ## Low\n\
                    Comment style.\n";
        let f = count_codex_findings(body);
        assert_eq!(f.critical, 1, "critical");
        assert_eq!(f.high, 1, "high");
        assert_eq!(f.medium, 1, "medium");
        assert_eq!(f.low, 1, "low");
    }

    #[test]
    fn codex_block_parser_handles_emoji_severity() {
        let body = "🔴 Critical: data loss\n🟠 High: API mismatch\n🟡 Medium: docs gap\n";
        let f = count_codex_findings(body);
        assert_eq!(f.critical, 1);
        assert_eq!(f.high, 1);
        assert_eq!(f.medium, 1);
    }

    #[test]
    fn codex_parser_returns_zero_on_unrelated_body() {
        let body = "Just a normal CodeRabbit walkthrough — no severity tags here.";
        let f = count_codex_findings(body);
        assert_eq!(f.critical + f.high + f.medium + f.low, 0);
    }

    #[test]
    fn coderabbit_parser_counts_categories() {
        let body = "_⚠️ Potential issue_ | _🔴 Critical_\nThis is a critical bug.\n\
                    \n\
                    ## file.ts (2)\n\
                    `1-10`: 🛠️ Refactor suggestion — extract function.\n\
                    `12-15`: 🧹 Nitpick (assertive) — rename variable.\n";
        let f = count_coderabbit_findings(body);
        assert!(f.critical >= 1, "critical detected");
        assert!(f.potential_issue >= 1, "potential issue detected");
        assert!(f.suggestion >= 1, "refactor suggestion detected");
        assert!(f.nitpick >= 1, "nitpick detected");
    }

    #[test]
    fn coderabbit_signature_distinguishes_bot_from_human() {
        assert!(is_bot("coderabbitai"));
        assert!(is_bot("coderabbitai[bot]"));
        assert!(is_bot("CODERABBITAI"));
        assert!(is_bot("github-actions[bot]"));
        assert!(is_bot("linear"));
        assert!(!is_bot("garysomerhalder"));
        assert!(!is_bot("alice"));
    }

    #[test]
    fn ttfr_hours_from_fixture() {
        let reviews = vec![
            ReviewRaw {
                author: "alice".into(),
                body: "lgtm".into(),
                state: "APPROVED".into(),
                submitted_at: "2026-04-30T11:00:00Z".into(),
            },
            ReviewRaw {
                author: "bob".into(),
                body: "comment".into(),
                state: "COMMENTED".into(),
                submitted_at: "2026-04-30T12:30:00Z".into(),
            },
        ];
        let ttfr = compute_ttfr_hours("2026-04-30T10:00:00Z", &reviews).unwrap();
        // 1.0h to first (alice).
        assert!((ttfr - 1.0).abs() < 0.01, "ttfr was {ttfr}");
    }

    #[test]
    fn ttfr_returns_none_when_no_reviews() {
        let ttfr = compute_ttfr_hours("2026-04-30T10:00:00Z", &[]);
        assert_eq!(ttfr, None);
    }

    #[test]
    fn percentiles_match_expected_for_known_distribution() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        let p50 = percentile(&v, 50.0);
        let p90 = percentile(&v, 90.0);
        // Linear interpolation: position = 0.5 * 9 = 4.5 -> between 5 (idx 4)
        // and 6 (idx 5) -> 5.5. p90 position = 0.9 * 9 = 8.1 -> between
        // 9 (idx 8) and 10 (idx 9) -> 9.1.
        assert!((p50 - 5.5).abs() < 0.01, "p50 was {p50}");
        assert!((p90 - 9.1).abs() < 0.01, "p90 was {p90}");
    }

    #[test]
    fn percentile_handles_empty_input() {
        assert_eq!(percentile(&[], 50.0), 0.0);
        assert_eq!(percentile(&[], 90.0), 0.0);
    }

    #[test]
    fn percentile_handles_single_element() {
        assert_eq!(percentile(&[42.0], 50.0), 42.0);
        assert_eq!(percentile(&[42.0], 90.0), 42.0);
    }

    #[test]
    fn parse_pr_list_extracts_basic_fields() {
        let json = r#"[
            {"number":768,"title":"fix(calendar): foo","createdAt":"2026-04-30T23:53:23Z","mergedAt":"2026-05-01T00:27:10Z","author":{"login":"alice"},"reviewDecision":"APPROVED"},
            {"number":767,"title":"fix(unipile): bar","createdAt":"2026-04-30T22:14:08Z","mergedAt":"2026-04-30T22:48:29Z","author":{"login":"bob"},"reviewDecision":""}
        ]"#;
        let rows = parse_pr_list(json).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].number, 768);
        assert_eq!(rows[0].author, "alice");
        assert_eq!(rows[0].review_decision, "APPROVED");
        assert_eq!(rows[1].number, 767);
    }

    #[test]
    fn parse_pr_detail_extracts_comments_reviews_commits() {
        let json = r#"{
            "comments":[
                {"author":{"login":"coderabbitai"},"body":"_⚠️ Potential issue_ | _🔴 Critical_","createdAt":"2026-04-30T10:00:00Z"},
                {"author":{"login":"alice"},"body":"lgtm","createdAt":"2026-04-30T11:00:00Z"}
            ],
            "reviews":[
                {"author":{"login":"bob"},"body":"comment","state":"COMMENTED","submittedAt":"2026-04-30T11:30:00Z"}
            ],
            "commits":[
                {"committedDate":"2026-04-30T12:00:00Z"}
            ]
        }"#;
        let detail = parse_pr_detail(json).unwrap();
        assert_eq!(detail.comments.len(), 2);
        assert_eq!(detail.reviews.len(), 1);
        assert_eq!(detail.commits.len(), 1);
        assert_eq!(detail.comments[0].author, "coderabbitai");
        assert_eq!(detail.reviews[0].author, "bob");
    }

    #[test]
    fn build_row_aggregates_correctly() {
        let summary = PrSummaryRaw {
            number: 1,
            title: "feat: foo".into(),
            author: "alice".into(),
            created_at: "2026-04-30T10:00:00Z".into(),
            merged_at: "2026-04-30T18:00:00Z".into(),
            review_decision: "APPROVED".into(),
        };
        let detail = PrDetailRaw {
            comments: vec![
                CommentRaw {
                    author: "coderabbitai".into(),
                    body: "🔴 critical issue here".into(),
                    created_at: String::new(),
                },
                CommentRaw {
                    author: "bob".into(),
                    body: "looks good".into(),
                    created_at: String::new(),
                },
            ],
            reviews: vec![ReviewRaw {
                author: "alice2".into(),
                body: "lgtm".into(),
                state: "APPROVED".into(),
                submitted_at: "2026-04-30T11:00:00Z".into(),
            }],
            commits: vec![],
        };
        let row = build_row("test/repo", &summary, &detail);
        assert_eq!(row.number, 1);
        assert_eq!(row.total_comments, 2);
        assert_eq!(row.bot_comments, 1);
        assert_eq!(row.human_comments, 1);
        assert_eq!(row.total_reviews, 1);
        assert_eq!(row.human_reviews, 1);
        assert!(row.has_human_reviewer);
        assert!(row.coderabbit_findings.total >= 1);
        let ttfr = row.time_to_first_review_hours.unwrap();
        assert!((ttfr - 1.0).abs() < 0.01);
    }

    #[test]
    fn report_aggregates_across_repos() {
        // Two synthetic rows in two repos.
        let row_a = PrRow {
            repo: "org/a".into(),
            number: 1,
            title: String::new(),
            author: String::new(),
            created_at: String::new(),
            merged_at: String::new(),
            review_decision: String::new(),
            total_comments: 4,
            human_comments: 2,
            bot_comments: 2,
            total_reviews: 2,
            human_reviews: 1,
            time_to_first_review_hours: Some(2.0),
            codex_findings: CodexFindings::default(),
            coderabbit_findings: CodeRabbitFindings::default(),
            coderabbit_remediation_pct: None,
            has_human_reviewer: true,
        };
        let row_b = PrRow {
            repo: "org/b".into(),
            number: 1,
            title: String::new(),
            author: String::new(),
            created_at: String::new(),
            merged_at: String::new(),
            review_decision: String::new(),
            total_comments: 0,
            human_comments: 0,
            bot_comments: 0,
            total_reviews: 0,
            human_reviews: 0,
            time_to_first_review_hours: None,
            codex_findings: CodexFindings::default(),
            coderabbit_findings: CodeRabbitFindings::default(),
            coderabbit_remediation_pct: None,
            has_human_reviewer: false,
        };
        let all = vec![row_a.clone(), row_b.clone()];
        let per_repo = vec![
            ("org/a".to_string(), vec![row_a]),
            ("org/b".to_string(), vec![row_b]),
        ];
        let report = build_report(&["org/a", "org/b"], 30, &all, &per_repo);
        assert_eq!(report.total_prs, 2);
        assert!((report.avg_comments_per_pr - 2.0).abs() < 0.01);
        // Only one TTFR (2.0) — both p50 and p90 collapse to it.
        assert!((report.p50_time_to_first_review_hours - 2.0).abs() < 0.01);
        assert!((report.p90_time_to_first_review_hours - 2.0).abs() < 0.01);
        assert!((report.human_review_pct - 50.0).abs() < 0.01);
    }
}
