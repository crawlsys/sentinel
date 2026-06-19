//! `sentinel linear-code-audit scan` — cross-check every Completed ticket in
//! the Linear cache against a precomputed code-evidence map, flagging
//! `done-no-evidence` false-dones. See
//! `sentinel-application::linear_code_audit`.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::PathBuf;

use sentinel_application::linear_code_audit::scan_code_audit;

pub async fn run() -> Result<()> {
    let sentinel_dir: PathBuf = sentinel_infrastructure::paths::sentinel_root();
    let linear_cache = sentinel_dir.join("linear-assigned.json");
    let evidence_map = sentinel_dir.join("ticket-code-evidence.json");
    let output_summary = sentinel_dir.join("metrics").join("linear-code-audit.json");
    let graph_runs = output_summary.with_extension("reconciliation-graph-runs.jsonl");

    println!("{}", "Sentinel Linear Code Audit".bold());
    println!("Linear cache:   {}", linear_cache.display());
    println!("Evidence map:   {}", evidence_map.display());
    println!("Output summary: {}", output_summary.display());
    println!("Graph audit:    {}", graph_runs.display());
    println!();

    let summary = scan_code_audit(&linear_cache, &evidence_map, &output_summary)
        .context("scan_code_audit failed")?;
    let reconciliation_audit =
        crate::code_reconciliation_audit::run_code_reconciliation_graph_audit(
            &summary.flags,
            &graph_runs,
        )
        .await
        .context("reconciliation graph audit failed")?;

    if summary.completed_total == 0 {
        println!(
            "{}",
            "No Completed tickets found in linear-assigned.json. Populate the cache \
             (the portfolio-health cron writes it) and rescan."
                .yellow()
        );
        return Ok(());
    }

    println!("{}", "==== CODE AUDIT ====".bold());
    println!(
        "  {} completed · {} with evidence · {} without evidence",
        summary.completed_total, summary.with_evidence, summary.without_evidence
    );
    println!(
        "  {} reconciliation graph run(s) · {} authorized flag(s)",
        reconciliation_audit.flags_audited, reconciliation_audit.authorized_flags
    );
    println!();

    if summary.flags.is_empty() {
        println!(
            "{}",
            "✓ Every Completed ticket has code evidence (commits or touched files).".green()
        );
        return Ok(());
    }

    println!("{}", "Done-but-no-code (possible false-done):".red().bold());
    for f in &summary.flags {
        println!(
            "  {} [{}]  {} commits · {} files",
            f.identifier.bold(),
            f.state.dimmed(),
            f.commits,
            f.files,
        );
    }
    println!();
    println!(
        "{}",
        format!(
            "⚠ {} Completed ticket(s) shipped no detectable code — verify they aren't false-dones.",
            summary.without_evidence
        )
        .red()
        .bold()
    );

    Ok(())
}
