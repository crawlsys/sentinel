//! Bug Task Gate — require a `TaskCreate` after a bug is observed.
//!
//! When a tool result reveals a bug (cargo test FAILED, cargo `error[Exxxx]`,
//! Rust panic, …), this hook records pending-bug state for the current repo.
//! Until a `TaskCreate` (or `TaskUpdate`) fires with a bug-related keyword in
//! its subject/description, mutating tools are blocked. The intent is the same
//! as the user's complaint that "tasks are not always enforced" — discovering
//! a bug without filing it is exactly the case where a task gets skipped.
//!
//! Detection is conservative on purpose:
//!   - Patterns must appear in `tool_result` (`PostToolUse`), not chat text.
//!   - Only three high-confidence patterns trigger: `test result: FAILED`,
//!     `error[E0000]` codes, and Rust `panicked at`.
//!   - False positives nag the user; missed signals are recoverable.
//!
//! State file: `~/.claude/sentinel/state/pending-bug-{repo_hash}.json`.
//! TTL: 10 minutes — bug signals are time-sensitive.

use chrono::Utc;
use sentinel_domain::events::{HookEnvelope, HookInput, HookOutput};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use super::{FileSystemPort, HookContext};

/// 10-minute pending-bug TTL — long enough to file a task, short enough that
/// a stale signal doesn't deadlock a follow-up session.
const PENDING_BUG_TTL_SECS: i64 = 600;

/// Read-only / progress-toward-task-create tools that should never be blocked
/// by this gate. Mirrors the philosophy of `skill_invocation_gate`: the gate
/// must never refuse to let the model do the very thing that satisfies it.
const ALLOWED_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LSP",
    "WebSearch",
    "WebFetch",
    "ToolSearch",
    "Skill",
    "TaskList",
    "TaskGet",
    "TaskCreate",
    "TaskUpdate",
    "TaskOutput",
    "mcp__sequential-thinking__sequentialthinking",
];

/// Keywords that indicate a `TaskCreate` / `TaskUpdate` is filing the bug. Match
/// case-insensitively against subject + description fields.
const BUG_KEYWORDS: &[&str] = &[
    "bug",
    "fix",
    "error",
    "regression",
    "broken",
    "failure",
    "failing",
    "panic",
    "crash",
];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PendingBugState {
    first_seen_at: String,
    evidence: String,
    repo_root: String,
}

/// 4-byte stable hash of the repo root for use in the state filename.
fn repo_hash(repo_root: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_root.as_bytes());
    hasher.finalize()[..4]
        .iter()
        .fold(String::new(), |mut s, b| { use std::fmt::Write; write!(s, "{b:02x}").unwrap(); s })
}

fn state_file(fs: &dyn FileSystemPort, repo_root: &str) -> Option<PathBuf> {
    let dir = fs
        .home_dir()?
        .join(".claude")
        .join("sentinel")
        .join("state");
    let _ = fs.create_dir_all(&dir);
    Some(dir.join(format!("pending-bug-{}.json", repo_hash(repo_root))))
}

