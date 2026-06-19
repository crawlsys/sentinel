//! `sentinel roi scan` — join SEN-7 + SEN-13 outputs to compute ROI vs
//! a fully-loaded human-team baseline. See
//! `sentinel-application::roi`.

use anyhow::{Context, Result};
use colored::Colorize;

use sentinel_application::roi::{
    human_baseline_per_day, human_baseline_per_point, scan_roi, RoiWindow,
};

pub async fn run() -> Result<()> {
    let metrics_dir = sentinel_infrastructure::paths::sentinel_root().join("metrics");
    let tokens_input = metrics_dir.join("tokens-per-ticket.jsonl");
    let cost_per_point_summary = metrics_dir.join("cost-per-point-summary.json");
    let output_jsonl = metrics_dir.join("roi.jsonl");
    let output_summary = metrics_dir.join("roi-summary.json");
    let graph_runs = output_summary.with_extension("graph-runs.jsonl");

    println!("{}", "Sentinel ROI Scan (SEN-15)".bold());
    println!("Tokens input:        {}", tokens_input.display());
    println!("Cost-per-point in:   {}", cost_per_point_summary.display());
    println!("Output JSONL:        {}", output_jsonl.display());
    println!("Output summary:      {}", output_summary.display());
    println!("Graph audit:         {}", graph_runs.display());
    println!();
    println!(
        "Human baseline:      ${:.0}/day fully-loaded, ${:.0}/point",
        human_baseline_per_day(),
        human_baseline_per_point(),
    );
    println!();

    let report = scan_roi(
        &tokens_input,
        &cost_per_point_summary,
        &output_jsonl,
        &output_summary,
    )
    .context("scan_roi failed")?;
    let graph_audit = crate::roi_graph::run_roi_graph_audit(&report, &graph_runs)
        .await
        .context("roi graph audit failed")?;

    let Some(headline) = report.headline.as_ref() else {
        println!(
            "Graph decision:      {} ({})",
            graph_audit.decision.bold(),
            graph_audit
                .authorization_checkpoint
                .as_deref()
                .expect("roi graph audit requires checkpoint")
                .dimmed()
        );
        println!();
        println!(
            "{}",
            "No SEN-7 input found at tokens-per-ticket.jsonl. Run `sentinel tokens scan` first."
                .yellow()
        );
        return Ok(());
    };

    // Top-line big number.
    let ratio_str = format!("{:.1}x", headline.roi_ratio);
    println!("{}", "==== HEADLINE ROI ====".bold());
    println!(
        "  graph decision {} · {}",
        graph_audit.decision.bold(),
        graph_audit
            .authorization_checkpoint
            .as_deref()
            .expect("roi graph audit requires checkpoint")
            .dimmed()
    );
    println!(
        "  {}  {}",
        "Claude vs Human team:".bold(),
        ratio_str.green().bold()
    );
    println!(
        "  Claude spend:     {}",
        format!("${:.2}", headline.claude_cost_usd_total).cyan()
    );
    println!(
        "  Human equivalent: {}",
        format!("${:.2}", headline.human_cost_usd_total).cyan()
    );
    println!(
        "  Tickets shipped:  {}",
        headline.tickets_shipped_total.to_string().cyan()
    );
    if headline.projected_annual_savings_usd > 0.0 {
        println!(
            "  Projected annual savings: {}",
            format!("${:.0}", headline.projected_annual_savings_usd)
                .green()
                .bold()
        );
    } else {
        println!(
            "  Projected annual savings: {}",
            "(insufficient timeline data)".dimmed()
        );
    }
    if !headline.estimate_data_available && headline.tickets_shipped_total > 0 {
        println!("  {} {}", "Note:".yellow(), headline.estimate_note.dimmed());
    }
    println!();

    println!("{}", "Per-window breakdown:".bold());
    println!(
        "  {:>10}  {:>8}  {:>8}  {:>14}  {:>14}  {:>10}  {:>16}",
        "Window".dimmed(),
        "Tickets".dimmed(),
        "Points".dimmed(),
        "Claude $".dimmed(),
        "Human $".dimmed(),
        "ROI".dimmed(),
        "Annual savings".dimmed(),
    );
    for w in &report.windows {
        print_window_row(w);
    }
    println!();

    if !headline.estimate_data_available && headline.tickets_shipped_total > 0 {
        println!(
            "{}",
            "Tip: SEN-13 estimate data is required before ROI can be calculated.".dimmed()
        );
        println!(
            "{}",
            "     Add `estimate` fields to ~/.claude/sentinel/linear-assigned*.json then rescan."
                .dimmed()
        );
    }

    Ok(())
}

fn print_window_row(w: &RoiWindow) {
    let savings_str = if w.projected_annual_savings_usd > 0.0 {
        format!("${:.0}", w.projected_annual_savings_usd)
    } else {
        "—".to_string()
    };
    let ratio_str = if w.roi_ratio > 0.0 {
        format!("{:.1}x", w.roi_ratio)
    } else {
        "—".to_string()
    };
    let points_str = if w.points_shipped > 0.0 {
        format!("{:.1}", w.points_shipped)
    } else {
        "—".to_string()
    };
    println!(
        "  {:>10}  {:>8}  {:>8}  {:>14}  {:>14}  {:>10}  {:>16}",
        w.label,
        w.tickets_shipped,
        points_str,
        format!("${:.2}", w.claude_cost_usd),
        format!("${:.2}", w.human_cost_usd),
        ratio_str,
        savings_str,
    );
}
