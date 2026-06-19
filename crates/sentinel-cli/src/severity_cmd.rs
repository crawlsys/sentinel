//! `sentinel severity scan [--apply]` — LLM-judged Linear ticket priority.
//!
//! Reads the Linear issue cache and asks BOTH Opus 4.8 and GPT-5.5 to judge
//! each ticket's severity (1-4), reconciling the two verdicts. Report-only by
//! default. See
//! `sentinel-application::severity`.
//!
//! ## The human-confirm rule
//!
//! Report-only by default means mutation is off. Under `--apply`, the scan still
//! only produces proposal rows first; then this command replays proposal rows
//! through the infrastructure severity LangGraph before issuing any
//! `issueUpdate`. The graph only authorizes gap-fill `set` actions; `suggest`
//! actions remain report-only and require human review.

use anyhow::{Context, Result};
use colored::Colorize;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use sentinel_application::severity::{scan_severity, SeverityProposal};
use sentinel_infrastructure::decision_graph_introspection::terminal_decision_checkpoint_result;
use sentinel_infrastructure::openrouter_llm::OpenRouterLlm;
use sentinel_infrastructure::severity_graph::{
    apply_severity_proposal, build_severity_mutation_graph,
};

use crate::severity_graph_audit::{
    audit_severity_proposals, load_severity_proposals, severity_graph_row,
};

