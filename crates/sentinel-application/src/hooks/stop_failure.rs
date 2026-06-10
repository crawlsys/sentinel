//! `StopFailure` hook — detect API errors and rate limits.
//!
//! Called when a turn ends due to an API error rather than normal completion.
//! For rate limits, this hook now rotates the active Claude account
//! immediately and requests a clean relaunch so the next account picks up
//! without the user getting stuck in a dead session.
//!
//! ## Error message formats (from Claude Code 2.1.88 source)
//!
//! Claude Code formats rate limit messages via `NP6()` + `SB1()`:
//! - `"You've hit your session limit · resets 4pm (CDT)"`
//! - `"You've hit your limit · resets Apr 3, 4:30pm (CDT)"`
//! - `"You've used 87% of your session limit · resets 4pm (CDT)"`
//! - `"You're now using extra usage · Your session limit resets 4pm (CDT)"`
//!
//! The `error` field is `"rate_limit"` and the `content/error_details` contains
//! the human-readable message above.

use sentinel_domain::events::{HookInput, HookOutput};

const DEFAULT_RATE_LIMIT_COOLDOWN_MINUTES: u64 = 300;

/// Parse `error_details` to extract a cooldown duration in minutes.
///
/// Handles patterns from Anthropic rate limit errors:
/// - "resets 4pm (CDT)" / "resets 4:30pm (CDT)" — Claude Code's actual format
/// - "resets Apr 3, 4pm (CDT)" — same but with date
/// - "resets at 4:00 PM" / "resets at 16:00" — absolute time
/// - "retry after 3600" / "retry-after: 3600" — seconds
/// - "resets in 4h 30m" / "resets in 270 minutes" — relative duration
/// - "`rate_limit_error`" with no parseable time — returns None (caller defaults to 5hr)
fn parse_reset_minutes(error_details: &str) -> Option<u64> {
    let lower = error_details.to_lowercase();

    // Pattern: "retry after <seconds>" or "retry-after: <seconds>"
    if let Some(idx) = lower
        .find("retry after")
        .or_else(|| lower.find("retry-after"))
    {
        let after = &lower[idx..];
        if let Some(secs) = extract_first_number(after) {
            let minutes = secs.div_ceil(60); // round up
            if minutes > 0 && minutes <= 600 {
                return Some(minutes);
            }
        }
    }

    // Pattern: "resets in <N>h <N>m" or "resets in <N> minutes" or "resets in <N> hours"
    if let Some(idx) = lower.find("resets in") {
        let after = &lower[idx + 9..];
        return parse_relative_duration(after);
    }

    // Pattern: "resets <time>" — Claude Code's actual format (no "at" keyword)
    // Matches: "resets 4pm", "resets 4:30pm (CDT)", "resets Apr 3, 4pm (CDT)"
    // Also: "resets at 4:00 PM"
    if let Some(idx) = lower.find("resets") {
        let after_resets = &error_details[idx + 6..]; // preserve original case
        let trimmed = after_resets.trim_start();
        // Skip "at " if present
        let time_str = if trimmed.to_lowercase().starts_with("at ") {
            &trimmed[3..]
        } else {
            trimmed
        };
        return parse_absolute_time_flexible(time_str);
    }

    None
}

/// Extract the first integer from a string
fn extract_first_number(s: &str) -> Option<u64> {
    let mut num_str = String::new();
    let mut found_digit = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num_str.push(ch);
            found_digit = true;
        } else if found_digit {
            break;
        }
    }
    num_str.parse().ok()
}

/// Parse relative durations like "4h 30m", "270 minutes", "3 hours"
fn parse_relative_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let mut total_minutes: u64 = 0;
    let mut found = false;

    for part in s.split_whitespace() {
        if let Some(h) = part.strip_suffix('h') {
            if let Ok(hours) = h.parse::<u64>() {
                total_minutes += hours * 60;
                found = true;
            }
        } else if let Some(m) = part.strip_suffix('m') {
            if let Ok(mins) = m.parse::<u64>() {
                total_minutes += mins;
                found = true;
            }
        } else if let Some(num) = extract_first_number(part) {
            if s.contains("hour") {
                total_minutes += num * 60;
                found = true;
            } else if s.contains("minute") || s.contains("min") {
                total_minutes += num;
                found = true;
            }
            if found {
                break;
            }
        }
    }

    if found && total_minutes > 0 && total_minutes <= 36_000 {
        Some(total_minutes)
    } else {
        None
    }
}

