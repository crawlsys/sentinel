//! `sentinel break` — Glass Break Emergency Override
//!
//! Temporarily suspends workflow enforcement for a limited duration.
//! Requires interactive terminal confirmation (6-digit challenge code)
//! to prevent AI self-invocation.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use sentinel_domain::state::GlassBreak;

/// Maximum break duration in minutes
const MAX_DURATION_MINUTES: u32 = 30;

/// Default break duration in minutes
const DEFAULT_DURATION_MINUTES: u32 = 5;

/// Maximum breaks per hour (rate limiting)
const MAX_BREAKS_PER_HOUR: usize = 3;

/// Challenge confirmation timeout in seconds
const CHALLENGE_TIMEOUT_SECS: u64 = 30;

/// Break log entry (written to breaks.jsonl)
#[derive(Debug, Serialize, Deserialize)]
struct BreakLogEntry {
    timestamp: String,
    reason: String,
    workflow: Option<String>,
    duration_minutes: u32,
    challenge_code: String,
    tools_used_during_break: Vec<BreakToolUseLog>,
    auto_reengaged: bool,
}

/// Tool use log entry for JSONL output
#[derive(Debug, Serialize, Deserialize)]
struct BreakToolUseLog {
    tool: String,
    detail: String,
    ts: String,
}

/// Path to breaks.jsonl log file
fn breaks_log_path() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory")
        .join(".claude")
        .join("sentinel")
        .join("state")
        .join("breaks.jsonl")
}

/// Generate a 6-digit challenge code using getrandom
fn generate_challenge_code() -> Result<String> {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow::anyhow!("CSPRNG failed: {e}"))?;
    let num = u32::from_le_bytes(bytes) % 1_000_000;
    Ok(format!("BREAK-{num:06}"))
}

/// Count breaks in the last hour from the log file
fn count_recent_breaks() -> Result<usize> {
    let path = breaks_log_path();
    if !path.exists() {
        return Ok(0);
    }

    let content = std::fs::read_to_string(&path)
        .context("Failed to read breaks.jsonl")?;

    let one_hour_ago = Utc::now() - chrono::Duration::hours(1);
    let mut count = 0;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<BreakLogEntry>(line) {
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&entry.timestamp) {
                if ts >= one_hour_ago {
                    count += 1;
                }
            }
        }
    }

    Ok(count)
}

/// Append a break log entry to breaks.jsonl
fn append_break_log(entry: &BreakLogEntry) -> Result<()> {
    let path = breaks_log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .context("Failed to open breaks.jsonl")?;

    let json = serde_json::to_string(entry)?;
    writeln!(file, "{json}")?;
    Ok(())
}

