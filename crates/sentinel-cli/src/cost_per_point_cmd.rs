//! `sentinel cost-per-point scan` — join SEN-7 tokens-per-ticket data
//! with Linear estimates to compute tokens/point and $/point. See
//! `sentinel-application::cost_per_point`.

use anyhow::{Context, Result};
use colored::Colorize;

use sentinel_application::cost_per_point::{
    scan_cost_per_point, BucketStats, DRIFT_ALARM_THRESHOLD,
};

pub async fn run() -> Result<()> {
    let sentinel_dir = sentinel_infrastructure::paths::sentinel_root();
    let metrics_dir = sentinel_dir.join("metrics");
    let tokens_input = metrics_dir.join("tokens-per-ticket.jsonl");
    let linear_cache = sentinel_dir.join("linear-assigned.json");
    let output_jsonl = metrics_dir.join("cost-per-point.jsonl");
    let output_summary = metrics_dir.join("cost-per-point-summary.json");
    let graph_runs = output_summary.with_extension("graph-runs.jsonl");

    println!("{}", "Sentinel Cost-Per-Point Scan".bold());
    println!("Tokens input:   {}", tokens_input.display());
    println!("Linear cache:   {}", linear_cache.display());
    println!("Output JSONL:   {}", output_jsonl.display());
    println!("Output summary: {}", output_summary.display());
    println!("Graph audit:    {}", graph_runs.display());
    println!();

    let report = scan_cost_per_point(&tokens_input, &linear_cache, &output_jsonl, &output_summary)
        .context("scan_cost_per_point failed")?;
    let graph_audit =
        crate::cost_per_point_graph::run_cost_per_point_graph_audit(&report, &graph_runs)
            .await
            .context("cost-per-point graph audit failed")?;

    let coverage_pct = if report.tickets_analyzed == 0 {
        0.0_f64
    } else {
        #[allow(clippy::cast_precision_loss)]
        let pct = (report.tickets_with_estimate as f64) / (report.tickets_analyzed as f64) * 100.0;
        pct
    };

    println!("{}", "Summary".bold());
    println!("  Tickets in tokens input:  {}", report.tickets_analyzed);
    println!(
        "  Graph decision:           {} ({})",
        graph_audit.decision.bold(),
        graph_audit
            .authorization_checkpoint
            .as_deref()
            .expect("cost-per-point graph audit requires checkpoint")
            .dimmed()
    );
    println!(
        "  Tickets with estimate:    {} ({:.1}%)",
        report.tickets_with_estimate.to_string().green(),
        coverage_pct
    );
    println!();

    if report.tickets_with_estimate == 0 {
        println!(
            "{}",
            "No estimates joined — Linear cache has no `estimate` field for any ticket.".yellow()
        );
        println!(
            "{}",
            "Add the field to ~/.claude/sentinel/linear-assigned.json (or wait for the SEN-1 webhook capture) and rescan."
                .dimmed()
        );
        return Ok(());
    }

    println!("{}", "Per-bucket cost-per-point (medians + p90):".bold());
    println!(
        "  {:>6}  {:>4}  {:>10}  {:>10}  {:>12}  {:>12}",
        "Bucket".dimmed(),
        "n".dimmed(),
        "$/pt p50".dimmed(),
        "$/pt p90".dimmed(),
        "tok/pt p50".dimmed(),
        "tok/pt p90".dimmed(),
    );
    for (bucket, stats) in &report.buckets {
        print_bucket_row(*bucket, stats);
    }
    println!();

    if let Some(ratio) = report.drift_ratio_high_vs_low {
        let label = "Estimating drift (8pt $/pt ÷ 2pt $/pt):";
        let value = format!("{ratio:.2}x");
        if report.drift_alarm {
            println!(
                "{} {}  {}",
                label.bold(),
                value.red().bold(),
                format!("(threshold {DRIFT_ALARM_THRESHOLD:.1}x — sizing curve is non-linear)")
                    .red()
            );
        } else {
            println!(
                "{} {}  {}",
                label.bold(),
                value.green(),
                format!("(threshold {DRIFT_ALARM_THRESHOLD:.1}x)").dimmed()
            );
        }
    } else {
        println!(
            "{}",
            "Drift ratio: not enough data (need both bucket-2 and bucket-8 samples).".dimmed()
        );
    }

    Ok(())
}

fn print_bucket_row(bucket: u8, s: &BucketStats) {
    println!(
        "  {:>6}  {:>4}  {:>10}  {:>10}  {:>12}  {:>12}",
        format!("{bucket}pt"),
        s.n,
        format!("${:.2}", s.cost_p50),
        format!("${:.2}", s.cost_p90),
        format_tokens(s.tokens_p50),
        format_tokens(s.tokens_p90),
    );
}

fn format_tokens(t: f64) -> String {
    if t >= 1_000_000.0 {
        format!("{:.2}M", t / 1_000_000.0)
    } else if t >= 1_000.0 {
        format!("{:.1}K", t / 1_000.0)
    } else {
        format!("{t:.0}")
    }
}
