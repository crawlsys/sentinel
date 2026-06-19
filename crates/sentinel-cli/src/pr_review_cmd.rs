//! `sentinel pr-review scan` — walk merged PRs across firefly-pro +
//! sentinel via `gh` CLI, write `~/.claude/sentinel/metrics/pr-review.jsonl`
//! and `pr-review-summary.json`. See `sentinel-application::pr_review`.

use anyhow::{Context, Result};
use colored::Colorize;

pub async fn run(window_days: u32) -> Result<()> {
    let output_dir = sentinel_application::pr_review::default_output_dir()
        .ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    let graph_runs = output_dir.join("pr-review-summary.graph-runs.jsonl");

    println!("{}", "Sentinel PR-Review Scan".bold());
    println!("Window:   last {window_days} days");
    println!("Output:   {}", output_dir.display());
    println!("Graph:    {}", graph_runs.display());
    println!();

    let repos: Vec<&str> = sentinel_application::pr_review::DEFAULT_REPOS.to_vec();
    println!("Walking {} repos...", repos.len());
    for r in &repos {
        println!("  - {r}");
    }
    println!();

    let report = sentinel_application::pr_review::scan_pr_reviews(window_days, &repos, &output_dir)
        .context("scan_pr_reviews failed")?;
    let graph_audit = crate::pr_review_graph::run_pr_review_graph_audit(&report, &graph_runs)
        .await
        .context("PR review graph audit failed")?;

    println!("{}", "Summary".bold());
    println!("  Total merged PRs:           {}", report.total_prs);
    println!(
        "  Graph decision:             {} ({})",
        graph_audit.decision.bold(),
        graph_audit
            .authorization_checkpoint
            .as_deref()
            .expect("PR review graph audit requires checkpoint")
            .dimmed()
    );
    println!(
        "  Avg comments per PR:        {:.2}",
        report.avg_comments_per_pr
    );
    println!(
        "  p50 time-to-first-review:   {:.2}h",
        report.p50_time_to_first_review_hours
    );
    println!(
        "  p90 time-to-first-review:   {:.2}h",
        report.p90_time_to_first_review_hours
    );
    println!(
        "  Codex findings (all):       {}",
        report.codex_findings_total.to_string().yellow()
    );
    println!(
        "  CodeRabbit findings (all):  {}",
        report.coderabbit_findings_total.to_string().yellow()
    );
    let pct_color = if report.human_review_pct >= 50.0 {
        report.human_review_pct.to_string().green()
    } else {
        report.human_review_pct.to_string().red()
    };
    println!("  Human-in-the-loop %:        {pct_color}%");
    println!();

    if !report.per_repo.is_empty() {
        println!("{}", "Per-repo breakdown".bold());
        for r in &report.per_repo {
            if r.prs == 0 {
                continue;
            }
            println!(
                "  {:<42} prs={:<3}  avg-cmts={:<5.1}  p50={:<5.1}h  p90={:<5.1}h  cr={:<3}  cdx={:<3}  hum%={:<5.1}",
                r.repo,
                r.prs,
                r.avg_comments,
                r.p50_ttfr_hours,
                r.p90_ttfr_hours,
                r.coderabbit_findings,
                r.codex_findings,
                r.human_review_pct
            );
        }
    }

    Ok(())
}
