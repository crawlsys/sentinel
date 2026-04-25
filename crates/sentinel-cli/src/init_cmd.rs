//! `sentinel init` — Generate standard project files
//!
//! Audits a repo (or all repos in batch mode) for missing standard files
//! and generates them from templates.

use std::path::PathBuf;

use colored::Colorize;
use sentinel_application::project_init;
use sentinel_domain::project::StandardFile;

/// Run the init command.
pub async fn run(dry_run: bool, force: bool, all: bool, dir: Option<String>) -> anyhow::Result<()> {
    if all {
        run_batch(dry_run, force)
    } else {
        let repo = match dir {
            Some(d) => PathBuf::from(d),
            None => std::env::current_dir()?,
        };
        run_single(&repo, dry_run, force)
    }
}

fn run_single(repo: &PathBuf, dry_run: bool, force: bool) -> anyhow::Result<()> {
    let audit = project_init::audit(repo);

    let dir_name = repo
        .file_name().map_or_else(|| repo.display().to_string(), |n| n.to_string_lossy().to_string());

    eprintln!(
        "{} {} — Project Standards Audit",
        "sentinel init".bold(),
        if dry_run { "(dry run)" } else { "" }
    );
    eprintln!("{}", "=".repeat(50));
    eprintln!("Repository: {}", dir_name.cyan());
    eprintln!(
        "Type: {} ({})",
        audit.metadata.project_type,
        audit
            .metadata
            .rust_flavor
            .as_ref().map_or_else(|| "-".into(), std::string::ToString::to_string)
    );
    eprintln!("Name: {}", audit.metadata.name);
    eprintln!();

    if audit.missing.is_empty() {
        eprintln!("  {} All standard files present.", "✓".green());
        return Ok(());
    }

    if dry_run {
        // Preview mode — just show what would happen
        for file in &audit.existing {
            eprintln!("  {} {} (exists)", "✓".green(), file);
        }
        for file in &audit.missing {
            eprintln!("  {} {} (would create)", "+".yellow(), file);
        }
        eprintln!();
        eprintln!(
            "Would create {} file(s), {} existing.",
            audit.missing.len().to_string().yellow(),
            audit.existing.len().to_string().green(),
        );
    } else {
        // Actually create files
        let result = project_init::init_repo(repo, force);

        for file in &result.skipped {
            eprintln!("  {} {} (exists, skipped)", "✓".green(), file);
        }
        for file in &result.created {
            eprintln!("  {} {} (created)", "+".green(), file);
        }
        for (file, err) in &result.errors {
            eprintln!("  {} {} (error: {})", "✗".red(), file, err);
        }
        eprintln!();
        eprintln!(
            "Created {} file(s), skipped {} existing.",
            result.created.len().to_string().green(),
            result.skipped.len().to_string().blue(),
        );
        if !result.errors.is_empty() {
            eprintln!("{} {} error(s).", "⚠".yellow(), result.errors.len());
        }
    }

    Ok(())
}

fn run_batch(dry_run: bool, force: bool) -> anyhow::Result<()> {
    let github_dir = dirs::home_dir()
        .map(|h| h.join("Documents").join("GitHub"))
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let repos = project_init::discover_repos(&github_dir);

    eprintln!(
        "{} {} — Batch Project Standards Audit {}",
        "sentinel init --all".bold(),
        if dry_run { "(dry run)" } else { "" },
        format!("({} repos)", repos.len()).dimmed(),
    );
    eprintln!("{}", "=".repeat(60));
    eprintln!();

    if repos.is_empty() {
        eprintln!("No repos found under ~/Documents/GitHub/");
        return Ok(());
    }

    let mut total_created = 0u32;
    let mut total_skipped = 0u32;
    let mut total_errors = 0u32;
    let mut repos_needing_work = 0u32;

    for repo in &repos {
        let audit = project_init::audit(repo);
        let dir_name = repo
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if audit.missing.is_empty() {
            eprintln!("  {} {} — all files present", "✓".green(), dir_name);
            total_skipped += audit.existing.len() as u32;
            continue;
        }

        repos_needing_work += 1;

        if dry_run {
            let missing_names: Vec<String> =
                audit.missing.iter().map(|f| f.path().to_string()).collect();
            eprintln!(
                "  {} {} — {} missing: {}",
                "+".yellow(),
                dir_name,
                audit.missing.len(),
                missing_names.join(", "),
            );
            total_created += audit.missing.len() as u32;
            total_skipped += audit.existing.len() as u32;
        } else {
            let result = project_init::init_repo(repo, force);
            let created_names: Vec<String> = result
                .created
                .iter()
                .map(|f| f.path().to_string())
                .collect();

            if result.created.is_empty() {
                eprintln!("  {} {} — nothing created", "–".dimmed(), dir_name,);
            } else {
                eprintln!(
                    "  {} {} — created: {}",
                    "+".green(),
                    dir_name,
                    created_names.join(", "),
                );
            }

            total_created += result.created.len() as u32;
            total_skipped += result.skipped.len() as u32;
            total_errors += result.errors.len() as u32;

            for (file, err) in &result.errors {
                eprintln!("    {} {} — {}", "✗".red(), file, err);
            }
        }
    }

    eprintln!();
    eprintln!("{}", "Summary".bold());
    eprintln!("{}", "-".repeat(30));
    eprintln!("Repos scanned:    {}", repos.len());
    eprintln!("Repos needing work: {repos_needing_work}");
    let verb = if dry_run { "Would create" } else { "Created" };
    eprintln!("{}: {}", verb, total_created.to_string().green());
    eprintln!("Skipped:          {total_skipped}");
    if total_errors > 0 {
        eprintln!("Errors:           {}", total_errors.to_string().red());
    }

    Ok(())
}

/// Get the list of standard files for display/reporting.
pub fn _standard_files_for_rust() -> Vec<StandardFile> {
    StandardFile::all_rust()
}