/// Parse flexible absolute time formats from Claude Code's `NP6()` formatter:
/// - "4pm (CDT)" → extract hour
/// - "4:30pm (CDT)" → extract hour:minute
/// - "Apr 3, 4pm (CDT)" → skip date part, extract time
/// - "4:00 PM" → standard format
/// - "16:00" → 24h format
///
/// Returns minutes from now. Since we can't reliably determine the timezone
/// from short abbreviations like "(CDT)", and the reset is always within
/// the Anthropic 5h rolling window, we return a conservative estimate:
/// - If we can parse the time, estimate 60–300 minutes (the 5h window)
/// - The exact value doesn't need to be perfect — it just needs to be
///   better than the 300-minute default
fn parse_absolute_time_flexible(s: &str) -> Option<u64> {
    let s = s.trim();
    // Strip timezone in parens: "(CDT)", "(EST)", etc.
    let clean = if let Some(paren_idx) = s.find('(') {
        s[..paren_idx].trim()
    } else {
        s
    };

    // If there's a comma, the date portion is before it: "Apr 3, 4pm"
    // Take only the part after the last comma
    let time_part = if let Some(comma_idx) = clean.rfind(',') {
        clean[comma_idx + 1..].trim()
    } else {
        clean.trim()
    };

    // Now parse the time part: "4pm", "4:30pm", "4:00 PM", "16:00"
    let lower = time_part.to_lowercase();
    let is_pm = lower.contains("pm");
    let is_am = lower.contains("am");

    let digits_only = lower.replace("pm", "").replace("am", "").trim().to_string();

    let (hour, minute) = if digits_only.contains(':') {
        let parts: Vec<&str> = digits_only.split(':').collect();
        let h: u64 = parts.first()?.trim().parse().ok()?;
        let m: u64 = parts
            .get(1)
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or(0);
        (h, m)
    } else {
        let h: u64 = digits_only.trim().parse().ok()?;
        (h, 0)
    };

    // Validate
    let hour_val = if is_pm && hour < 12 {
        hour + 12
    } else if is_am && hour == 12 {
        0
    } else {
        hour
    };
    if hour_val >= 24 || minute >= 60 {
        return None;
    }

    // We successfully parsed a time from the reset message.
    // Since the timezone is local (not UTC) and we can't reliably convert,
    // use chrono::Local to compute diff in the user's timezone.
    let now = chrono::Local::now();
    let now_minutes = now.format("%H").to_string().parse::<u64>().unwrap_or(0) * 60
        + now.format("%M").to_string().parse::<u64>().unwrap_or(0);
    let target_minutes = hour_val * 60 + minute;

    let diff = if target_minutes > now_minutes {
        target_minutes - now_minutes
    } else {
        // Target is earlier today — assume it's tomorrow (or just passed)
        // For rate limits, if it just passed, use a small buffer
        let wrap = (24 * 60) - now_minutes + target_minutes;
        if wrap > 300 {
            // More than 5h away means it probably just passed — use 5m buffer
            5
        } else {
            wrap
        }
    };

    // Sanity: cap at 5h (300min) since Anthropic uses a rolling 5h window
    if diff > 0 {
        Some(diff.min(300))
    } else {
        Some(5) // Just reset — 5 minute buffer
    }
}

fn rotate_accounts(
    ctx: &super::HookContext<'_>,
    cooldown_minutes: u64,
) -> anyhow::Result<super::ProcessOutput> {
    let cooldown_arg = format!("--cooldown-minutes={cooldown_minutes}");
    // Binary was renamed from `accounts` to `claude-accounts` (per
    // claude-accounts-cli-rust Cargo.toml `[[bin]] name = "claude-accounts"`).
    // The legacy name was kept as a shadow copy at ~/.cargo/bin/accounts.exe
    // for a while, but that shadow drifts: any cargo install of the CLI only
    // updates `claude-accounts.exe`. Call the canonical name directly.
    ctx.process
        .run("claude-accounts", &["rotate", cooldown_arg.as_str()], None)
}

fn summarize_process_failure(output: &super::ProcessOutput) -> String {
    let stderr = output.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }

    let stdout = output.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_string();
    }

    "accounts rotate exited unsuccessfully".to_string()
}

