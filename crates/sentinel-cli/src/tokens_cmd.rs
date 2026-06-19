//! `sentinel tokens scan` — aggregate Claude Code session JSONL
//! token usage by Linear ticket. See `sentinel-application::tokens`.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::PathBuf;

pub async fn run(top: usize) -> Result<()> {
    let home = sentinel_infrastructure::paths::home_root_or_fatal();
    let projects: PathBuf = home.join(".claude").join("projects");
    let output: PathBuf = sentinel_infrastructure::paths::sentinel_root()
        .join("metrics")
        .join("tokens-per-ticket.jsonl");
    let graph_runs = output.with_extension("graph-runs.jsonl");

    println!("{}", "Sentinel Tokens Scan".bold());
    println!("Projects: {}", projects.display());
    println!("Output:   {}", output.display());
    println!("Graph:    {}", graph_runs.display());
    println!();

    let report = sentinel_application::tokens::scan_token_usage(&projects, &output)
        .context("scan_token_usage failed")?;
    let graph_audit = crate::token_usage_graph::run_token_usage_graph_audit(&report, &graph_runs)
        .await
        .context("token usage graph audit failed")?;

    let coverage_pct = if report.total_sessions == 0 {
        0.0_f64
    } else {
        #[allow(clippy::cast_precision_loss)]
        let pct = (report.mapped_sessions as f64) / (report.total_sessions as f64) * 100.0;
        pct
    };

    println!("{}", "Summary".bold());
    println!("  Total sessions:    {}", report.total_sessions);
    println!(
        "  Graph decision:    {} ({})",
        graph_audit.decision.bold(),
        graph_audit
            .authorization_checkpoint
            .as_deref()
            .expect("token usage graph audit requires checkpoint")
            .dimmed()
    );
    println!(
        "  Mapped to ticket:  {} ({:.1}%)",
        report.mapped_sessions.to_string().green(),
        coverage_pct
    );
    println!(
        "  Unmapped:          {}",
        report.unmapped_sessions.to_string().yellow()
    );
    if report.unpriced_tokens > 0 {
        println!(
            "  Unpriced usage:    {} sessions · {:.2}M tokens",
            report.unpriced_sessions.to_string().red(),
            m(report.unpriced_tokens)
        );
    }
    println!("  Distinct tickets:  {}", report.tickets);
    println!();

    if report.top_n_expensive.is_empty() {
        println!("{}", "No tickets with token usage found.".yellow());
        return Ok(());
    }

    let limit = report.top_n_expensive.len().min(top);
    println!("{}", format!("Top {limit} most expensive tickets:").bold());
    for (i, (ticket, cost)) in report.top_n_expensive.iter().take(top).enumerate() {
        let rank = format!("#{:>2}", i + 1);
        let dollars = format!("${cost:>9.2}");
        println!("  {} {}  {}", rank.dimmed(), dollars.green(), ticket);
    }

    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn m(t: u64) -> f64 {
    t as f64 / 1e6
}
