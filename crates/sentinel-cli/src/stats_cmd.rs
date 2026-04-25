//! `sentinel stats` — Hook execution statistics

use anyhow::Result;
use colored::Colorize;

pub async fn run() -> Result<()> {
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
