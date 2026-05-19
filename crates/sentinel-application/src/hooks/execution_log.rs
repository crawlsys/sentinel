//! Execution Log Capture
//!
//! Extracts `[RUN]`, `[STEP N]`, and `[PHASE N]` markers from the
//! transcript and persists them to `~/.claude/metrics/execution-log.jsonl`.
//! Uses offset tracking to avoid reprocessing lines across invocations.

use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use std::path::PathBuf;

use super::{FileSystemPort, HookContext};

/// Resolve `~/.claude/sentinel/metrics` directory, creating it if needed.
fn metrics_dir(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = super::metrics_dir(&home);
    fs.create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Read the byte-offset marker so we only process new transcript lines.
fn read_offset(fs: &dyn FileSystemPort, session_id: &str) -> usize {
    let path = std::env::temp_dir().join(format!("claude-execlog-offset-{session_id}"));
    fs.read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Persist the line-offset marker for next invocation.
fn write_offset(fs: &dyn FileSystemPort, session_id: &str, offset: usize) {
    let path = std::env::temp_dir().join(format!("claude-execlog-offset-{session_id}"));
    let _ = fs.write(&path, offset.to_string().as_bytes());
}

/// Classify a marker line into its type and optional step/phase number.
fn classify_marker(line: &str) -> (&'static str, Option<String>) {
    if line.starts_with("[RUN]") {
        return ("run", None);
    }

    let step_re = Regex::new(r"^\[STEP\s+(\d+[a-z]?)\]").expect("valid regex");
    if let Some(caps) = step_re.captures(line) {
        return ("step", Some(caps[1].to_string()));
    }

    let phase_re = Regex::new(r"^\[PHASE\s+([\d.]+)\]").expect("valid regex");
    if let Some(caps) = phase_re.captures(line) {
        return ("phase", Some(caps[1].to_string()));
    }

    ("unknown", None)
}

/// Extract the `[context]` tag from a marker line, e.g. `[STEP 1] [plan] ...` -> `"plan"`.
fn extract_context(line: &str) -> Option<String> {
    let re = Regex::new(r"\]\s+\[([^\]]+)\]\s+").expect("valid regex");
    re.captures(line).map(|c| c[1].to_string())
}

/// Read the current skill from the telemetry state file written by skill-router.
/// Uses sentinel's protected telemetry dir instead of world-writable temp_dir(). (Attack #51)
fn current_skill(fs: &dyn FileSystemPort) -> String {
    let dir = fs
        .home_dir()
        .map(|h| h.join(".claude").join("sentinel").join("telemetry"))
        .unwrap_or_else(|| std::env::temp_dir());
    let path = dir.join("claude-current-skill");
    fs.read_to_string(&path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "none".to_string())
}

/// Process the execution-log hook event (Stop).
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let metrics = match metrics_dir(ctx.fs) {
        Some(d) => d,
        None => return HookOutput::allow(),
    };

    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd = input.cwd.as_deref().unwrap_or(".");

    let transcript_path = match &input.transcript_path {
        Some(p) if !p.is_empty() => p.clone(),
        _ => return HookOutput::allow(),
    };

    // Read transcript, extract assistant text from new lines only
    let transcript = match ctx
        .fs
        .read_to_string(std::path::Path::new(&transcript_path))
    {
        Ok(t) => t,
        Err(_) => return HookOutput::allow(),
    };

    let lines: Vec<&str> = transcript.lines().collect();
    let total_lines = lines.len();
    let last_offset = read_offset(ctx.fs, session_id);
    let start_idx = last_offset.min(total_lines);

    let mut text_chunks: Vec<String> = Vec::new();
    for line in &lines[start_idx..] {
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
            if entry.get("type").and_then(|v| v.as_str()) == Some("assistant") {
                if let Some(content) = entry
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in content {
                        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                text_chunks.push(text.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    write_offset(ctx.fs, session_id, total_lines);

    let message = text_chunks.join("\n");
    if message.is_empty() {
        return HookOutput::allow();
    }

    // Extract markers: [RUN], [STEP N], [PHASE N.N]
    // Also handle LOG: prefix and markdown/HTML comment prefixes
    let patterns = [
        Regex::new(r"(?m)^(?:#\s*|<!--\s*)?(?:LOG:\s*)?\[RUN\]\s+\S.+").expect("valid regex"),
        Regex::new(r"(?m)^(?:#\s*|<!--\s*)?(?:LOG:\s*)?\[STEP\s+\d+[a-z]?\]\s+.+")
            .expect("valid regex"),
        Regex::new(r"(?m)^(?:#\s*|<!--\s*)?(?:LOG:\s*)?\[PHASE\s+[-\d.]+\]\s+.+")
            .expect("valid regex"),
    ];

    let prefix_re = Regex::new(r"^(?:#\s*|<!--\s*)?(?:LOG:\s*)?").expect("valid regex");
    let suffix_re = Regex::new(r"\s*-->$").expect("valid regex");

    let mut log_lines: Vec<String> = Vec::new();
    for pattern in &patterns {
        for mat in pattern.find_iter(&message) {
            let cleaned = prefix_re.replace(mat.as_str(), "");
            let cleaned = suffix_re.replace(&cleaned, "");
            log_lines.push(cleaned.trim().to_string());
        }
    }

    if log_lines.is_empty() {
        return HookOutput::allow();
    }

    // Build JSONL entries
    let skill = current_skill(ctx.fs);
    let timestamp = chrono::Utc::now().to_rfc3339();
    let log_file = metrics.join("execution-log.jsonl");

    let mut all_lines = String::new();
    for line in &log_lines {
        let (marker_type, number) = classify_marker(line);
        let context = extract_context(line);

        let entry = serde_json::json!({
            "type": marker_type,
            "phase": if marker_type == "phase" { number.clone() } else { None },
            "step": if marker_type == "step" { number.clone() } else { None },
            "context": context,
            "line": line,
            "skill": skill,
            "session_id": session_id,
            "cwd": cwd,
            "ts": timestamp,
        });
        all_lines.push_str(&serde_json::to_string(&entry).unwrap_or_default());
        all_lines.push('\n');
    }

    let _ = ctx.fs.append(&log_file, all_lines.as_bytes());

    // Notify the daemon-hosted legatus (if any) that this session
    // wrapped a run. We only fire when there was at least one
    // execution marker — otherwise the Stop is a non-event for the
    // operator (e.g. background tooling, first-ever invocation).
    // Summary is the last marker line, which is whatever the
    // session most recently surfaced as visible work.
    let summary = log_lines.last().map(|s| truncate_summary(s, 140));
    crate::legatus_client::escalate_fire_and_forget(
        sentinel_legatus::EscalationKind::Completed {
            summary: summary.clone(),
        },
    );

    // Per-instruction Result reporting: for every operator-relayed
    // instruction the consul_inbox hook drained during this
    // session, fire an InstructionResult tied to its
    // instruction_id. Outcome is classified from observed turn
    // signals + the assistant's reply text via `classify_outcome`:
    //   - PermissionDenied during the turn → Declined { tool(s) }
    //   - reply contains a deferral phrase → Deferred { waiting_on }
    //   - otherwise → Success
    // This is the Stop path (API error → StopFailure handles its
    // own Failure classification), so no api_error is passed.
    // `message` is the concatenated assistant text from this turn
    // — captured above for execution-marker extraction; reused
    // here as the deferral-detection corpus.
    let pending = crate::legatus_client::take_pending_instructions(session_id);
    let signals = crate::legatus_client::take_turn_signals(session_id);
    // Per-instruction tool-call attribution (Item E). One drain
    // per turn, shared across all instructions queued by the
    // same consul_inbox fire. Per-turn granularity until we have
    // a model-cooperative way to split tool calls per instruction.
    let tool_calls = crate::legatus_client::take_tool_calls(session_id);
    let enriched_summary = enrich_summary_with_tool_calls(summary.as_deref(), &tool_calls);
    for instruction_id in pending {
        let outcome =
            crate::legatus_client::classify_outcome(&signals, None, Some(message.as_str()));
        crate::legatus_client::report_result_fire_and_forget(
            instruction_id,
            outcome,
            enriched_summary.clone(),
        );
    }

    HookOutput::allow()
}

/// Prepend a short `[tools: A, B, C]` prefix to the existing
/// summary when at least one tool was recorded during the turn.
/// Per-turn (not per-instruction) granularity — see
/// [`crate::legatus_client::take_tool_calls`] docs for the
/// attribution caveat.
///
/// Duplicate tool names are deduplicated while preserving
/// first-seen order so the operator's view stays readable
/// (e.g. a 20× `Bash`-then-`Edit`-then-`Bash` sequence collapses
/// to `[tools: Bash, Edit]` rather than a noisy 60-char prefix).
/// Up to 6 distinct tools, then `…+N more`. Output capped at the
/// same 140-char ceiling as the base summary.
fn enrich_summary_with_tool_calls(base: Option<&str>, tool_calls: &[String]) -> Option<String> {
    if tool_calls.is_empty() {
        return base.map(str::to_owned);
    }
    let mut seen: Vec<&str> = Vec::new();
    for name in tool_calls {
        let s = name.as_str();
        if !seen.iter().any(|existing| *existing == s) {
            seen.push(s);
        }
    }
    let prefix = if seen.len() <= 6 {
        format!("[tools: {}]", seen.join(", "))
    } else {
        format!(
            "[tools: {}, …+{} more]",
            seen.iter().take(6).copied().collect::<Vec<_>>().join(", "),
            seen.len() - 6,
        )
    };
    let combined = match base {
        Some(s) if !s.trim().is_empty() => format!("{prefix} {s}"),
        _ => prefix,
    };
    Some(truncate_summary(&combined, 140))
}

fn truncate_summary(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_owned()
    } else {
        let mut out: String = trimmed.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn make_transcript(entries: &[serde_json::Value]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for entry in entries {
            writeln!(file, "{}", serde_json::to_string(entry).unwrap()).unwrap();
        }
        file
    }

    #[test]
    fn test_no_transcript_path() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn enrich_summary_no_tools_returns_base_unchanged() {
        assert_eq!(
            enrich_summary_with_tool_calls(Some("ship feature X"), &[]),
            Some("ship feature X".to_owned()),
        );
        assert_eq!(enrich_summary_with_tool_calls(None, &[]), None);
    }

    #[test]
    fn enrich_summary_deduplicates_repeated_tool_names() {
        // Bash, Edit, Bash, Edit, Bash → "[tools: Bash, Edit]"
        let summary = enrich_summary_with_tool_calls(
            Some("did work"),
            &[
                "Bash".into(),
                "Edit".into(),
                "Bash".into(),
                "Edit".into(),
                "Bash".into(),
            ],
        );
        assert_eq!(summary.as_deref(), Some("[tools: Bash, Edit] did work"));
    }

    #[test]
    fn enrich_summary_caps_at_six_distinct_tools() {
        let tools = vec![
            "A".into(),
            "B".into(),
            "C".into(),
            "D".into(),
            "E".into(),
            "F".into(),
            "G".into(),
            "H".into(),
        ];
        let summary = enrich_summary_with_tool_calls(None, &tools);
        let body = summary.expect("non-empty when tool calls present");
        assert!(
            body.contains("…+2 more"),
            "expected truncated tail, got {body:?}",
        );
        assert!(body.contains("A, B, C, D, E, F"));
        assert!(!body.contains(", G"));
    }

    #[test]
    fn enrich_summary_falls_back_to_prefix_only_when_base_blank() {
        let summary = enrich_summary_with_tool_calls(Some("   "), &["Read".into()]);
        assert_eq!(summary.as_deref(), Some("[tools: Read]"));
    }

    #[test]
    fn test_empty_transcript() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let input = HookInput {
            transcript_path: Some(file.path().to_string_lossy().to_string()),
            session_id: Some("test-exec-empty".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_extracts_run_marker() {
        let transcript = make_transcript(&[json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "[RUN] linear | run_id: run-123 | session: sess-1"
                }]
            }
        })]);

        let input = HookInput {
            transcript_path: Some(transcript.path().to_string_lossy().to_string()),
            session_id: Some("test-exec-run".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_extracts_step_and_phase() {
        let transcript = make_transcript(&[json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "[STEP 1] [plan] Starting implementation\n[PHASE 1.2] [review] Code reviewed\n[RUN] test COMPLETE | 3 steps"
                }]
            }
        })]);

        let input = HookInput {
            transcript_path: Some(transcript.path().to_string_lossy().to_string()),
            session_id: Some("test-exec-multi".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_classify_marker_run() {
        let (t, n) = classify_marker("[RUN] linear | run_id: abc");
        assert_eq!(t, "run");
        assert!(n.is_none());
    }

    #[test]
    fn test_classify_marker_step() {
        let (t, n) = classify_marker("[STEP 3a] [context] doing something");
        assert_eq!(t, "step");
        assert_eq!(n.as_deref(), Some("3a"));
    }

    #[test]
    fn test_classify_marker_phase() {
        let (t, n) = classify_marker("[PHASE 2.1] [review] checking code");
        assert_eq!(t, "phase");
        assert_eq!(n.as_deref(), Some("2.1"));
    }

    #[test]
    fn test_extract_context_present() {
        let ctx = extract_context("[STEP 1] [plan] Starting");
        assert_eq!(ctx.as_deref(), Some("plan"));
    }

    #[test]
    fn test_extract_context_absent() {
        let ctx = extract_context("[RUN] linear complete");
        assert!(ctx.is_none());
    }
}