fn load_state(fs: &dyn FileSystemPort, repo_root: &str) -> Option<PendingBugState> {
    let path = state_file(fs, repo_root)?;
    let content = fs.read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn clear_state(fs: &dyn FileSystemPort, repo_root: &str) {
    if let Some(path) = state_file(fs, repo_root) {
        // Best-effort: write empty content so subsequent loads return None.
        let _ = fs.write(&path, b"");
    }
}

fn is_stale(first_seen_at: &str) -> bool {
    let parsed = chrono::DateTime::parse_from_rfc3339(first_seen_at);
    match parsed {
        Ok(dt) => Utc::now().signed_duration_since(dt).num_seconds() > PENDING_BUG_TTL_SECS,
        Err(_) => true,
    }
}

/// Pull a string-like `tool_result` out of a `HookInput`. Tool results can be
/// either a top-level string or an object with a `content` field; check both.
fn extract_tool_result_text(input: &HookInput) -> Option<String> {
    let result = input.tool_result.as_ref()?;
    if let Some(s) = result.as_str() {
        return Some(s.to_string());
    }
    if let Some(content) = result.get("content").and_then(|c| c.as_str()) {
        return Some(content.to_string());
    }
    // Some adapters wrap in {"output": "..."}; try that too.
    if let Some(output) = result.get("output").and_then(|c| c.as_str()) {
        return Some(output.to_string());
    }
    None
}

/// Detect a high-confidence bug signal in a tool-result string. Returns the
/// matched evidence snippet (truncated) so it can be stored in state for the
/// reminder message.
fn detect_bug_signal(text: &str) -> Option<String> {
    // 1. Cargo test failure summary.
    if text.contains("test result: FAILED") {
        return Some(snippet_around(text, "test result: FAILED"));
    }
    // 2. Cargo / rustc compile error with code (`error[E0277]: ...`).
    if let Some(idx) = find_error_code(text) {
        return Some(text[idx..].lines().next().unwrap_or("").to_string());
    }
    // 3. Rust panic.
    if text.contains("panicked at") {
        return Some(snippet_around(text, "panicked at"));
    }
    None
}

/// Find `error[E\d+]:` in `text` and return the byte offset of the `e`.
/// Hand-rolled rather than pulling regex into the hot path of every Bash result.
fn find_error_code(text: &str) -> Option<usize> {
    let needle = "error[E";
    let mut search_from = 0usize;
    while let Some(rel) = text[search_from..].find(needle) {
        let abs = search_from + rel;
        let after = &text[abs + needle.len()..];
        // Validate: digits then `]:`.
        let digit_count = after.chars().take_while(char::is_ascii_digit).count();
        if digit_count > 0 {
            let after_digits = &after[digit_count..];
            if after_digits.starts_with("]:") || after_digits.starts_with("] ") {
                return Some(abs);
            }
        }
        search_from = abs + needle.len();
    }
    None
}

fn snippet_around(text: &str, marker: &str) -> String {
    if let Some(idx) = text.find(marker) {
        // Return up to 200 chars centered on the marker for storage.
        let start = idx.saturating_sub(40);
        let end = (idx + marker.len() + 160).min(text.len());
        text[start..end]
            .lines()
            .next()
            .unwrap_or(marker)
            .to_string()
    } else {
        marker.to_string()
    }
}

/// True when `tool_input` for a `TaskCreate` / `TaskUpdate` references a bug-
/// related keyword in its subject or description fields.
fn task_input_mentions_bug(input: &HookInput) -> bool {
    let Some(ti) = input.tool_input.as_ref() else {
        return false;
    };
    let subject = ti.get("subject").and_then(|v| v.as_str()).unwrap_or("");
    let desc = ti.get("description").and_then(|v| v.as_str()).unwrap_or("");
    let combined = format!("{subject} {desc}").to_lowercase();
    BUG_KEYWORDS.iter().any(|k| combined.contains(k))
}

/// `PostToolUse` handler — scans tool output for bug signals; clears state when
/// a `TaskCreate` / `TaskUpdate` references a bug keyword.
pub fn process_posttool(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let repo_root = match ctx.git.repo_root(cwd) {
        Some(r) => r,
        None => return HookOutput::allow(),
    };

    let tool_name = input.tool_name.as_deref().unwrap_or("");

    // Clear state on a TaskCreate/TaskUpdate that names the bug.
    if matches!(tool_name, "TaskCreate" | "TaskUpdate") && task_input_mentions_bug(input) {
        clear_state(ctx.fs, &repo_root);
        return HookOutput::allow();
    }

    // Otherwise: scan the tool result for bug signals.
    let Some(text) = extract_tool_result_text(input) else {
        return HookOutput::allow();
    };
    let Some(evidence) = detect_bug_signal(&text) else {
        return HookOutput::allow();
    };

    // Don't overwrite an already-pending entry — preserve the original
    // first_seen_at so the TTL countdown isn't reset by every rerun.
    if load_state(ctx.fs, &repo_root).is_some() {
        return HookOutput::allow();
    }

    let state = PendingBugState {
        first_seen_at: Utc::now().to_rfc3339(),
        evidence,
        repo_root: repo_root.clone(),
    };
    if let Some(path) = state_file(ctx.fs, &repo_root) {
        if let Ok(json) = serde_json::to_string(&state) {
            let _ = ctx.fs.write(&path, json.as_bytes());
        }
    }
    HookOutput::allow()
}

/// `PreToolUse` handler — block mutating tools when a pending bug is recorded
/// for this repo. Allowlists Task* / Read / Skill / sequential-thinking so
/// the model can satisfy the gate.
pub fn process_pretool(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let repo_root = match ctx.git.repo_root(cwd) {
        Some(r) => r,
        None => return HookOutput::allow(),
    };

    let state = match load_state(ctx.fs, &repo_root) {
        Some(s) => s,
        None => return HookOutput::allow(),
    };

    if is_stale(&state.first_seen_at) {
        clear_state(ctx.fs, &repo_root);
        return HookOutput::allow();
    }

    let tool_name = input.tool_name.as_deref().unwrap_or("");
    if ALLOWED_TOOLS.contains(&tool_name) {
        return HookOutput::allow();
    }

    let envelope = HookEnvelope::block(
        "Bug Task Gate",
        format!(
            "A bug signal was observed (`{}`). File a `TaskCreate` referencing \
             the bug (subject or description must include one of: bug, fix, \
             error, regression, broken, failure, failing, panic, crash) before \
             using `{}`. Auto-clears after 10 minutes.",
            state.evidence.trim(),
            tool_name,
        ),
    );
    HookOutput::block(envelope.render())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_test_failure() {
        let out = "running 5 tests\n\
                   test foo ... ok\n\
                   test bar ... FAILED\n\
                   test result: FAILED. 4 passed; 1 failed\n";
        assert!(detect_bug_signal(out).is_some());
    }

    #[test]
    fn test_detect_compile_error_with_code() {
        let out = "error[E0277]: the trait bound `T: Foo` is not satisfied\n";
        assert!(detect_bug_signal(out).is_some());
    }

    #[test]
    fn test_detect_panic() {
        let out = "thread 'main' panicked at 'unwrap on None', src/main.rs:42:5";
        assert!(detect_bug_signal(out).is_some());
    }

    #[test]
    fn test_no_signal_for_passing_tests() {
        let out = "test result: ok. 12 passed; 0 failed";
        assert!(detect_bug_signal(out).is_none());
    }

    #[test]
    fn test_no_signal_for_word_error_alone() {
        // The word "error" without an `error[Exxxx]` code shouldn't trigger —
        // too noisy. Documentation, comments, normal log lines.
        let out = "fn parse(s: &str) -> Result<T, ParseError> { ... }";
        assert!(detect_bug_signal(out).is_none());
    }

    #[test]
    fn test_find_error_code_handles_no_match() {
        assert!(find_error_code("nothing to see here").is_none());
        assert!(find_error_code("error[Exx]: bad code").is_none());
        assert!(find_error_code("error[E]: missing digits").is_none());
    }

    #[test]
    fn test_find_error_code_finds_after_other_text() {
        // The error[Eddd] needle should still match when buried in output.
        let text = "Compiling foo\n  --> src/main.rs:1:1\nerror[E0432]: unresolved import";
        assert!(find_error_code(text).is_some());
    }

    #[test]
    fn test_task_input_mentions_bug_via_subject() {
        let input = HookInput {
            tool_input: Some(serde_json::json!({
                "subject": "Fix the regression in foo",
                "description": "see test failure"
            })),
            ..Default::default()
        };
        assert!(task_input_mentions_bug(&input));
    }

    #[test]
    fn test_task_input_mentions_bug_via_description() {
        let input = HookInput {
            tool_input: Some(serde_json::json!({
                "subject": "Refactor parser",
                "description": "Address the panic in unwrap chain"
            })),
            ..Default::default()
        };
        assert!(task_input_mentions_bug(&input));
    }

    #[test]
    fn test_task_input_does_not_mention_bug() {
        let input = HookInput {
            tool_input: Some(serde_json::json!({
                "subject": "Add new metric",
                "description": "Track p95 latency"
            })),
            ..Default::default()
        };
        assert!(!task_input_mentions_bug(&input));
    }

    #[test]
    fn test_is_stale_returns_true_for_old() {
        let old = (Utc::now() - chrono::Duration::seconds(PENDING_BUG_TTL_SECS + 60)).to_rfc3339();
        assert!(is_stale(&old));
    }

    #[test]
    fn test_is_stale_returns_false_for_recent() {
        assert!(!is_stale(&Utc::now().to_rfc3339()));
    }

    #[test]
    fn test_extract_tool_result_text_handles_string() {
        let input = HookInput {
            tool_result: Some(serde_json::Value::String("hello".to_string())),
            ..Default::default()
        };
        assert_eq!(extract_tool_result_text(&input).as_deref(), Some("hello"));
    }

    #[test]
    fn test_extract_tool_result_text_handles_content_field() {
        let input = HookInput {
            tool_result: Some(serde_json::json!({"content": "wrapped result"})),
            ..Default::default()
        };
        assert_eq!(
            extract_tool_result_text(&input).as_deref(),
            Some("wrapped result"),
        );
    }

    #[test]
    fn test_repo_hash_is_stable() {
        let h1 = repo_hash("/repos/sentinel");
        let h2 = repo_hash("/repos/sentinel");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_pretool_allows_allowlisted_tools_even_when_pending() {
        // Sanity: TaskCreate / Read / Skill must not be blocked even if a
        // bug is pending — otherwise the gate refuses to let the model
        // satisfy it.
        for t in ["TaskCreate", "Read", "Skill", "Glob", "Grep"] {
            assert!(ALLOWED_TOOLS.contains(&t), "{t} must be allowlisted");
        }
    }
}
