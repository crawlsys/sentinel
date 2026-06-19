//! `sentinel deploy-freq` — record + aggregate deploy events (SEN-9).
//!
//! Two actions:
//!
//!   * `aggregate` — read `~/.claude/sentinel/metrics/deploys.jsonl`,
//!     compute 7d/30d rolling counts per (repo, env), write
//!     `deploys-summary.json`, print a human-readable summary.
//!   * `record` — append a single [`DeployRecord`] for manual entry, testing,
//!     and the backfill path before Hookdeck wiring lands.

use anyhow::{Context, Result};
use chrono::Utc;
use colored::Colorize;
use sentinel_application::deploy_freq::{aggregate, append_deploy, DeployRecord, DoraTier};
use std::path::PathBuf;

fn metrics_dir() -> Result<PathBuf> {
    Ok(sentinel_infrastructure::paths::sentinel_root().join("metrics"))
}

/// Run `sentinel deploy-freq aggregate`.
///
/// # Errors
/// Returns the aggregation error (IO, parse, or write).
pub async fn run_aggregate() -> Result<()> {
    let dir = metrics_dir()?;
    let deploys = dir.join("deploys.jsonl");
    let summary = dir.join("deploys-summary.json");
    let graph_runs = summary.with_extension("graph-runs.jsonl");

    println!("{}", "Sentinel Deploy Frequency".bold());
    println!("Source:  {}", deploys.display());
    println!("Summary: {}", summary.display());
    println!("Graph:   {}", graph_runs.display());
    println!();

    let s = aggregate(&deploys, &summary).context("aggregate deploys")?;
    let graph_audit = crate::deploy_freq_graph::run_deploy_frequency_graph_audit(&s, &graph_runs)
        .await
        .context("deploy frequency graph audit failed")?;
    println!("{}", "Summary".bold());
    println!("  Records scanned:    {}", s.records_scanned);
    println!("  Repo/env pairs:     {}", s.aggregates.len());
    println!(
        "  Graph decision:     {} ({})",
        graph_audit.decision.bold(),
        graph_audit.authorization_checkpoint.as_str().dimmed()
    );
    println!();

    if s.aggregates.is_empty() {
        println!(
            "{}",
            "No deploys recorded yet. Use `sentinel deploy-freq record` or wire \
             Hookdeck `deployment.success` events to populate the stream."
                .yellow()
        );
        return Ok(());
    }

    println!(
        "{}",
        "Per repo/env (sorted by 30d rate, descending):".bold()
    );
    println!(
        "  {:<26}  {:>8}  {:>6}  {:>6}  {:>10}  {:>10}",
        "repo/env", "tier", "7d", "30d", "rate/d 7d", "rate/d 30d"
    );

    let mut sorted = s.aggregates;
    sorted.sort_by(|a, b| {
        b.rate_per_day_30d
            .partial_cmp(&a.rate_per_day_30d)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for a in &sorted {
        let label = format!("{}/{}", a.repo, a.env);
        let label = if label.len() > 26 {
            format!("{}…", &label[..25])
        } else {
            label
        };
        let tier_str = tier_label(a.tier);
        println!(
            "  {:<26}  {:>8}  {:>6}  {:>6}  {:>10}  {:>10}",
            label,
            tier_str,
            a.deploys_7d,
            a.deploys_30d,
            format!("{:.3}", a.rate_per_day_7d),
            format!("{:.3}", a.rate_per_day_30d)
        );
    }

    let elite_count = sorted
        .iter()
        .filter(|a| matches!(a.tier, DoraTier::Elite))
        .count();
    let low_count = sorted
        .iter()
        .filter(|a| matches!(a.tier, DoraTier::Low))
        .count();
    println!();
    println!(
        "  {} elite, {} low (target: zero `low` outside of intentionally-paused repos)",
        elite_count.to_string().green(),
        low_count.to_string().red()
    );

    Ok(())
}

/// Run `sentinel deploy-freq record --repo X --env Y --commit Z [--duration-s N]`.
///
/// `timestamp` defaults to `Utc::now()` when omitted.
///
/// # Errors
/// Returns the append error (IO or serde).
pub fn run_record(
    repo: String,
    env: String,
    commit: String,
    duration_s: Option<u64>,
    timestamp: Option<String>,
) -> Result<()> {
    let dir = metrics_dir()?;
    let deploys = dir.join("deploys.jsonl");
    let rec = DeployRecord {
        timestamp: timestamp.unwrap_or_else(|| Utc::now().to_rfc3339()),
        repo,
        env,
        commit,
        duration_s,
    };
    append_deploy(&deploys, &rec)?;
    println!(
        "{} recorded deploy {} → {}/{} @ {}",
        "✓".green(),
        rec.commit,
        rec.repo,
        rec.env,
        rec.timestamp
    );
    Ok(())
}

fn tier_label(tier: DoraTier) -> colored::ColoredString {
    match tier {
        DoraTier::Elite => "elite".green().bold(),
        DoraTier::High => "high".green(),
        DoraTier::Medium => "medium".yellow(),
        DoraTier::Low => "low".red(),
    }
}
