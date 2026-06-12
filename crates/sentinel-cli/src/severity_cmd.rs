//! `sentinel severity scan [--apply]` — LLM-judged Linear ticket priority.
//!
//! Reads the Linear issue cache and asks BOTH Opus 4.8 and GPT-5.5 to judge
//! each ticket's severity (1-4), reconciling the two verdicts. Shadow by
//! default (read-only, mutates nothing). See
//! `sentinel-application::severity`.
//!
//! ## The human-confirm rule
//!
//! Shadow-by-default means the `set` path (gap-fill a ticket with NO priority)
//! only runs under `--apply`. For a ticket that ALREADY has a priority the spec
//! wants a human to confirm BEFORE a suggestion is posted — but a CLI can't ask
//! an interactive question safely in this harness. So in `--apply` mode this
//! command applies ONLY the `set` actions (untriaged gap-fills) automatically;
//! the `suggest` actions (priority already set) are written to the report and
//! NOT auto-posted. A clear note tells the operator that suggestions need
//! manual review — the in-session MCP path / a human confirms and posts them.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::PathBuf;

use sentinel_application::severity::scan_severity;
use sentinel_infrastructure::openrouter_llm::OpenRouterLlm;

/// `--apply` arms the gap-fill (`set`) mutations; suggestions are never
/// auto-posted from the CLI (they require human confirmation — see module doc).
pub async fn run(apply: bool) -> Result<()> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    let sentinel_dir: PathBuf = home.join(".claude").join("sentinel");
    let linear_cache = sentinel_dir.join("linear-assigned.json");
    let output = sentinel_dir.join("metrics").join("severity.json");

    println!("{}", "Sentinel Auto-Severity".bold());
    println!("Linear cache:   {}", linear_cache.display());
    println!("Output summary: {}", output.display());
    let mode = if apply {
        "APPLY (gap-fills set; suggestions report-only)".yellow().bold()
    } else {
        "SHADOW (read-only — mutates nothing)".green().bold()
    };
    println!("Mode:           {mode}");
    println!();

    // Build the OpenRouter LLM. Missing key is a graceful no-op, not a crash.
    let Ok(llm) = OpenRouterLlm::from_env() else {
        println!(
            "{}",
            "OPENROUTER_API_KEY is not set — auto-severity needs it to call Opus 4.8 + \
             GPT-5.5. Set the key and re-run. (No tickets were scanned.)"
                .yellow()
        );
        return Ok(());
    };

    // The Linear token is only needed for the apply path. Suggestions are never
    // auto-posted from the CLI, so we pass the token only when applying so the
    // module's `set` mutations can run; the `suggest` rows stay report-only
    // because we surface them without the CLI confirming them (the module posts
    // a suggestion only when invoked directly with apply+token — the in-session
    // MCP/human path owns that confirmation, not this command).
    let linear_token = std::env::var("SENTINEL_LINEAR_TOKEN").ok();
    if apply && linear_token.is_none() {
        println!(
            "{}",
            "--apply was set but SENTINEL_LINEAR_TOKEN is not configured — nothing can be \
             written. Falling back to a shadow report."
                .yellow()
        );
    }

    // In apply mode we pass the token so `set` (gap-fill) mutations fire. The
    // command intentionally does NOT confirm-and-post `suggest` rows — those are
    // reported for manual review per the human-confirm rule.
    let token_for_scan = if apply { linear_token.as_deref() } else { None };

    let summary = scan_severity(&linear_cache, &output, &llm, apply, token_for_scan)
        .await
        .context("scan_severity failed")?;

    if summary.tickets_scanned == 0 {
        println!(
            "{}",
            "No issues found in linear-assigned.json. Populate the cache (the \
             portfolio-health cron writes it) and rescan."
                .yellow()
        );
        return Ok(());
    }

    // Per-ticket report from the JSONL the scan wrote.
    print_proposals(&output.with_extension("jsonl"));

    println!();
    println!("{}", "==== AUTO-SEVERITY ====".bold());
    println!("  Tickets scanned: {}", summary.tickets_scanned);
    print_count_line("Would SET (gap-fill, no priority)", summary.would_set);
    print_count_line("Would SUGGEST (priority exists)", summary.would_suggest);
    print_count_line("Model disagreements (Opus != GPT)", summary.disagreements);
    println!();

    if summary.shadow {
        println!(
            "{}",
            "Shadow run — no Linear mutations performed. Re-run with --apply to gap-fill \
             untriaged tickets."
                .dimmed()
        );
    } else {
        println!(
            "  {} mutation(s) applied (gap-fill `set` only).",
            summary.applied.to_string().green().bold()
        );
        println!(
            "{}",
            "Note: SUGGEST actions (tickets that already have a priority) were NOT auto-posted. \
             They require human review before any priority change — confirm and post them via \
             the in-session MCP path."
                .yellow()
        );
    }

    Ok(())
}

/// Stream the JSONL proposal rows into a compact per-ticket report.
fn print_proposals(jsonl: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(jsonl) else {
        return;
    };
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let id = v.get("identifier").and_then(serde_json::Value::as_str).unwrap_or("?");
        let proposed = v.get("proposed_priority").and_then(serde_json::Value::as_i64).unwrap_or(0);
        let action = v.get("action").and_then(serde_json::Value::as_str).unwrap_or("?");
        let agreed = v.get("models_agreed").and_then(serde_json::Value::as_bool).unwrap_or(true);
        let action_disp = match action {
            "set" => action.green().to_string(),
            "suggest" => action.yellow().to_string(),
            _ => action.dimmed().to_string(),
        };
        let flag = if agreed { String::new() } else { "  ⚠ models disagreed".dimmed().to_string() };
        println!("  {id:14} P{proposed}  [{action_disp}]{flag}");
    }
}

fn print_count_line(label: &str, count: usize) {
    let n = if count > 0 {
        count.to_string().yellow().bold().to_string()
    } else {
        count.to_string().green().to_string()
    };
    println!("  {label:36} {n}");
}
