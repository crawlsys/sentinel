//! `sentinel sla` — SLA breach detection commands (SEN-12).
//!
//! Three actions:
//!
//!   * `check --config <path> --subjects <jsonl>` — apply every rule in
//!     `config` against every subject in `subjects`, append breaches to
//!     `~/.claude/sentinel/metrics/sla-breaches.jsonl`, print a summary.
//!   * `aggregate` — read the breach JSONL, write `sla-breaches-summary.json`,
//!     print rolling 24h / 7d / 30d counts per SLA.
//!   * `template` — print a starter `slas.toml` to stdout so operators can
//!     seed `~/.claude/sentinel/config/slas.toml`.

use anyhow::{Context, Result};
use chrono::Utc;
use colored::Colorize;
use sentinel_application::sla::{
    aggregate, append_breach, check_rules, load_config, BreachRecord, Subject,
};
use std::fs;
use std::path::{Path, PathBuf};

const TEMPLATE_TOML: &str = include_str!("../templates/slas.example.toml");

fn metrics_dir() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    Ok(home.join(".claude").join("sentinel").join("metrics"))
}

/// Run `sentinel sla check`.
///
/// `subjects_path` is a JSONL of [`Subject`] records; one breach is appended
/// for every (rule, subject) match. The breach JSONL is written under
/// `~/.claude/sentinel/metrics/sla-breaches.jsonl` unless `--dry-run` is set.
///
/// # Errors
/// Returns config load, subject read, or breach append errors.
pub fn run_check(config_path: PathBuf, subjects_path: PathBuf, dry_run: bool) -> Result<()> {
    let cfg = load_config(&config_path).context("load sla config")?;
    if cfg.rules.is_empty() {
        println!(
            "{}",
            "No SLA rules configured. Run `sentinel sla template` for a starter.".yellow()
        );
        return Ok(());
    }

    let subjects = read_subjects(&subjects_path)?;
    let now = Utc::now();
    let breaches = check_rules(&cfg, &subjects, now);

    println!("{}", "Sentinel SLA Check".bold());
    println!("  Config:   {}", config_path.display());
    println!("  Subjects: {}", subjects_path.display());
    println!("  Rules:    {}", cfg.rules.len());
    println!("  Subjects: {}", subjects.len());
    println!("  Breaches: {}", breaches.len().to_string().red());
    println!();

    if breaches.is_empty() {
        println!("{}", "  ✓ no breaches detected".green());
        return Ok(());
    }

    for b in &breaches {
        println!(
            "  {} {} ({}/{} min, overdue {}m): {} [{}]",
            "⚠".red(),
            b.sla.bold(),
            b.actual_minutes,
            b.target_minutes,
            b.overdue_by_minutes.to_string().red(),
            b.subject_id,
            b.subject_kind
        );
    }

    if dry_run {
        println!();
        println!(
            "{}",
            "--dry-run set — not appending to sla-breaches.jsonl".dimmed()
        );
        return Ok(());
    }

    let breaches_path = metrics_dir()?.join("sla-breaches.jsonl");
    for b in &breaches {
        append_breach(&breaches_path, b).context("append breach")?;
    }
    println!();
    println!("  Appended {} breach record(s) to {}", breaches.len(), breaches_path.display());
    Ok(())
}

/// Run `sentinel sla aggregate`. Reads the breach JSONL, writes the
/// summary JSON, prints counts.
///
/// # Errors
/// Returns aggregation errors.
pub fn run_aggregate() -> Result<()> {
    let dir = metrics_dir()?;
    let breaches = dir.join("sla-breaches.jsonl");
    let summary = dir.join("sla-breaches-summary.json");

    println!("{}", "Sentinel SLA Aggregate".bold());
    println!("  Source:  {}", breaches.display());
    println!("  Summary: {}", summary.display());
    println!();

    let s = aggregate(&breaches, &summary).context("aggregate sla breaches")?;
    println!("  Records scanned: {}", s.records_scanned);
    println!("  SLAs with breaches: {}", s.aggregates.len());
    println!();

    if s.aggregates.is_empty() {
        println!(
            "{}",
            "  No breaches recorded yet. Use `sentinel sla check` to populate.".yellow()
        );
        return Ok(());
    }

    println!(
        "  {:<40}  {:>6}  {:>6}  {:>6}",
        "SLA", "24h", "7d", "30d"
    );
    let mut sorted = s.aggregates;
    sorted.sort_by(|a, b| b.breaches_30d.cmp(&a.breaches_30d));
    for a in &sorted {
        let label = if a.sla.len() > 40 {
            format!("{}…", &a.sla[..39])
        } else {
            a.sla.clone()
        };
        let h24 = colorize_count(a.breaches_24h);
        let d7 = colorize_count(a.breaches_7d);
        let d30 = colorize_count(a.breaches_30d);
        println!("  {label:<40}  {h24:>15}  {d7:>15}  {d30:>15}");
    }
    Ok(())
}

/// Print the starter slas.toml template.
pub fn run_template() {
    print!("{TEMPLATE_TOML}");
}

fn read_subjects(path: &Path) -> Result<Vec<Subject>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut out = Vec::new();
    for (n, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let s: Subject = serde_json::from_str(line)
            .with_context(|| format!("parse subject line {} of {}", n + 1, path.display()))?;
        out.push(s);
    }
    Ok(out)
}

fn colorize_count(n: u64) -> colored::ColoredString {
    let s = n.to_string();
    if n == 0 {
        s.green()
    } else if n < 5 {
        s.yellow()
    } else {
        s.red()
    }
}

#[allow(dead_code)]
fn _ensure_record_in_scope(_b: &BreachRecord) {}