/// Find the current session ID by looking at the most recent state file
fn find_current_session() -> Option<String> {
    let state_dir = dirs::home_dir()?
        .join(".claude")
        .join("sentinel")
        .join("state");

    if !state_dir.exists() {
        return None;
    }

    // Find the most recently modified .json state file
    let mut newest: Option<(String, std::time::SystemTime)> = None;
    if let Ok(entries) = std::fs::read_dir(&state_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_suffix(".json") {
                // Skip temp files and lock files
                if id.ends_with(".tmp") || id.ends_with(".lock") {
                    continue;
                }
                if let Ok(meta) = entry.metadata() {
                    if let Ok(modified) = meta.modified() {
                        match &newest {
                            Some((_, prev_time)) if modified > *prev_time => {
                                newest = Some((id.to_string(), modified));
                            }
                            None => {
                                newest = Some((id.to_string(), modified));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    newest.map(|(id, _)| id)
}

/// Main entry point for `sentinel break`
pub async fn run(
    reason: Option<String>,
    duration: Option<u32>,
    workflow: Option<String>,
    status: bool,
    cancel: bool,
    history: bool,
    history_json: bool,
) -> Result<()> {
    if status {
        return show_status().await;
    }

    if cancel {
        return cancel_break().await;
    }

    if history {
        return show_history(history_json).await;
    }

    // Initiate a new break
    let reason = reason.context(
        "Missing --reason flag. Usage: sentinel break --reason \"why you need this\"",
    )?;

    initiate_break(&reason, duration, workflow).await
}

/// Initiate a new glass break with interactive challenge
async fn initiate_break(
    reason: &str,
    duration: Option<u32>,
    workflow: Option<String>,
) -> Result<()> {
    // SECURITY: Refuse if stdin is not a terminal (prevents AI self-invocation)
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "[sentinel] BLOCKED: Glass break can only be initiated from an interactive terminal."
        );
        eprintln!("           This prevents AI agents from self-invoking break overrides.");
        anyhow::bail!("Glass break requires interactive terminal (stdin must be a TTY)");
    }

    // Rate limit: max 3 breaks per hour
    let recent_count = count_recent_breaks()?;
    if recent_count >= MAX_BREAKS_PER_HOUR {
        eprintln!(
            "[sentinel] RATE LIMITED: {recent_count} breaks in the last hour (max {MAX_BREAKS_PER_HOUR})."
        );
        eprintln!("           Wait for older breaks to age out before initiating another.");
        anyhow::bail!("Rate limit exceeded: max {MAX_BREAKS_PER_HOUR} breaks per hour");
    }

    // Validate duration
    let duration_minutes = duration.unwrap_or(DEFAULT_DURATION_MINUTES);
    if duration_minutes == 0 || duration_minutes > MAX_DURATION_MINUTES {
        anyhow::bail!(
            "Duration must be between 1 and {MAX_DURATION_MINUTES} minutes (got {duration_minutes})"
        );
    }

    // Generate challenge code
    let challenge_code = generate_challenge_code()?;

    // Display challenge
    eprintln!();
    eprintln!("  +============================================================+");
    eprintln!("  |  GLASS BREAK — Emergency Workflow Override                  |");
    eprintln!("  +============================================================+");
    eprintln!("  |  Reason: {:<51}|", reason);
    eprintln!("  |  Duration: {} minutes{:<44}|",
        duration_minutes,
        if workflow.is_some() {
            format!(" (workflow: {})", workflow.as_deref().unwrap_or(""))
        } else {
            " (all workflows)".to_string()
        }
    );
    eprintln!("  |                                                            |");
    eprintln!("  |  This will SUSPEND all workflow enforcement.               |");
    eprintln!("  |  All tool calls during the break will be logged.           |");
    eprintln!("  |                                                            |");
    eprintln!("  |  To confirm, type the challenge code below:                |");
    eprintln!("  |                                                            |");
    eprintln!("  |    >>> {:<53}|", &challenge_code);
    eprintln!("  |                                                            |");
    eprintln!("  |  You have {CHALLENGE_TIMEOUT_SECS} seconds to respond.                         |");
    eprintln!("  +============================================================+");
    eprintln!();
    eprint!("  Enter challenge code: ");
    std::io::stderr().flush()?;

    // Read user input with timeout
    let start = Instant::now();
    let mut input = String::new();

    // Use a blocking read with a timeout check
    // Since we verified stdin is a terminal, we can read directly
    let read_result = tokio::task::spawn_blocking(move || {
        std::io::stdin().read_line(&mut input).map(|_| input)
    });

    let timeout = Duration::from_secs(CHALLENGE_TIMEOUT_SECS);
    let user_input = match tokio::time::timeout(timeout, read_result).await {
        Ok(Ok(Ok(input))) => input.trim().to_string(),
        Ok(Ok(Err(e))) => {
            eprintln!("\n  [sentinel] Failed to read input: {e}");
            anyhow::bail!("Failed to read challenge response");
        }
        Ok(Err(e)) => {
            eprintln!("\n  [sentinel] Task error: {e}");
            anyhow::bail!("Challenge input task failed");
        }
        Err(_) => {
            eprintln!("\n  [sentinel] Challenge timed out after {CHALLENGE_TIMEOUT_SECS} seconds.");
            anyhow::bail!("Challenge timed out");
        }
    };

    let elapsed = start.elapsed();
    if elapsed > Duration::from_secs(CHALLENGE_TIMEOUT_SECS) {
        eprintln!("  [sentinel] Challenge timed out ({:.1}s > {CHALLENGE_TIMEOUT_SECS}s).", elapsed.as_secs_f64());
        anyhow::bail!("Challenge timed out");
    }

    // Verify challenge
    if user_input != challenge_code {
        eprintln!("  [sentinel] Challenge FAILED. Expected '{}', got '{}'.", challenge_code, user_input);
        anyhow::bail!("Challenge verification failed");
    }

    eprintln!("  [sentinel] Challenge accepted.");

    // Find current session and update state
    let session_id = find_current_session();
    if let Some(ref sid) = session_id {
        let _lock = sentinel_infrastructure::state_store::acquire_session_lock(sid)?;
        let mut state = sentinel_infrastructure::state_store::load(sid)?
            .unwrap_or_else(|| sentinel_domain::state::SessionState::new(sid.clone()));

        let now = Utc::now();
        let expires_at = now + chrono::Duration::minutes(i64::from(duration_minutes));

        state.glass_break = Some(GlassBreak {
            reason: reason.to_string(),
            started_at: now,
            expires_at,
            duration_minutes,
            workflow: workflow.clone(),
            challenge_code: challenge_code.clone(),
            tools_used: Vec::new(),
        });

        sentinel_infrastructure::state_store::save(&mut state)?;
        eprintln!(
            "  [sentinel] Glass break ACTIVE for session '{}'. Expires at {}.",
            sid,
            expires_at.format("%H:%M:%S UTC")
        );
    } else {
        eprintln!("  [sentinel] WARNING: No active session found. Break state will be logged but not applied.");
    }

    // Log to breaks.jsonl
    let entry = BreakLogEntry {
        timestamp: Utc::now().to_rfc3339(),
        reason: reason.to_string(),
        workflow,
        duration_minutes,
        challenge_code,
        tools_used_during_break: Vec::new(),
        auto_reengaged: false,
    };
    append_break_log(&entry)?;

    eprintln!();
    eprintln!("  Workflow enforcement SUSPENDED for {duration_minutes} minutes.");
    eprintln!("  Use `sentinel break --cancel` to re-engage early.");
    eprintln!();

    Ok(())
}

/// Show the status of the current glass break
async fn show_status() -> Result<()> {
    let session_id = find_current_session();
    match session_id {
        Some(ref sid) => {
            let _lock = sentinel_infrastructure::state_store::acquire_session_lock(sid)?;
            let state = sentinel_infrastructure::state_store::load(sid)?;

            match state.and_then(|s| s.glass_break) {
                Some(gb) => {
                    let now = Utc::now();
                    if now < gb.expires_at {
                        let remaining = gb.expires_at - now;
                        let mins = remaining.num_minutes();
                        let secs = remaining.num_seconds() % 60;
                        eprintln!("  Glass break ACTIVE");
                        eprintln!("    Reason:    {}", gb.reason);
                        eprintln!("    Started:   {}", gb.started_at.format("%H:%M:%S UTC"));
                        eprintln!("    Expires:   {}", gb.expires_at.format("%H:%M:%S UTC"));
                        eprintln!("    Remaining: {}m {}s", mins, secs);
                        eprintln!("    Workflow:  {}", gb.workflow.as_deref().unwrap_or("all"));
                        eprintln!("    Challenge: {}", gb.challenge_code);
                        eprintln!("    Tools used: {}", gb.tools_used.len());
                        for tu in &gb.tools_used {
                            eprintln!("      - {} {} ({})", tu.tool, tu.detail, tu.ts);
                        }
                    } else {
                        eprintln!("  No active glass break (last break expired at {})",
                            gb.expires_at.format("%H:%M:%S UTC"));
                    }
                }
                None => {
                    eprintln!("  No active glass break.");
                }
            }
        }
        None => {
            eprintln!("  No active session found.");
        }
    }
    Ok(())
}

/// Cancel the current glass break (re-engage workflow enforcement)
async fn cancel_break() -> Result<()> {
    let session_id = find_current_session();
    match session_id {
        Some(ref sid) => {
            let _lock = sentinel_infrastructure::state_store::acquire_session_lock(sid)?;
            let mut state = sentinel_infrastructure::state_store::load(sid)?
                .unwrap_or_else(|| sentinel_domain::state::SessionState::new(sid.clone()));

            match state.glass_break.take() {
                Some(gb) => {
                    // Update the JSONL log with tools_used
                    let entry = BreakLogEntry {
                        timestamp: gb.started_at.to_rfc3339(),
                        reason: gb.reason.clone(),
                        workflow: gb.workflow.clone(),
                        duration_minutes: gb.duration_minutes,
                        challenge_code: gb.challenge_code.clone(),
                        tools_used_during_break: gb.tools_used.iter().map(|tu| BreakToolUseLog {
                            tool: tu.tool.clone(),
                            detail: tu.detail.clone(),
                            ts: tu.ts.clone(),
                        }).collect(),
                        auto_reengaged: false,
                    };
                    append_break_log(&entry)?;

                    sentinel_infrastructure::state_store::save(&mut state)?;
                    eprintln!("  [sentinel] Glass break CANCELLED. Workflow enforcement re-engaged.");
                    eprintln!("    {} tool calls were made during the break.", gb.tools_used.len());
                }
                None => {
                    eprintln!("  [sentinel] No active glass break to cancel.");
                }
            }
        }
        None => {
            eprintln!("  No active session found.");
        }
    }
    Ok(())
}

/// Show break history from breaks.jsonl
async fn show_history(json_output: bool) -> Result<()> {
    let path = breaks_log_path();
    if !path.exists() {
        if json_output {
            println!("[]");
        } else {
            eprintln!("  No break history found.");
        }
        return Ok(());
    }

    let content = std::fs::read_to_string(&path)
        .context("Failed to read breaks.jsonl")?;

    let thirty_days_ago = Utc::now() - chrono::Duration::days(30);
    let mut entries: Vec<BreakLogEntry> = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<BreakLogEntry>(line) {
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&entry.timestamp) {
                if ts >= thirty_days_ago {
                    entries.push(entry);
                }
            }
        }
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        if entries.is_empty() {
            eprintln!("  No breaks in the last 30 days.");
            return Ok(());
        }

        eprintln!("  Break history (last 30 days):");
        eprintln!("  {:-<62}", "");
        for entry in &entries {
            eprintln!(
                "  {} | {} | {}min | {} | {} tools",
                &entry.timestamp[..19],
                entry.challenge_code,
                entry.duration_minutes,
                entry.workflow.as_deref().unwrap_or("all"),
                entry.tools_used_during_break.len(),
            );
            if !entry.reason.is_empty() {
                eprintln!("    Reason: {}", entry.reason);
            }
        }
        eprintln!("  {:-<62}", "");
        eprintln!("  Total: {} breaks", entries.len());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_challenge_code() {
        let code = generate_challenge_code().unwrap();
        assert!(code.starts_with("BREAK-"));
        assert_eq!(code.len(), 12); // "BREAK-" (6) + 6 digits
        // Verify the numeric part is valid
        let num_part = &code[6..];
        assert!(num_part.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn test_challenge_code_uniqueness() {
        let code1 = generate_challenge_code().unwrap();
        let code2 = generate_challenge_code().unwrap();
        // Not a guarantee but with 1M possibilities, collision is very unlikely
        assert_ne!(code1, code2);
    }

    #[test]
    fn test_break_log_serialization() {
        let entry = BreakLogEntry {
            timestamp: "2026-03-14T05:03:00Z".to_string(),
            reason: "test break".to_string(),
            workflow: Some("steel".to_string()),
            duration_minutes: 5,
            challenge_code: "BREAK-123456".to_string(),
            tools_used_during_break: vec![
                BreakToolUseLog {
                    tool: "Bash".to_string(),
                    detail: "git status".to_string(),
                    ts: "2026-03-14T05:03:10Z".to_string(),
                },
            ],
            auto_reengaged: true,
        };

        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: BreakLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.reason, "test break");
        assert_eq!(deserialized.duration_minutes, 5);
        assert_eq!(deserialized.tools_used_during_break.len(), 1);
    }

    #[test]
    fn test_max_duration_validation() {
        assert!(MAX_DURATION_MINUTES == 30);
        assert!(DEFAULT_DURATION_MINUTES == 5);
    }
}
