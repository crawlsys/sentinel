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
/// - `--dir <path>`: override marketplace root. When omitted, the root is
///   auto-detected by walking up from the current directory for a marketplace
///   marker (`.claude-plugin/marketplace.json` or a root `marketplace.json`
///   alongside a `skills/` dir); if none is found, falls back to `~/.claude/`.
pub fn run(
    counts_only: bool,
    validate_only: bool,
    sync_counts: bool,
    manifest: bool,
    dry_run: bool,
    dir: Option<String>,
) -> anyhow::Result<()> {
    let root_dir = match dir {
        Some(d) => PathBuf::from(d),
        None => detect_marketplace_root().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".claude")
        }),
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
            "  scripts={}, docs={}, templates={}, browserbase_tools={}",
            ext.scripts, ext.docs, ext.templates, ext.browserbase_tools,
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
                v.warned.to_string()
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
        eprintln!("{} Validation: all {} checks passed", "✓".green(), v.passed);
    }

    Ok(())
}

/// Detect a marketplace root by walking up from the current working directory.
///
/// A directory qualifies as a marketplace root when it contains either:
/// - `.claude-plugin/marketplace.json` (the plugin-marketplace manifest), or
/// - a `marketplace.json` alongside a `skills/` directory (the repo layout).
///
/// Returns `None` if no marker is found before reaching the filesystem root, so
/// the caller can fall back to `~/.claude/`. This makes `sentinel scan` operate
/// on the repo it is invoked in rather than always rewriting `~/.claude/`.
fn detect_marketplace_root() -> Option<PathBuf> {
    let start = std::env::current_dir().ok()?;
    find_marketplace_root(&start)
}

/// Walk up from `start` looking for a marketplace marker. Pure (no cwd access)
/// so it is unit-testable. Returns the first ancestor (incl. `start`) that is a
/// marketplace root, or `None` at the filesystem root.
fn find_marketplace_root(start: &std::path::Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let is_marketplace = dir.join(".claude-plugin/marketplace.json").is_file()
            || (dir.join("marketplace.json").is_file() && dir.join("skills").is_dir());
        if is_marketplace {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::find_marketplace_root;
    use std::fs;

    #[test]
    fn detects_claude_plugin_marker_and_walks_up_from_subdir() {
        let tmp = std::env::temp_dir().join(format!("sen-mkt-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".claude-plugin")).unwrap();
        fs::write(tmp.join(".claude-plugin/marketplace.json"), "{}").unwrap();
        let sub = tmp.join("skills").join("foo");
        fs::create_dir_all(&sub).unwrap();

        // From the root itself and from a nested subdir, both resolve to root.
        assert_eq!(find_marketplace_root(&tmp).as_deref(), Some(tmp.as_path()));
        assert_eq!(find_marketplace_root(&sub).as_deref(), Some(tmp.as_path()));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn no_marker_returns_none() {
        let tmp = std::env::temp_dir().join(format!("sen-nomkt-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        // A bare temp dir has no marketplace marker up its (temp) ancestry.
        assert_eq!(find_marketplace_root(&tmp), None);
        let _ = fs::remove_dir_all(&tmp);
    }
}
