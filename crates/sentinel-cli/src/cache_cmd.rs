//! `sentinel cache scan` — compute prompt-cache hit rates across all
//! Claude Code sessions. See `sentinel-application::cache_efficiency`.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::PathBuf;

pub async fn run(top: usize) -> Result<()> {
    let home = sentinel_infrastructure::paths::home_root_or_fatal();
    let projects: PathBuf = home.join(".claude").join("projects");
    let metrics_dir: PathBuf = sentinel_infrastructure::paths::sentinel_root().join("metrics");
    let output_jsonl: PathBuf = metrics_dir.join("cache-efficiency.jsonl");
    let output_summary: PathBuf = metrics_dir.join("cache-efficiency-summary.json");
    let graph_runs = output_summary.with_extension("graph-runs.jsonl");

    println!("{}", "Sentinel Cache Efficiency Scan".bold());
    println!("Projects: {}", projects.display());
    println!("Output:   {}", output_jsonl.display());
    println!("Summary:  {}", output_summary.display());
    println!("Graph:    {}", graph_runs.display());
    println!();

    let report = sentinel_application::cache_efficiency::scan_cache_efficiency(
        &projects,
        &output_jsonl,
        &output_summary,
    )
    .context("scan_cache_efficiency failed")?;
    let graph_audit =
        crate::cache_efficiency_graph::run_cache_efficiency_graph_audit(&report, &graph_runs)
            .await
            .context("cache efficiency graph audit failed")?;

    println!("{}", "Summary".bold());
    println!("  Sessions scanned:     {}", report.sessions_scanned);
    println!(
        "  Graph decision:       {} ({})",
        graph_audit.decision.bold(),
        graph_audit
            .authorization_checkpoint
            .as_deref()
            .expect("cache efficiency graph audit requires checkpoint")
            .dimmed()
    );
    println!(
        "  Sessions with usage:  {}",
        report.sessions_with_usage.to_string().green()
    );
    println!(
        "  p50 hit rate:         {}",
        format_rate(report.p50_hit_rate)
    );
    println!(
        "  p90 hit rate:         {}",
        format_rate(report.p90_hit_rate)
    );
    println!();

    if report.worst_sessions.is_empty() {
        println!("{}", "No long sessions found to rank.".yellow());
        return Ok(());
    }

    let limit = report.worst_sessions.len().min(top);
    println!(
        "{}",
        format!("Top {limit} worst sessions (by waste = (1 - hit_rate) * total_tokens):").bold()
    );
    println!(
        "  {:>2}  {:>8}  {:>14}  {:>10}  session",
        "#", "hit", "tokens", "waste $"
    );
    for (i, w) in report.worst_sessions.iter().take(top).enumerate() {
        let rank = format!("#{:>2}", i + 1);
        let hr = format!("{:.1}%", w.hit_rate * 100.0);
        let toks = format_tokens(w.total_input_tokens);
        let dollars = format!("${:.2}", w.waste_estimate_usd);
        let session_short = if w.session_id.len() > 18 {
            format!("{}…", &w.session_id[..17])
        } else {
            w.session_id.clone()
        };
        println!(
            "  {}  {:>8}  {:>14}  {:>10}  {}",
            rank.dimmed(),
            hr.red(),
            toks,
            dollars.yellow(),
            session_short
        );
    }

    if !report.daily_trend.is_empty() {
        println!();
        let recent: Vec<_> = report.daily_trend.iter().rev().take(7).collect();
        println!("{}", "Last 7 days (mean hit rate):".bold());
        for d in recent.iter().rev() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let bar_len = (d.hit_rate.clamp(0.0, 1.0) * 30.0).round() as usize;
            let bar = "█".repeat(bar_len);
            println!(
                "  {}  {} {} ({} sessions)",
                d.date.dimmed(),
                bar.green(),
                format_rate(d.hit_rate),
                d.sessions
            );
        }
    }

    Ok(())
}

fn format_rate(r: f64) -> String {
    format!("{:.1}%", r * 100.0)
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        #[allow(clippy::cast_precision_loss)]
        let v = n as f64 / 1_000_000.0;
        format!("{v:.1}M")
    } else if n >= 1_000 {
        #[allow(clippy::cast_precision_loss)]
        let v = n as f64 / 1_000.0;
        format!("{v:.0}K")
    } else {
        n.to_string()
    }
}
