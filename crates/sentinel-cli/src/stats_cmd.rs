//! `sentinel stats` — Hook execution statistics

use anyhow::Result;
use colored::Colorize;
use std::collections::HashMap;
use std::fs;

pub fn run() -> Result<()> {
    println!("{}", "Sentinel Hook Statistics".bold());
    println!();

    // List sessions with state
    let sessions = sentinel_infrastructure::state_store::list_sessions()?;

    if sessions.is_empty() {
        println!("{}", "No sessions found.".yellow());
        return Ok(());
    }

    println!("Sessions: {}", sessions.len());

    let mut corrupt_count = 0u32;
    for session_id in &sessions {
        let state_result = sentinel_infrastructure::state_store::load(session_id);
        let state_opt = match state_result {
            Ok(s) => s,
            Err(e) => {
                corrupt_count += 1;
                eprintln!("  {} skipping {session_id}: {e:#}", "warning:".yellow());
                continue;
            }
        };
        if let Some(state) = state_opt {
            println!("\n{}", format!("Session: {session_id}").cyan());
            println!("  Hooks invoked: {}", state.hook_stats.total_invocations);
            println!("  Tool calls blocked: {}", state.hook_stats.total_blocked);
            println!(
                "  Active skill: {}",
                state.active_skill.as_deref().unwrap_or("none")
            );

            if !state.hook_stats.per_hook.is_empty() {
                println!("  Per-hook counts:");
                let mut hooks: Vec<_> = state.hook_stats.per_hook.iter().collect();
                hooks.sort_by(|a, b| b.1.cmp(a.1));
                for (hook, count) in hooks {
                    let avg_ms = state
                        .hook_stats
                        .per_hook_time_ms
                        .get(hook)
                        .map_or(0, |t| t / count);
                    println!("    {hook}: {count} calls (avg {avg_ms}ms)");
                }
            }
        }
    }

    if corrupt_count > 0 {
        println!(
            "\n{} {corrupt_count} corrupt state file(s) skipped",
            "warning:".yellow()
        );
    }

    // List proof chains
    let proof_sessions = sentinel_infrastructure::proof_store::list_sessions()?;
    if !proof_sessions.is_empty() {
        println!("\n{}", "Proof Chains:".bold());
        for session_id in &proof_sessions {
            if let Some(chain) = sentinel_infrastructure::proof_store::load_chain(session_id)? {
                let status = if chain.chain_valid {
                    "valid".green()
                } else {
                    "INVALID".red()
                };
                println!("  {session_id}: {} phases, {status}", chain.proofs.len());
            }
        }
    }

    Ok(())
}

/// `sentinel stats hooks` — read the per-call telemetry JSONL and print a
/// human summary scoped to the last N hours. The dashboard will eventually
/// read the same file via the daemon API; this CLI exists so users can
/// sanity-check the data without spinning up the daemon.
pub fn run_hooks(limit: usize, hours: u32) -> Result<()> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    let path = home
        .join(".claude")
        .join("sentinel")
        .join("metrics")
        .join("hook-invocations.jsonl");
    if !path.exists() {
        println!(
            "{} No hook telemetry recorded yet at {}",
            "info:".cyan(),
            path.display()
        );
        return Ok(());
    }

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(i64::from(hours));
    let content = fs::read_to_string(&path)?;

    let mut total: u64 = 0;
    let mut by_hook_count: HashMap<String, u64> = HashMap::new();
    let mut by_hook_duration: HashMap<String, u128> = HashMap::new();
    let mut by_outcome: HashMap<String, u64> = HashMap::new();
    let mut blocks: Vec<(String, String, String)> = Vec::new(); // (ts, hook, reason)

    for line in content.lines() {
        let row: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ts_str = row.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(ts_str) {
            if ts < cutoff {
                continue;
            }
        }
        total += 1;
        let hook = row
            .get("hook")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let outcome = row
            .get("outcome")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let dur = row.get("duration_us").and_then(serde_json::Value::as_u64).unwrap_or(0);

        *by_hook_count.entry(hook.clone()).or_insert(0) += 1;
        *by_hook_duration.entry(hook.clone()).or_insert(0) += u128::from(dur);
        *by_outcome.entry(outcome.clone()).or_insert(0) += 1;

        if matches!(outcome.as_str(), "block" | "deny") {
            let reason = row
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            blocks.push((ts_str.to_string(), hook, reason));
        }
    }

    println!("{}", format!("Hook telemetry — last {hours}h").bold());
    println!("Total invocations: {total}");
    println!();

    if total == 0 {
        return Ok(());
    }

    // Outcome breakdown
    println!("{}", "Outcomes:".bold());
    let mut outcomes: Vec<_> = by_outcome.iter().collect();
    outcomes.sort_by(|a, b| b.1.cmp(a.1));
    for (outcome, count) in &outcomes {
        let label = match outcome.as_str() {
            "allow" => "allow".green(),
            "block" => "block".yellow(),
            "deny" => "deny".red(),
            "inject" => "inject".cyan(),
            _ => outcome.normal(),
        };
        println!("  {label}: {count}");
    }
    println!();

    // Top by call count
    println!("{}", format!("Top {limit} hooks by call count:").bold());
    let mut counts: Vec<_> = by_hook_count.iter().collect();
    counts.sort_by(|a, b| b.1.cmp(a.1));
    for (hook, count) in counts.iter().take(limit) {
        let total_us = by_hook_duration.get(*hook).copied().unwrap_or(0);
        let avg_us = if **count > 0 {
            total_us / u128::from(**count)
        } else {
            0
        };
        println!("  {hook}: {count} calls (avg {avg_us}μs)");
    }
    println!();

    // Top by total time spent
    println!("{}", format!("Top {limit} hooks by total time:").bold());
    let mut by_time: Vec<_> = by_hook_duration.iter().collect();
    by_time.sort_by(|a, b| b.1.cmp(a.1));
    for (hook, total_us) in by_time.iter().take(limit) {
        let count = by_hook_count.get(*hook).copied().unwrap_or(0);
        println!("  {hook}: {total_us}μs total ({count} calls)");
    }

    if !blocks.is_empty() {
        println!();
        println!("{}", "Recent blocks:".bold());
        for (ts, hook, reason) in blocks.iter().rev().take(limit) {
            let short_ts = ts.split('.').next().unwrap_or(ts);
            println!("  {} {} — {}", short_ts.dimmed(), hook.yellow(), reason);
        }
    }

    Ok(())
}
