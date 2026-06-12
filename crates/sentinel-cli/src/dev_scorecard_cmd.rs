//! `sentinel dev-scorecard scan` — compute per-developer scorecards from
//! the git-stats input + the Linear issue cache (throughput, first-pass QA,
//! consistency, composite score, and the attribution-divergence check). See
//! `sentinel-application::dev_scorecard`.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::PathBuf;

use sentinel_application::dev_scorecard::scan_dev_scorecard;

pub fn run() -> Result<()> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    let sentinel_dir: PathBuf = home.join(".claude").join("sentinel");
    let git_stats = sentinel_dir.join("dev-git-stats.json");
    let linear_cache = sentinel_dir.join("linear-assigned.json");
    let output_summary = sentinel_dir.join("metrics").join("dev-scorecard.json");

    println!("{}", "Sentinel Dev Scorecard".bold());
    println!("Git stats:      {}", git_stats.display());
    println!("Linear cache:   {}", linear_cache.display());
    println!("Output summary: {}", output_summary.display());
    println!();

    let summary = scan_dev_scorecard(&git_stats, &linear_cache, &output_summary)
        .context("scan_dev_scorecard failed")?;

    if summary.devs_total == 0 {
        println!(
            "{}",
            "No devs found in dev-git-stats.json. Populate the git-stats input \
             (a cron precomputes commits / active_days / merged_prs / \
             delivered_tickets per dev) and rescan."
                .yellow()
        );
        return Ok(());
    }

    println!("{}", "==== DEV SCORECARD ====".bold());
    println!(
        "  {} devs · {} commits · {} merged PRs",
        summary.devs_total, summary.commits_total, summary.merged_prs_total
    );
    println!();

    for d in &summary.devs {
        let score = if d.score >= 85.0 {
            format!("{:.1}", d.score).green().bold().to_string()
        } else if d.score >= 70.0 {
            format!("{:.1}", d.score).yellow().to_string()
        } else {
            format!("{:.1}", d.score).red().to_string()
        };
        println!("  {:14} score {score}", d.name.bold());
        println!(
            "    {:.1} commits/day · {:.1} PRs/wk · {} delivered ({} clean / {} bounced, {:.0}% first-pass QA)",
            d.commits_per_active_day,
            d.prs_per_week,
            d.delivered_tickets,
            d.clean_tickets,
            d.bounced_tickets,
            d.first_pass_qa_rate * 100.0,
        );
        if d.attribution_divergence {
            println!(
                "    {}",
                "⚠ attribution divergence — git shows delivery but ~0 Linear-assignee \
                 completions (the merge-reassign bug)"
                    .red()
                    .bold()
            );
        }
    }
    println!();

    if summary.attribution_divergences > 0 {
        println!(
            "{}",
            format!(
                "⚠ {} dev(s) with attribution divergence — credit landed on the wrong assignee.",
                summary.attribution_divergences
            )
            .red()
            .bold()
        );
    } else {
        println!("{}", "✓ No attribution divergence.".green());
    }

    Ok(())
}
