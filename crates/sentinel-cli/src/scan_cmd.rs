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
/// - `--sync-counts`: synchronize counts across all marketplace text files
/// - `--manifest`: generate manifest.json with SHA-256 hashes
/// - `--dry-run`: preview changes without writing (for --sync-counts)
/// - `--dir <path>`: override marketplace root (default: `~/.claude/`)
pub async fn run(
    counts_only: bool,
    validate_only: bool,
    sync_counts: bool,
    manifest: bool,
    dry_run: bool,
    dir: Option<String>,
) -> anyhow::Result<()> {
    let root_dir = match dir {
        Some(d) => PathBuf::from(d),
        None => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude"),
    };

    if !root_dir.exists() {
        anyhow::bail!("Marketplace directory not found: {}", root_dir.display());
    }

    // --sync-counts mode
    if sync_counts {
        let ext = scanner::count_extended(&root_dir);
        eprintln!("{} Filesystem counts:", "▶".blue());
        eprintln!(
            "  skills={}, hooks={}, commands={}, agents={}, mcp_servers={}, mcp_repos={}, cli_repos={}",
            ext.core.skills, ext.core.hooks, ext.core.commands, ext.core.agents,
            ext.core.mcp_servers, ext.core.mcp_repos, ext.core.cli_repos,
        );
        eprintln!(
            "  scripts={}, docs={}, templates={}, steel_tools={}",
            ext.scripts, ext.docs, ext.templates, ext.steel_tools,
        );

        let report = scanner::sync_counts(&root_dir, dry_run);

        if report.files_changed.is_empty() {
            eprintln!("\n{} All counts up to date.", "✓".green());
        } else {
            let verb = if dry_run { "Would update" } else { "Updated" };
            eprintln!(
                "\n{} {} {} file(s):",
                if dry_run { "▶".blue() } else { "✓".green() },
                verb,
                report.files_changed.len(),
            );
            for f in &report.files_changed {
                eprintln!("  - {f}");
            }
        }

        // Also output JSON to stdout
        let json = serde_json::to_string_pretty(&report)?;
        println!("{json}");
        return Ok(());
    }

    // --manifest mode
    if manifest {
        let result = scanner::generate_manifest(&root_dir);
        eprintln!(
            "{} Generated manifest.json: {} files",
            "✓".green(),
            result.file_count,
        );

        // Summary by directory
        let mut by_dir: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for f in &result.files {
            let top = f.path.split('/').next().unwrap_or("?").to_string();
            *by_dir.entry(top).or_insert(0) += 1;
        }
        for (dir, count) in &by_dir {
            eprintln!("  {dir}: {count}");
        }

        // Output JSON to stdout
        let json = serde_json::to_string_pretty(&result)?;
        println!("{json}");
        return Ok(());
    }

    // --counts-only mode
    if counts_only {
        let counts = scanner::count_components(&root_dir);
        let json = serde_json::to_string_pretty(&counts)?;
        println!("{json}");
        return Ok(());
    }

    let snapshot = scanner::scan_marketplace(&root_dir);

    // --validate mode
    if validate_only {
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

        let json = serde_json::to_string_pretty(&snapshot.validation)?;
        println!("{json}");
        return Ok(());
    }

    // Full snapshot (default)
    let json = serde_json::to_string_pretty(&snapshot)?;
    println!("{json}");

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
        eprintln!("{} Validation: all {} checks passed", "✓".green(), v.passed,);
    }

    Ok(())
}
