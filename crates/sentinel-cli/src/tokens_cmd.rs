//! `sentinel tokens scan` — aggregate Claude Code session JSONL
//! token usage by Linear ticket. See `sentinel-application::tokens`.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::PathBuf;

pub fn run(top: usize) -> Result<()> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    let projects: PathBuf = home.join(".claude").join("projects");
    let output: PathBuf = home
        .join(".claude")
        .join("sentinel")
        .join("metrics")
        .join("tokens-per-ticket.jsonl");

    println!("{}", "Sentinel Tokens Scan".bold());
    println!("Projects: {}", projects.display());
    println!("Output:   {}", output.display());
    println!();

    let report = sentinel_application::tokens::scan_token_usage(&projects, &output)
        .context("scan_token_usage failed")?;

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
        "  Mapped to ticket:  {} ({:.1}%)",
        report.mapped_sessions.to_string().green(),
        coverage_pct
    );
    println!(
        "  Unmapped:          {}",
        report.unmapped_sessions.to_string().yellow()
    );
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
