//! `sentinel linear-health scan` — compute the composite 0-100 Linear health
//! score (hygiene + structure + data-quality + flow) over the Linear issue
//! cache. See `sentinel-application::linear_health_score`.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::PathBuf;

use sentinel_application::linear_health_score::scan_health_score;

pub async fn run() -> Result<()> {
    let sentinel_dir: PathBuf = sentinel_infrastructure::paths::sentinel_root();
    let linear_cache = sentinel_dir.join("linear-assigned.json");
    let output_summary = sentinel_dir.join("metrics").join("linear-health.json");
    let graph_runs = output_summary.with_extension("graph-runs.jsonl");

    println!("{}", "Sentinel Linear Health Score".bold());
    println!("Linear cache:   {}", linear_cache.display());
    println!("Output summary: {}", output_summary.display());
    println!("Graph audit:    {}", graph_runs.display());
    println!();

    let summary =
        scan_health_score(&linear_cache, &output_summary).context("scan_health_score failed")?;
    let graph_audit =
        crate::linear_health_graph::run_linear_health_graph_audit(&summary, &graph_runs)
            .await
            .context("Linear health graph audit failed")?;

    if summary.issues_total == 0 {
        println!(
            "{}",
            "No issues found in linear-assigned.json. Populate the cache (the \
             portfolio-health cron writes it) and rescan."
                .yellow()
        );
        return Ok(());
    }

    let band = match summary.grade.as_str() {
        "healthy" => summary.grade.green().bold(),
        "ok" => summary.grade.yellow().bold(),
        _ => summary.grade.red().bold(),
    };

    println!("{}", "==== LINEAR HEALTH ====".bold());
    println!(
        "  {} issues · score {} / 100 → {band}",
        summary.issues_total,
        summary.total_score.to_string().bold()
    );
    println!(
        "  graph decision {} · {}",
        graph_audit.decision.bold(),
        graph_audit.authorization_checkpoint.as_str().dimmed()
    );
    println!();
    println!("  {:14} {:.1} / 30", "hygiene", summary.hygiene_score);
    println!("  {:14} {:.1} / 20", "structure", summary.structure_score);
    println!(
        "  {:14} {:.1} / 15  ({} QA-failed)",
        "data_quality", summary.data_quality_score, summary.qa_failed_count
    );
    println!(
        "  {:14} {:.1} / 35  ({:.0}% open pts in QA lanes)",
        "flow",
        summary.flow_score,
        summary.qa_congestion_fraction * 100.0
    );

    Ok(())
}
