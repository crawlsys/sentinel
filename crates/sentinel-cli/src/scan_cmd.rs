//! `sentinel scan` — Marketplace scanner CLI
//!
//! Outputs a full marketplace snapshot as JSON to stdout.
//! Replaces `dashboard/server/scanner.cjs`.

use std::path::PathBuf;

use colored::Colorize;
use sentinel_application::scanner;

/// Run the scan command.
///
/// - Default: full marketplace snapshot as JSON
/// - `--counts-only`: just component counts
/// - `--validate`: just validation results
/// - `--dir <path>`: override marketplace root (default: `~/.claude/`)
pub async fn run(counts_only: bool, validate_only: bool, dir: Option<String>) -> anyhow::Result<()> {
    let root_dir = match dir {
        Some(d) => PathBuf::from(d),
        None => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude"),
    };

    if !root_dir.exists() {
        anyhow::bail!("Marketplace directory not found: {}", root_dir.display());
    }

    if counts_only {
        let counts = scanner::count_components(&root_dir);
        let json = serde_json::to_string_pretty(&counts)?;
        println!("{json}");
        return Ok(());
    }

    let snapshot = scanner::scan_marketplace(&root_dir);

    if validate_only {
        // Print validation report with colored output
        let v = &snapshot.validation;
        eprintln!(
            "{} Validation: {} passed, {} failed, {} warned ({}ms)",
            "▶".blue(),
            v.passed.to_string().green(),
            if v.failed > 0 {
                v.failed.to_string().red().to_string()
            } else {
                v.failed.to_string().green().to_string()
            },
            if v.warned > 0 {
                v.warned.to_string().yellow().to_string()
            } else {
                v.warned.to_string().to_string()
            },
            v.duration_ms,
        );

        for r in &v.results {
            let icon = match r.status.as_str() {
                "pass" => "✓".green(),
                "fail" => "✗".red(),
                "warn" => "!".yellow(),
                _ => "?".normal(),
            };
            eprintln!("  {icon} [{}] {} — {}", r.category, r.rule, r.message);
        }

        // Also output JSON to stdout for machine consumption
        let json = serde_json::to_string_pretty(&snapshot.validation)?;
        println!("{json}");
        return Ok(());
    }

    // Full snapshot
    let json = serde_json::to_string_pretty(&snapshot)?;
    println!("{json}");

    // Summary to stderr
    eprintln!(
        "{} Marketplace: {} skills, {} hooks, {} agents, {} commands, {} MCP servers ({} repos), {} CLIs",
        "▶".blue(),
        snapshot.counts.skills.to_string().green(),
        snapshot.counts.hooks.to_string().green(),
        snapshot.counts.agents.to_string().green(),
        snapshot.counts.commands.to_string().green(),
        snapshot.counts.mcp_servers.to_string().green(),
        snapshot.counts.mcp_repos,
        snapshot.counts.cli_repos,
    );

    let v = &snapshot.validation;
    if v.failed > 0 {
        eprintln!(
            "{} Validation: {} issues found",
            "⚠".yellow(),
            v.failed.to_string().red(),
        );
    } else {
        eprintln!(
            "{} Validation: all {} checks passed",
            "✓".green(),
            v.passed,
        );
    }

    Ok(())
}
