//! `sentinel linear-audit scan` — run the Linear PM-enforcement audit
//! (estimate hygiene, oversized tickets, QA-failed, velocity burndown,
//! estimate-vs-actual calibration) over the Linear issue cache. See
//! `sentinel-application::linear_pm_audit`.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::PathBuf;

use sentinel_application::linear_pm_audit::{scan_pm_audit, BurndownInputs};

/// `velocity` (pts/week) and `weeks` (until target date) enable the
/// burndown projection; both must be supplied or it is skipped.
pub fn run(velocity: Option<f64>, weeks: Option<f64>) -> Result<()> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    let sentinel_dir: PathBuf = home.join(".claude").join("sentinel");
    let linear_cache = sentinel_dir.join("linear-assigned.json");
    let output_summary = sentinel_dir
        .join("metrics")
        .join("linear-pm-audit.json");

    println!("{}", "Sentinel Linear PM Audit".bold());
    println!("Linear cache:   {}", linear_cache.display());
    println!("Output summary: {}", output_summary.display());
    println!();

    let summary = scan_pm_audit(
        &linear_cache,
        &output_summary,
        BurndownInputs {
            velocity_pts_per_week: velocity,
            weeks_available: weeks,
        },
    )
    .context("scan_pm_audit failed")?;

    if summary.issues_total == 0 {
        println!(
            "{}",
            "No issues found in linear-assigned.json. Populate the cache (the \
             portfolio-health cron writes it) and rescan."
                .yellow()
        );
        return Ok(());
    }

    println!("{}", "==== PM AUDIT ====".bold());
    println!(
        "  Issues: {} total · {} done · {} open",
        summary.issues_total, summary.issues_done, summary.issues_open
    );
    println!(
        "  Points: {:.0} total · {:.0} done · {:.0} remaining",
        summary.points_total, summary.points_done, summary.points_remaining
    );
    println!();

    // Check 1: hygiene
    print_flag_line(
        "Missing estimate",
        summary.missing_estimate,
        summary.missing_estimate > 0,
    );
    print_flag_line(
        "Non-Fibonacci estimate",
        summary.non_fibonacci,
        summary.non_fibonacci > 0,
    );
    // Check 2 + 3
    print_flag_line(
        "Oversized & open (decompose)",
        summary.oversized_open,
        summary.oversized_open > 0,
    );
    print_flag_line(
        &format!("QA-failed ({:.0} pts at risk)", summary.qa_failed_points),
        summary.qa_failed,
        summary.qa_failed > 0,
    );
    println!();

    // Check 4: burndown
    if let Some(b) = &summary.burndown {
        let verdict = if b.on_track {
            "ON TRACK".green().bold()
        } else {
            "BEHIND".red().bold()
        };
        println!("{}", "Burndown:".bold());
        println!(
            "  {:.0} pts remaining ÷ {:.1} pts/wk = {:.1} weeks needed vs {:.1} available → {verdict}",
            b.remaining_points, b.velocity_points_per_week, b.weeks_needed, b.weeks_available
        );
        println!();
    } else {
        println!(
            "{}",
            "Burndown: skipped (pass --velocity <pts/wk> --weeks <n> to project)".dimmed()
        );
        println!();
    }

    // Check 5: calibration
    if summary.calibration.is_empty() {
        println!(
            "{}",
            "Calibration: no completed tickets with start+complete timestamps in cache".dimmed()
        );
    } else {
        println!("{}", "Estimate-vs-actual calibration (median calendar days):".bold());
        for (bucket, c) in &summary.calibration {
            let flag = if c.low_confidence {
                "  ⚠ low-confidence (thin/coarse sample)".dimmed().to_string()
            } else {
                String::new()
            };
            println!("  {bucket}-pt: {:.1} d  (n={}){flag}", c.median_days, c.n);
        }
    }
    println!();

    if summary.hard_violations {
        println!(
            "{}",
            format!(
                "⚠ {} hard PM violation(s) — these would be blocked by the linear_pm_gate hook.",
                summary.oversized_open + summary.missing_estimate
            )
            .red()
            .bold()
        );
    } else {
        println!("{}", "✓ No hard PM violations.".green());
    }

    Ok(())
}

fn print_flag_line(label: &str, count: usize, bad: bool) {
    let n = if bad {
        count.to_string().red().bold().to_string()
    } else {
        count.to_string().green().to_string()
    };
    println!("  {label:32} {n}");
}
