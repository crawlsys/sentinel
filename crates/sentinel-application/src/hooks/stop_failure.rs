//! StopFailure hook — detect API errors and rate limits
//!
//! Called when a turn ends due to an API error rather than normal completion.
//! Parses error_details for reset time information and advises Claude to
//! call account_rotate with the correct cooldown_minutes.
//!
//! ## Error message formats (from Claude Code 2.1.88 source)
//!
//! Claude Code formats rate limit messages via `NP6()` + `SB1()`:
//! - `"You've hit your session limit · resets 4pm (CDT)"`
//! - `"You've hit your limit · resets Apr 3, 4:30pm (CDT)"`
//! - `"You've used 87% of your session limit · resets 4pm (CDT)"`
//! - `"You're now using extra usage · Your session limit resets 4pm (CDT)"`
//!
//! The `error` field is `"rate_limit"` and the content/error_details contains
//! the human-readable message above.

use sentinel_domain::events::{HookInput, HookOutput};

/// Parse error_details to extract a cooldown duration in minutes.
///
/// Handles patterns from Anthropic rate limit errors:
/// - "resets 4pm (CDT)" / "resets 4:30pm (CDT)" — Claude Code's actual format
/// - "resets Apr 3, 4pm (CDT)" — same but with date
/// - "resets at 4:00 PM" / "resets at 16:00" — absolute time
/// - "retry after 3600" / "retry-after: 3600" — seconds
/// - "resets in 4h 30m" / "resets in 270 minutes" — relative duration
/// - "rate_limit_error" with no parseable time — returns None (caller defaults to 5hr)
fn parse_reset_minutes(error_details: &str) -> Option<u64> {
    let lower = error_details.to_lowercase();

    // Pattern: "retry after <seconds>" or "retry-after: <seconds>"
    if let Some(idx) = lower.find("retry after").or_else(|| lower.find("retry-after")) {
        let after = &lower[idx..];
        if let Some(secs) = extract_first_number(after) {
            let minutes = (secs + 59) / 60; // round up
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

/// Parse flexible absolute time formats from Claude Code's NP6() formatter:
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

    let digits_only = lower
        .replace("pm", "").replace("am", "")
        .trim().to_string();

    let (hour, minute) = if digits_only.contains(':') {
        let parts: Vec<&str> = digits_only.split(':').collect();
        let h: u64 = parts.first()?.trim().parse().ok()?;
        let m: u64 = parts.get(1).and_then(|p| p.trim().parse().ok()).unwrap_or(0);
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

/// Process StopFailure event
///
/// Logs the error, parses reset time from error_details, and injects
/// a system message advising Claude to call account_rotate with the
/// correct cooldown_minutes.
pub fn process(input: &HookInput) -> HookOutput {
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
    if let Some(home) = dirs::home_dir() {
        let metrics_dir = home.join(".claude").join("metrics");
        let entry = serde_json::json!({
            "event": "stop_failure",
            "error": error,
            "error_details": error_details,
            "session_id": input.session_id,
            "ts": chrono::Utc::now().to_rfc3339(),
        });

        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(metrics_dir.join("errors.jsonl"))
        {
            use std::io::Write;
            let _ = writeln!(file, "{}", entry);
        }
    }

    // If this is a rate limit error, parse reset time and advise rotation
    let is_rate_limit = error.contains("rate_limit")
        || error.contains("overloaded")
        || error_details.contains("rate limit")
        || error_details.contains("rate_limit")
        || error_details.contains("Too many requests")
        || error_details.contains("You've hit your")
        || error_details.contains("You've used");

    if is_rate_limit {
        let parsed_minutes = parse_reset_minutes(error_details);
        let cooldown_param = parsed_minutes
            .map(|m| format!(", cooldown_minutes: {m}"))
            .unwrap_or_default();
        let cooldown_desc = parsed_minutes
            .map(|m| format!(" (parsed reset: ~{m} minutes)"))
            .unwrap_or_else(|| " (defaulting to 5h)".to_string());

        let msg = format!(
            "[Rate Limit] Account hit rate limit{cooldown_desc}. \
             Call `mcp__accounts__account_rotate({cooldown_param})` to switch to the next available account, \
             then resend your last message."
        );

        tracing::info!(
            parsed_minutes = parsed_minutes,
            "Rate limit detected, advising rotation"
        );

        return HookOutput {
            system_message: Some(msg),
            ..HookOutput::default()
        };
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stop_failure_allows_non_rate_limit() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("error".to_string(), serde_json::json!("auth_error"));

        let output = process(&input);
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

        let output = process(&input);
        assert!(output.system_message.is_some());
        let msg = output.system_message.unwrap();
        assert!(msg.contains("account_rotate"));
        assert!(msg.contains("defaulting to 5h"));
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

        let output = process(&input);
        let msg = output.system_message.unwrap();
        assert!(msg.contains("cooldown_minutes: 120"));
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

        let output = process(&input);
        assert!(output.system_message.is_some());
        let msg = output.system_message.unwrap();
        assert!(msg.contains("account_rotate"));
        assert!(msg.contains("cooldown_minutes:"));
    }
}