/// `--apply` arms graph-backed gap-fill (`set`) mutations; suggestions are
/// never auto-posted from the CLI.
pub async fn run(apply: bool) -> Result<()> {
    let sentinel_dir: PathBuf = sentinel_infrastructure::paths::sentinel_root();
    let linear_cache = sentinel_dir.join("linear-assigned.json");
    let output = sentinel_dir.join("metrics").join("severity.json");
    let proposals_jsonl = output.with_extension("jsonl");
    let graph_runs = output.with_extension("graph-runs.jsonl");

    println!("{}", "Sentinel Auto-Severity".bold());
    println!("Linear cache:   {}", linear_cache.display());
    println!("Output summary: {}", output.display());
    println!("Graph audit:    {}", graph_runs.display());
    let mode = if apply {
        "APPLY (graph-backed gap-fills set; suggestions report-only)"
            .yellow()
            .bold()
    } else {
        "REPORT (read-only, mutates nothing)".green().bold()
    };
    println!("Mode:           {mode}");
    println!();

    // Auto-severity is model-authoritative; do not silently downgrade to a no-op.
    let llm = OpenRouterLlm::from_env().context(
        "OPENROUTER_API_KEY is required for auto-severity; refusing to run without model authority",
    )?;

    // The Linear token is only needed for the graph-backed apply path.
    let linear_token = if apply {
        Some(std::env::var("SENTINEL_LINEAR_TOKEN").context(
            "--apply requires SENTINEL_LINEAR_TOKEN; refusing to downgrade to a read-only scan",
        )?)
    } else {
        None
    };

    let mut summary = scan_severity(&linear_cache, &output, &llm)
        .await
        .context("scan_severity failed")?;

    let proposals = load_severity_proposals(&proposals_jsonl)?;
    if apply {
        if let Some(token) = linear_token.as_deref() {
            let apply_audit = apply_gap_fills(&proposals, &graph_runs, token).await?;
            summary.applied = apply_audit.applied;
            summary.report_only = apply_audit.applied == 0;
            std::fs::write(&output, serde_json::to_vec_pretty(&summary)?)
                .with_context(|| format!("write updated severity summary {}", output.display()))?;
            println!(
                "Graph decisions: {} set-authorized, {} skipped",
                apply_audit.authorized_sets.to_string().green().bold(),
                apply_audit.skipped.to_string().dimmed()
            );
        }
    } else {
        let graph_audit = audit_severity_proposals(&proposals, &graph_runs).await?;
        println!(
            "Graph decisions: {} set-authorized, {} skipped",
            graph_audit.authorized_sets.to_string().yellow().bold(),
            graph_audit.skipped.to_string().dimmed()
        );
    }

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
    print_proposals(&proposals_jsonl);

    println!();
    println!("{}", "==== AUTO-SEVERITY ====".bold());
    println!("  Tickets scanned: {}", summary.tickets_scanned);
    print_count_line("Would SET (gap-fill, no priority)", summary.would_set);
    print_count_line("Would SUGGEST (priority exists)", summary.would_suggest);
    print_count_line("Model disagreements (Opus != GPT)", summary.disagreements);
    println!();

    if apply && linear_token.is_some() {
        println!(
            "  {} graph-backed mutation(s) applied (gap-fill `set` only).",
            summary.applied.to_string().green().bold()
        );
        if summary.applied == 0 {
            println!(
                "{}",
                "No graph-authorized Linear priority mutations were needed.".dimmed()
            );
        }
        println!(
            "{}",
            "Note: SUGGEST actions (tickets that already have a priority) were NOT auto-posted. \
             They require human review before any priority change — confirm and post them via \
             the in-session MCP path."
                .yellow()
        );
    } else if summary.report_only {
        println!(
            "{}",
            "Report-only run: no Linear mutations performed. Re-run with --apply to gap-fill \
             untriaged tickets."
                .dimmed()
        );
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct SeverityApplyAudit {
    applied: usize,
    authorized_sets: usize,
    skipped: usize,
}

async fn apply_gap_fills(
    proposals: &[SeverityProposal],
    graph_jsonl: &std::path::Path,
    token: &str,
) -> Result<SeverityApplyAudit> {
    let graph = build_severity_mutation_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build severity mutation graph: {e}"))?;
    let client = reqwest::Client::new();
    let mut applied = 0usize;
    let mut authorized_sets = 0usize;
    let mut skipped = 0usize;
    let graph_file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create severity graph audit {}", graph_jsonl.display()))?;
    let mut graph_writer = BufWriter::new(graph_file);
    for proposal in proposals {
        let result = apply_severity_proposal(&client, token, &graph, &proposal)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "severity graph apply failed for {}: {e}",
                    proposal.identifier
                )
            })?;
        let authorization_checkpoint = result
            .authorization
            .as_ref()
            .map(|authorization| authorization.checkpoint_ref());
        let terminal_checkpoint = terminal_decision_checkpoint_result(
            "severity",
            &result.run.thread_id,
            &result.run.state,
            &result.run.checkpoints,
            &result.run.write_history,
        )
        .map_err(|e| anyhow::anyhow!("severity graph terminal checkpoint failed: {e}"))?
        .checkpoint_ref();
        let decision = format!("{:?}", result.run.state.decision).to_ascii_lowercase();
        let thread_id = result.run.thread_id.clone();
        if authorization_checkpoint.is_some() {
            authorized_sets += 1;
        } else {
            skipped += 1;
        }
        let audit_row = severity_graph_row(
            &proposal.identifier,
            &decision,
            &terminal_checkpoint,
            authorization_checkpoint.as_deref(),
            &thread_id,
            Some(result.applied),
            serde_json::to_value(&result.run)?,
        );
        serde_json::to_writer(&mut graph_writer, &audit_row).with_context(|| {
            format!(
                "write severity graph audit row for {} to {}",
                proposal.identifier,
                graph_jsonl.display()
            )
        })?;
        graph_writer.write_all(b"\n").with_context(|| {
            format!(
                "terminate severity graph audit row for {} in {}",
                proposal.identifier,
                graph_jsonl.display()
            )
        })?;
        applied += usize::from(result.applied);
        if result.applied {
            let authorization = result.authorization.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "severity graph applied {} without checkpoint authorization",
                    proposal.identifier
                )
            })?;
            println!(
                "  {} {} via {}",
                "SET".green().bold(),
                proposal.identifier,
                authorization.checkpoint_ref()
            );
        }
    }
    graph_writer
        .flush()
        .with_context(|| format!("flush severity graph audit {}", graph_jsonl.display()))?;
    Ok(SeverityApplyAudit {
        applied,
        authorized_sets,
        skipped,
    })
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
        let id = v
            .get("identifier")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let proposed = v
            .get("proposed_priority")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let action = v
            .get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let agreed = v
            .get("models_agreed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let action_disp = match action {
            "set" => action.green().to_string(),
            "suggest" => action.yellow().to_string(),
            _ => action.dimmed().to_string(),
        };
        let flag = if agreed {
            String::new()
        } else {
            "  ⚠ models disagreed".dimmed().to_string()
        };
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