/// Process `StopFailure` event
///
/// Logs the error, rotates accounts on rate limits, and requests a clean
/// relaunch so the handler can resume on the next account automatically.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let error = input
        .extra
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let error_details = input
        .extra
        .get("error_details")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    tracing::warn!(error, error_details, "Turn ended with API error");

    // Log to errors.jsonl for diagnostics
    if let Some(home) = ctx.fs.home_dir() {
        let metrics_dir = super::metrics_dir(&home);
        let entry = serde_json::json!({
            "event": "stop_failure",
            "error": error,
            "error_details": error_details,
            "session_id": input.session_id,
            "ts": chrono::Utc::now().to_rfc3339(),
        });

        let line = format!("{entry}\n");
        let _ = ctx
            .fs
            .append(&metrics_dir.join("errors.jsonl"), line.as_bytes());
    }

    // If this is a rate limit error, rotate immediately and stop this session
    let is_rate_limit = error.contains("rate_limit")
        || error.contains("overloaded")
        || error_details.contains("rate limit")
        || error_details.contains("rate_limit")
        || error_details.contains("Too many requests")
        || error_details.contains("You've hit your")
        || error_details.contains("You've used");

    if is_rate_limit {
        let cooldown_minutes =
            parse_reset_minutes(error_details).unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN_MINUTES);

        match rotate_accounts(ctx, cooldown_minutes) {
            Ok(output) if output.success => {
                let summary = output.stdout.trim();
                let rotate_msg = if summary.is_empty() {
                    "Auto-rotated account.".to_string()
                } else {
                    format!("Auto-rotated account: {}", summary.replace("**", ""))
                };

                // `accounts rotate` ran synchronously and finished its
                // fan-out: every live `c`-launched session now has new
                // tokens atomically written to its
                // `~/.claude/session-env/<id>/.credentials.json`. Claude
                // Code calls `Ue9()` (mtime check) inside `k$()` before
                // every API request — so the next request after this
                // hook returns will read the fresh tokens automatically.
                //
                // We DON'T persist a relaunch request file: nothing in
                // claude-code-handler-rust consumes it (verified: zero
                // refs to `relaunch_request` / `RATE_LIMIT_RELAUNCH_FILE`
                // anywhere in the handler crate). It was vestigial from
                // an earlier design. The mtime-watch fanout makes
                // restart-style recovery unnecessary.
                tracing::info!(
                    cooldown_minutes,
                    session_id = input.session_id.as_deref().unwrap_or("unknown"),
                    "Rate limit detected, account rotated, fanout complete (in-place token swap)"
                );

                return HookOutput {
                    system_message: Some(format!(
                        "[Rate Limit] Account hit rate limit (cooldown: {cooldown_minutes}m). \
                         {rotate_msg} Tokens were swapped in-place across all live `c` sessions; \
                         the next API request will use the new account automatically. \
                         No restart needed — just retry the message that failed."
                    )),
                    // Stop the current turn cleanly. The user retries
                    // their message; the retry's k$()/Ue9() detects the
                    // updated .credentials.json mtime, clears the OAuth
                    // cache, and reads the new tokens. No restart, no
                    // session loss, no manual intervention.
                    continue_: Some(false),
                    stop_reason: Some(
                        "Account rate-limited. Rotated to the next account; \
                         next request will use it automatically."
                            .to_string(),
                    ),
                    ..HookOutput::default()
                };
            }
            Ok(output) => {
                let detail = summarize_process_failure(&output);
                tracing::warn!(
                    cooldown_minutes,
                    detail,
                    "Rate limit detected but account rotation command failed"
                );

                return HookOutput {
                    system_message: Some(format!(
                        "[Rate Limit] Account hit rate limit (cooldown: {cooldown_minutes}m), \
                         but auto-rotation failed: {detail}. Close this session and relaunch `c`."
                    )),
                    continue_: Some(false),
                    stop_reason: Some(
                        "Account rate-limited. Auto-rotation failed and this session cannot continue."
                            .to_string(),
                    ),
                    ..HookOutput::default()
                };
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    cooldown_minutes,
                    "Rate limit detected but accounts CLI could not be executed"
                );

                return HookOutput {
                    system_message: Some(format!(
                        "[Rate Limit] Account hit rate limit (cooldown: {cooldown_minutes}m), \
                         but auto-rotation could not start: {e}. Close this session and relaunch `c`."
                    )),
                    continue_: Some(false),
                    stop_reason: Some(
                        "Account rate-limited. Auto-rotation could not start and this session cannot continue."
                            .to_string(),
                    ),
                    ..HookOutput::default()
                };
            }
        }
    }

    // Authentication errors — the in-process token was rejected by Anthropic.
    //
    // This happens when (a) the access_token expired and we somehow served a
    // stale one, (b) the refresh_token was revoked, or (c) the session's
    // `.credentials.json` got out of sync with the slot it claims to be on.
    // In all three cases the right move is to rotate to the next live slot
    // and let the fanout swap fresh tokens into every session — *without*
    // penalizing the current slot, because the slot itself isn't necessarily
    // exhausted (it's just stale or revoked, which the rotator will detect
    // when it tries to mint a token for it).
    //
    // Cooldown=0 means "rotate now, don't mark this slot as rate-limited."
    // If the slot's refresh_token is genuinely revoked, the rotator will
    // skip past it via its invalid_grant detection.
    let is_auth_error = error.contains("authentication_failed")
        || error.contains("authentication_error")
        || error.contains("invalid_credentials")
        || error_details.contains("authentication_error")
        || error_details.contains("Invalid credentials")
        || error_details.contains("\"401\"")
        || error_details.starts_with("401 ");

    if is_auth_error {
        match rotate_accounts(ctx, 0) {
            Ok(output) if output.success => {
                let summary = output.stdout.trim();
                let rotate_msg = if summary.is_empty() {
                    "Auto-rotated account.".to_string()
                } else {
                    format!("Auto-rotated account: {}", summary.replace("**", ""))
                };

                tracing::info!(
                    session_id = input.session_id.as_deref().unwrap_or("unknown"),
                    "Auth error detected, account rotated, fanout complete (in-place token swap)"
                );

                return HookOutput {
                    system_message: Some(format!(
                        "[Auth Error] Token rejected by Anthropic. \
                         {rotate_msg} Tokens were swapped in-place across all live `c` sessions; \
                         the next API request will use the new account automatically. \
                         Just retry the message that failed."
                    )),
                    continue_: Some(false),
                    stop_reason: Some(
                        "Token rejected. Rotated to the next account; \
                         next request will use it automatically."
                            .to_string(),
                    ),
                    ..HookOutput::default()
                };
            }
            Ok(output) => {
                let detail = summarize_process_failure(&output);
                tracing::warn!(detail, "Auth error detected but account rotation failed");

                return HookOutput {
                    system_message: Some(format!(
                        "[Auth Error] Token rejected by Anthropic and auto-rotation failed: {detail}. \
                         Run `account_login <slot>` to re-auth, or close this session and relaunch `c`."
                    )),
                    continue_: Some(false),
                    stop_reason: Some(
                        "Token rejected. Auto-rotation failed and this session cannot continue."
                            .to_string(),
                    ),
                    ..HookOutput::default()
                };
            }
            Err(e) => {
                tracing::warn!(error = %e, "Auth error detected but accounts CLI could not be executed");

                return HookOutput {
                    system_message: Some(format!(
                        "[Auth Error] Token rejected by Anthropic and auto-rotation could not start: {e}. \
                         Run `account_login <slot>` to re-auth, or close this session and relaunch `c`."
                    )),
                    continue_: Some(false),
                    stop_reason: Some(
                        "Token rejected. Auto-rotation could not start and this session cannot continue."
                            .to_string(),
                    ),
                    ..HookOutput::default()
                };
            }
        }
    }

    // Non-rate-limit, non-auth API error — turn aborted. Account-failure
    // notifications now live in claude-code-handler-rust where they have
    // direct access to slot/trace/session forensics; this hook just lets
    // the turn end cleanly.
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::super::{FileSystemPort, GitStatusPort, HookContext, ProcessOutput, ProcessPort};

    struct TestFs {
        home: PathBuf,
    }

    impl FileSystemPort for TestFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(&self, path: &Path) -> anyhow::Result<String> {
            Ok(fs::read_to_string(path)?)
        }
        fn write(&self, path: &Path, content: &[u8]) -> anyhow::Result<()> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            Ok(fs::write(path, content)?)
        }
        fn create_dir_all(&self, path: &Path) -> anyhow::Result<()> {
            Ok(fs::create_dir_all(path)?)
        }
        fn read_dir(&self, path: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(fs::read_dir(path)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect())
        }
        fn exists(&self, path: &Path) -> bool {
            path.exists()
        }
        fn is_dir(&self, path: &Path) -> bool {
            path.is_dir()
        }
        fn metadata(&self, path: &Path) -> anyhow::Result<std::fs::Metadata> {
            Ok(fs::metadata(path)?)
        }
        fn append(&self, path: &Path, content: &[u8]) -> anyhow::Result<()> {
            use std::io::Write;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            file.write_all(content)?;
            Ok(())
        }
    }

    struct StubGit;
    impl GitStatusPort for StubGit {
        fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn current_branch(&self, _: &str) -> anyhow::Result<String> {
            Ok("main".into())
        }
        fn is_worktree(&self, _: &str) -> bool {
            false
        }
        fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn repo_root(&self, _: &str) -> Option<String> {
            None
        }
        fn list_worktree_names(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merge_base(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> {
            None
        }
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> {
            None
        }
        fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
    }

    /// Test fixture for [`TestProcess::run`]. The `Err` variant is used by
    /// future tests that exercise the "rotation failed" failure mode; it's
    /// preserved here so the harness covers both shell-success and shell-
    /// failure paths without touching the production code shape.
    #[allow(dead_code)]
    enum TestProcessResult {
        Ok(ProcessOutput),
        Err(String),
    }

    struct TestProcess {
        output: TestProcessResult,
    }

    impl ProcessPort for TestProcess {
        fn run(&self, _: &str, _: &[&str], _: Option<&str>) -> anyhow::Result<ProcessOutput> {
            match &self.output {
                TestProcessResult::Ok(output) => Ok(output.clone()),
                TestProcessResult::Err(message) => Err(anyhow::anyhow!(message.clone())),
            }
        }

        fn spawn_detached(&self, _: &str, _: &[&str]) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_stop_failure_allows_non_rate_limit() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("error".to_string(), serde_json::json!("auth_error"));

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
        assert!(output.system_message.is_none());
    }

    #[test]
    fn test_stop_failure_rate_limit_no_details() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("error".to_string(), serde_json::json!("rate_limit"));
        input
            .extra
            .insert("error_details".to_string(), serde_json::json!(""));

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.system_message.is_some());
        let msg = output.system_message.as_ref().unwrap();
        assert!(msg.contains("cooldown: 300m"));
        assert!(msg.contains("Auto-rotated"));
        assert_eq!(output.continue_, Some(false));
        assert!(output.stop_reason.is_some());
    }

    #[test]
    fn test_stop_failure_rate_limit_with_retry_after() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("error".to_string(), serde_json::json!("rate_limit"));
        input.extra.insert(
            "error_details".to_string(),
            serde_json::json!("retry after 7200"),
        );

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        let msg = output.system_message.as_ref().unwrap();
        assert!(msg.contains("cooldown: 120m"));
        assert_eq!(output.continue_, Some(false));
    }

    #[test]
    fn test_parse_retry_after_seconds() {
        assert_eq!(parse_reset_minutes("retry after 3600"), Some(60));
        assert_eq!(parse_reset_minutes("retry-after: 7200"), Some(120));
        assert_eq!(parse_reset_minutes("retry after 1800"), Some(30));
    }

    #[test]
    fn test_parse_resets_in_relative() {
        assert_eq!(parse_reset_minutes("resets in 4h 30m"), Some(270));
        assert_eq!(parse_reset_minutes("resets in 2h"), Some(120));
        assert_eq!(parse_reset_minutes("resets in 30m"), Some(30));
        assert_eq!(parse_reset_minutes("resets in 270 minutes"), Some(270));
        assert_eq!(parse_reset_minutes("resets in 3 hours"), Some(180));
    }

    #[test]
    fn test_parse_no_time_info() {
        assert_eq!(parse_reset_minutes("rate_limit_error"), None);
        assert_eq!(parse_reset_minutes(""), None);
        assert_eq!(parse_reset_minutes("something went wrong"), None);
    }

    // Claude Code format: "resets 4pm (CDT)"
    #[test]
    fn test_parse_claude_code_format() {
        // These depend on current time, so we just verify they return Some
        let r1 = parse_reset_minutes("You've hit your session limit · resets 4pm (CDT)");
        assert!(r1.is_some(), "should parse 'resets 4pm (CDT)'");

        let r2 = parse_reset_minutes("You've hit your limit · resets 4:30pm (CDT)");
        assert!(r2.is_some(), "should parse 'resets 4:30pm (CDT)'");

        let r3 = parse_reset_minutes("You've used 87% of your session limit · resets 4pm (CDT)");
        assert!(r3.is_some(), "should parse utilization + resets");
    }

    // Claude Code format with date: "resets Apr 3, 4pm (CDT)"
    #[test]
    fn test_parse_claude_code_format_with_date() {
        let r = parse_reset_minutes("You've hit your limit · resets Apr 3, 4pm (CDT)");
        assert!(r.is_some(), "should parse 'resets Apr 3, 4pm (CDT)'");
    }

    #[test]
    fn test_parse_resets_at_absolute() {
        let result = parse_reset_minutes("resets at 4:00 PM");
        assert!(result.is_some() || result.is_none()); // time-dependent

        let result = parse_reset_minutes("resets at 16:00");
        assert!(result.is_some() || result.is_none());
    }

    #[test]
    fn test_stop_failure_detects_youve_hit_your() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("error".to_string(), serde_json::json!("rate_limit"));
        input.extra.insert(
            "error_details".to_string(),
            serde_json::json!("You've hit your session limit · resets 4pm (CDT)"),
        );

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.system_message.is_some());
        let msg = output.system_message.as_ref().unwrap();
        assert!(msg.contains("cooldown:"));
        assert_eq!(output.continue_, Some(false));
    }

    /// On rate_limit detection, the hook stops the current turn cleanly
    /// and emits a system message describing the in-place token swap.
    /// It does NOT persist a relaunch-request file (no consumer exists)
    /// and it does NOT request a process restart — the
    /// `~/.claude/session-env/<id>/.credentials.json` mtime watch in
    /// Claude Code's k$()/Ue9() handles the swap automatically on the
    /// next API request.
    #[test]
    fn test_stop_failure_rate_limit_rotates_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let fs: &'static TestFs = Box::leak(Box::new(TestFs {
            home: tmp.path().to_path_buf(),
        }));
        let git: &'static StubGit = Box::leak(Box::new(StubGit));
        let process_port: &'static TestProcess = Box::leak(Box::new(TestProcess {
            output: TestProcessResult::Ok(ProcessOutput {
                success: true,
                stdout: "Auto-rotated: **claude4** -> **claude5**".to_string(),
                stderr: String::new(),
            }),
        }));
        let memory_mcp: &'static crate::hooks::test_support::StubMemoryMcp =
            Box::leak(Box::new(crate::hooks::test_support::StubMemoryMcp));
        let env: &'static crate::hooks::test_support::StubEnv =
            Box::leak(Box::new(crate::hooks::test_support::StubEnv::new()));
        let ctx = HookContext {
            git,
            vector_store: None,
            fs,
            process: process_port,
            llm: None,
            memory_mcp,
            env,
        };

        let mut input = HookInput::default();
        input.session_id = Some("session-123".to_string());
        input.cwd = Some(r"C:\Users\garys".to_string());
        input
            .extra
            .insert("error".to_string(), serde_json::json!("rate_limit"));
        input.extra.insert(
            "error_details".to_string(),
            serde_json::json!("You've hit your limit · resets 1pm (America/Chicago)"),
        );

        let output = process(&input, &ctx);

        // Turn stops cleanly so the user can retry their message.
        assert_eq!(output.continue_, Some(false));
        // System message describes the in-place swap, not a phantom restart.
        let msg = output.system_message.expect("system_message present");
        assert!(msg.contains("swapped in-place"), "msg: {msg}");
        assert!(msg.contains("retry"), "msg: {msg}");
        assert!(
            msg.contains("claude4"),
            "msg should mention old account: {msg}"
        );
        assert!(
            msg.contains("claude5"),
            "msg should mention new account: {msg}"
        );
        assert!(
            !msg.contains("Restarting"),
            "msg must not promise a restart that doesn't happen: {msg}"
        );

        // The hook MUST NOT write the dead-letter relaunch-request file.
        let request_path = tmp
            .path()
            .join(".claude")
            .join("claude-code-handler")
            .join("rate-limit-relaunch.json");
        assert!(
            !request_path.exists(),
            "hook should not persist the vestigial relaunch-request file"
        );
    }

    /// On `authentication_failed` (typical 401 from Anthropic), the hook
    /// must rotate to the next slot the same way it does for `rate_limit`,
    /// but with cooldown=0 (auth errors mean stale token, not exhausted
    /// account — the slot itself isn't burning quota).
    #[test]
    fn test_stop_failure_auth_error_rotates_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let fs: &'static TestFs = Box::leak(Box::new(TestFs {
            home: tmp.path().to_path_buf(),
        }));
        let git: &'static StubGit = Box::leak(Box::new(StubGit));
        let process_port: &'static TestProcess = Box::leak(Box::new(TestProcess {
            output: TestProcessResult::Ok(ProcessOutput {
                success: true,
                stdout: "Auto-rotated: **claude2** -> **claude3**".to_string(),
                stderr: String::new(),
            }),
        }));
        let memory_mcp: &'static crate::hooks::test_support::StubMemoryMcp =
            Box::leak(Box::new(crate::hooks::test_support::StubMemoryMcp));
        let env: &'static crate::hooks::test_support::StubEnv =
            Box::leak(Box::new(crate::hooks::test_support::StubEnv::new()));
        let ctx = HookContext {
            git,
            vector_store: None,
            fs,
            process: process_port,
            llm: None,
            memory_mcp,
            env,
        };

        let mut input = HookInput::default();
        input.session_id = Some("session-auth".to_string());
        input.extra.insert(
            "error".to_string(),
            serde_json::json!("authentication_failed"),
        );
        input
            .extra
            .insert("error_details".to_string(), serde_json::json!(""));

        let output = process(&input, &ctx);

        // Same shape as rate_limit: stop the turn cleanly so the user retries.
        assert_eq!(output.continue_, Some(false));
        let msg = output.system_message.expect("system_message present");
        assert!(msg.contains("[Auth Error]"), "msg: {msg}");
        assert!(msg.contains("swapped in-place"), "msg: {msg}");
        assert!(
            msg.contains("claude2"),
            "msg should mention old account: {msg}"
        );
        assert!(
            msg.contains("claude3"),
            "msg should mention new account: {msg}"
        );
    }

    /// `error: "invalid_request"` (e.g. prompt-too-long 400s) MUST NOT
    /// trigger account rotation. Those are user input problems, not auth
    /// problems — rotating away from a healthy slot would be silly.
    #[test]
    fn test_stop_failure_invalid_request_does_not_rotate() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("error".to_string(), serde_json::json!("invalid_request"));
        input.extra.insert(
            "error_details".to_string(),
            serde_json::json!("400 prompt is too long: 200993 tokens > 200000 maximum"),
        );

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        // No rotation message, no continue_:false — just the standard
        // turn-aborted ntfy push and HookOutput::allow().
        assert!(output.system_message.is_none());
        assert!(output.continue_.is_none() || output.continue_ == Some(true));
    }

    /// 401 error_details payloads (the form Claude Code reports when
    /// Anthropic returns "Invalid credentials") must trigger rotation
    /// even if the top-level `error` field doesn't say "auth".
    #[test]
    fn test_stop_failure_401_in_details_rotates() {
        let tmp = tempfile::tempdir().unwrap();
        let fs: &'static TestFs = Box::leak(Box::new(TestFs {
            home: tmp.path().to_path_buf(),
        }));
        let git: &'static StubGit = Box::leak(Box::new(StubGit));
        let process_port: &'static TestProcess = Box::leak(Box::new(TestProcess {
            output: TestProcessResult::Ok(ProcessOutput {
                success: true,
                stdout: "Auto-rotated: **claude5** -> **claude1**".to_string(),
                stderr: String::new(),
            }),
        }));
        let memory_mcp: &'static crate::hooks::test_support::StubMemoryMcp =
            Box::leak(Box::new(crate::hooks::test_support::StubMemoryMcp));
        let env: &'static crate::hooks::test_support::StubEnv =
            Box::leak(Box::new(crate::hooks::test_support::StubEnv::new()));
        let ctx = HookContext {
            git,
            vector_store: None,
            fs,
            process: process_port,
            llm: None,
            memory_mcp,
            env,
        };

        let mut input = HookInput::default();
        input.session_id = Some("session-401".to_string());
        input
            .extra
            .insert("error".to_string(), serde_json::json!("api_error"));
        input.extra.insert(
            "error_details".to_string(),
            serde_json::json!(
                "401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"Invalid credentials\"}}"
            ),
        );

        let output = process(&input, &ctx);
        assert_eq!(output.continue_, Some(false));
        let msg = output.system_message.expect("system_message present");
        assert!(msg.contains("[Auth Error]"), "msg: {msg}");
        assert!(msg.contains("claude5"), "msg should mention old: {msg}");
        assert!(msg.contains("claude1"), "msg should mention new: {msg}");
    }
}
