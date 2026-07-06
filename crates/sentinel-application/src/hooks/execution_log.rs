//! Execution Log Capture
//!
//! Extracts `[RUN]`, `[STEP N]`, and `[PHASE N]` markers from the
//! transcript and persists them to `~/.claude/metrics/execution-log.jsonl`.
//! Uses offset tracking to avoid reprocessing lines across invocations.

use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use std::path::PathBuf;

use super::{
    concrete_input_session_id as concrete_session_id, session_path_component, FileSystemPort,
    HookContext,
};

/// Resolve `~/.claude/sentinel/metrics` directory, creating it if needed.
fn metrics_dir(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = super::metrics_dir(&home);
    fs.create_dir_all(&dir).ok()?;
    Some(dir)
}

// Session-id validation centralized in `super::session_path_component` /
// `super::concrete_input_session_id` (imported at top, latter aliased). The
// canonical validator adds path-traversal (`..`) rejection the inline copy
// lacked.

fn offset_path(session_id: &str) -> Option<PathBuf> {
    let session_id = session_path_component(session_id)?;
    Some(std::env::temp_dir().join(format!("claude-execlog-offset-{session_id}")))
}

/// Read the byte-offset marker so we only process new transcript lines.
fn read_offset(fs: &dyn FileSystemPort, session_id: &str) -> usize {
    let Some(path) = offset_path(session_id) else {
        return 0;
    };
    fs.read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Persist the line-offset marker for next invocation.
fn write_offset(fs: &dyn FileSystemPort, session_id: &str, offset: usize) {
    let Some(path) = offset_path(session_id) else {
        return;
    };
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
/// Uses sentinel's protected telemetry dir instead of world-writable `temp_dir()`. (Attack #51)
fn current_skill(fs: &dyn FileSystemPort) -> String {
    let dir = fs.home_dir().map_or_else(std::env::temp_dir, |h| {
        h.join(".claude").join("sentinel").join("telemetry")
    });
    let path = dir.join("claude-current-skill");
    fs.read_to_string(&path)
        .map_or_else(|_| "none".to_string(), |s| s.trim().to_string())
}

/// Process the execution-log hook event (Stop).
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let Some(session_id) = concrete_session_id(input) else {
        return HookOutput::allow();
    };

    let metrics = match metrics_dir(ctx.fs) {
        Some(d) => d,
        None => return HookOutput::allow(),
    };

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

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, TestHomeFs};
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
    fn missing_session_does_not_write_unknown_offset_or_log() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let unknown_offset = std::env::temp_dir().join("claude-execlog-offset-unknown");
        let log_path = super::metrics_dir(&fs)
            .expect("metrics dir")
            .join("execution-log.jsonl");
        let _ = std::fs::remove_file(&unknown_offset);
        let _ = std::fs::remove_file(&log_path);

        let transcript = make_transcript(&[json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "[RUN] linear | run_id: run-123 | session: missing"
                }]
            }
        })]);

        let input = HookInput {
            transcript_path: Some(transcript.path().to_string_lossy().to_string()),
            session_id: None,
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(!unknown_offset.exists());
        assert!(!log_path.exists());
    }

    #[test]
    fn synthetic_unknown_session_does_not_write_offset_or_log() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let raw_offset = std::env::temp_dir().join("claude-execlog-offset- unknown ");
        let trimmed_offset = std::env::temp_dir().join("claude-execlog-offset-unknown");
        let log_path = super::metrics_dir(&fs)
            .expect("metrics dir")
            .join("execution-log.jsonl");
        let _ = std::fs::remove_file(&raw_offset);
        let _ = std::fs::remove_file(&trimmed_offset);
        let _ = std::fs::remove_file(&log_path);

        let transcript = make_transcript(&[json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "[RUN] linear | run_id: run-123 | session: synthetic"
                }]
            }
        })]);

        let input = HookInput {
            transcript_path: Some(transcript.path().to_string_lossy().to_string()),
            session_id: Some(" unknown ".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(!raw_offset.exists());
        assert!(!trimmed_offset.exists());
        assert!(!log_path.exists());
    }

    #[test]
    fn concrete_session_writes_scoped_log_and_offset() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let session_id = format!("test-exec-real-{}", std::process::id());
        let offset = offset_path(&session_id).expect("safe offset path");
        let log_path = super::metrics_dir(&fs)
            .expect("metrics dir")
            .join("execution-log.jsonl");
        let _ = std::fs::remove_file(&offset);
        let _ = std::fs::remove_file(&log_path);

        let transcript = make_transcript(&[json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "[RUN] linear | run_id: run-123 | session: concrete"
                }]
            }
        })]);

        let input = HookInput {
            transcript_path: Some(transcript.path().to_string_lossy().to_string()),
            session_id: Some(session_id.clone()),
            cwd: Some("/repo".to_string()),
            ..Default::default()
        };
        let output = process(&input, &ctx);
        let log = std::fs::read_to_string(&log_path).expect("execution log");
        let entry: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();

        assert!(output.blocked.is_none());
        assert_eq!(entry["session_id"], session_id);
        assert_eq!(entry["type"], "run");
        assert!(offset.exists());
        let _ = std::fs::remove_file(offset);
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
