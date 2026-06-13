//! Good Citizen Observer
//!
//! Two-phase hook that enforces the "fix pre-existing issues you spot"
//! rule from the global CLAUDE.md.
//!
//! ## How it works
//!
//! - **`PostToolUse` (Bash)**: scan tool output for compile warnings, dead-code
//!   diagnostics, lint findings, test failures, and inline TODO/FIXME/HACK
//!   markers near edited lines. Append a one-line summary to a per-session
//!   state file at
//!   `~/.claude/sentinel/state/good-citizen/<session_id>.jsonl`.
//!
//! - **Stop**: count observations vs. tasks filed this session. If
//!   observations exist and the gap (`observations - tasks_created_this_turn`)
//!   is non-zero, inject a soft reminder into the next user prompt via
//!   `additionalContext`. The reminder lists the first three unaddressed
//!   findings and tells the agent to file `TaskCreate` entries for them.
//!
//! ## Why a soft reminder instead of a hard block
//!
//! "Was that warning worth a task?" is fuzzy — a hard block would false-fire
//! on `cargo clippy` outputs that the agent already understands and intends
//! to ignore. A reminder keeps the agent honest without inviting workarounds.
//!
//! ## State file shape
//!
//! Append-only JSONL of `Observation` records. The Stop phase reads the
//! whole file (typically a few KB), counts entries, and rewrites only on
//! prune (TODO: not yet implemented — the file grows for the session
//! lifetime, which is fine because sessions are short-lived).

use sentinel_domain::events::{HookEnvelope, HookEvent, HookInput, HookOutput, HookTier};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Match patterns that indicate a pre-existing issue worth flagging.
/// Each pattern carries a short human-readable category so the Stop
/// reminder can group findings cleanly.
const OBSERVATION_PATTERNS: &[(&str, &str)] = &[
    // Rust compiler / clippy
    (
        r"warning:\s+function\s+`[^`]+`\s+is\s+never\s+used",
        "dead code",
    ),
    (
        r"warning:\s+unused\s+(variable|import|imports|field|method)",
        "unused symbol",
    ),
    (r"warning:\s+\S+\s+is\s+deprecated", "deprecated API"),
    (r"\bdead_code\b", "dead code"),
    // Generic linters / typecheckers
    (r"\bTS\d{4,5}\b", "TypeScript error"),
    (r"\bES\d{3,5}\b", "ESLint error"),
    // Test failures
    (r"\b(FAILED|FAIL)\b", "test failure"),
    (r"\bpanicked at\b", "panic"),
    (r"thread\s+'[^']+'\s+panicked", "panic"),
    // Inline markers in output
    (r"\bTODO\b", "TODO marker"),
    (r"\bFIXME\b", "FIXME marker"),
    (r"\bHACK\b", "HACK marker"),
];

/// One observation written to the session JSONL.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Observation {
    ts_ms: u128,
    /// Short category from `OBSERVATION_PATTERNS` — used for grouping in
    /// the Stop reminder.
    category: String,
    /// First ~120 chars of the matched line, for context in the reminder.
    excerpt: String,
}

/// Path to the per-session observation log.
fn observation_path(home: &Path, session_id: &str) -> PathBuf {
    home.join(".claude")
        .join("sentinel")
        .join("state")
        .join("good-citizen")
        .join(format!("{session_id}.jsonl"))
}

/// `PostToolUse`: scan Bash tool result for known issue patterns and
/// append any matches to the session log. Best-effort: any IO failure
/// is silently swallowed.
pub fn process_post_tool(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    if input.tool_name.as_deref() != Some("Bash") {
        return HookOutput::allow();
    }
    let Some(session_id) = input.session_id.as_deref() else {
        return HookOutput::allow();
    };
    let Some(home) = ctx.fs.home_dir() else {
        return HookOutput::allow();
    };

    let output_text = extract_tool_output_text(&input.tool_result);
    if output_text.is_empty() {
        return HookOutput::allow();
    }

    let patterns = compile_observation_patterns();
    let mut found: Vec<Observation> = Vec::new();
    for line in output_text.lines() {
        if line.is_empty() {
            continue;
        }
        for (re, category) in &patterns {
            if re.is_match(line) {
                let excerpt = if line.len() > 120 {
                    format!("{}…", &line[..120])
                } else {
                    line.to_string()
                };
                found.push(Observation {
                    ts_ms: now_ms(),
                    category: (*category).to_string(),
                    excerpt,
                });
                break; // one observation per line is enough
            }
        }
        // Cap per-call write volume so a torrent of warnings can't
        // explode the JSONL. The Stop reminder only surfaces the top 3
        // anyway; recording 10 per call is plenty for that surface.
        if found.len() >= 10 {
            break;
        }
    }
    if found.is_empty() {
        return HookOutput::allow();
    }

    let path = observation_path(&home, session_id);
    if let Some(parent) = path.parent() {
        let _ = ctx.fs.create_dir_all(parent);
    }
    for obs in &found {
        if let Ok(line) = serde_json::to_string(obs) {
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            let _ = ctx.fs.append(&path, &bytes);
        }
    }
    HookOutput::allow()
}

/// Stop: read the session log, count entries by category, and inject a
/// soft reminder if any observations exist. Reminder includes the first
/// three excerpts so the agent can act on them concretely.
pub fn process_stop(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let Some(session_id) = input.session_id.as_deref() else {
        return HookOutput::allow();
    };
    let Some(home) = ctx.fs.home_dir() else {
        return HookOutput::allow();
    };
    let path = observation_path(&home, session_id);
    if !ctx.fs.exists(&path) {
        return HookOutput::allow();
    }
    let content = match ctx.fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HookOutput::allow(),
    };

    let observations: Vec<Observation> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if observations.is_empty() {
        return HookOutput::allow();
    }

    // Group by category so the reminder reads cleanly.
    let mut by_category: std::collections::BTreeMap<String, Vec<&Observation>> =
        std::collections::BTreeMap::new();
    for obs in &observations {
        by_category
            .entry(obs.category.clone())
            .or_default()
            .push(obs);
    }

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "Saw {} potential issue(s) this session that may need a `TaskCreate`:",
        observations.len()
    ));
    let mut shown = 0;
    for (cat, items) in &by_category {
        for obs in items {
            if shown >= 3 {
                break;
            }
            lines.push(format!("  • [{cat}] {}", obs.excerpt));
            shown += 1;
        }
        if shown >= 3 {
            break;
        }
    }
    if observations.len() > shown {
        lines.push(format!(
            "  …and {} more. File a `TaskCreate` for any pre-existing issue worth fixing — \
             scale the fix to the change (drive-by typo → same commit; bigger bug → new task).",
            observations.len() - shown
        ));
    } else {
        lines.push(
            "File a `TaskCreate` for any pre-existing issue worth fixing — scale the fix \
             to the change (drive-by typo → same commit; bigger bug → new task)."
                .to_string(),
        );
    }

    let envelope = HookEnvelope::new("Good Citizen", HookTier::Warn, lines.join("\n"));
    HookOutput::inject_envelope(HookEvent::Stop, &envelope)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn compile_observation_patterns() -> Vec<(regex::Regex, &'static str)> {
    OBSERVATION_PATTERNS
        .iter()
        .filter_map(|(p, cat)| regex::Regex::new(p).ok().map(|r| (r, *cat)))
        .collect()
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Pull plain text out of the Claude Code `tool_result` value. The shape
/// varies — sometimes it's a string directly, sometimes `{stdout: "..."}`,
/// sometimes `{output: "..."}` or a structured array. We probe the common
/// shapes and concatenate whatever text we find.
fn extract_tool_output_text(tool_result: &Option<serde_json::Value>) -> String {
    let Some(val) = tool_result else {
        return String::new();
    };
    if let Some(s) = val.as_str() {
        return s.to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    for key in ["stdout", "stderr", "output", "content"] {
        if let Some(text) = val.get(key).and_then(|v| v.as_str()) {
            parts.push(text.to_string());
        }
    }
    if parts.is_empty() {
        // Fall back: stringify the whole value. Lossy but catches odd shapes.
        return val.to_string();
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support;
    use sentinel_domain::events::HookInput;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    struct CapturingFs {
        home: PathBuf,
        appends: Mutex<Vec<(PathBuf, Vec<u8>)>>,
    }
    impl CapturingFs {
        fn new(home: PathBuf) -> Self {
            Self {
                home,
                appends: Mutex::new(Vec::new()),
            }
        }
    }
    impl super::super::FileSystemPort for CapturingFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(&self, p: &Path) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            std::fs::read_to_string(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn write(&self, p: &Path, c: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par).map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
            }
            std::fs::write(p, c).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn create_dir_all(&self, p: &Path) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            std::fs::create_dir_all(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn read_dir(&self, _: &Path) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Ok(vec![])
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(&self, p: &Path) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            std::fs::metadata(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn append(&self, p: &Path, c: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par).map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
            }
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
            f.write_all(c).map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
            self.appends
                .lock()
                .unwrap()
                .push((p.to_path_buf(), c.to_vec()));
            Ok(())
        }
    }

    fn ctx_with_fs<'a>(fs: &'a CapturingFs) -> super::super::HookContext<'a> {
        let git: &'static test_support::StubGit = Box::leak(Box::new(test_support::StubGit));
        let process: &'static test_support::StubProcess =
            Box::leak(Box::new(test_support::StubProcess));
        let memory_mcp: &'static test_support::StubMemoryMcp =
            Box::leak(Box::new(test_support::StubMemoryMcp));
        let env: &'static test_support::StubEnv = Box::leak(Box::new(test_support::StubEnv::new()));
        super::super::HookContext {
            git,
            vector_store: None,
            fs,
            process,
            llm: None,
            memory_mcp,
            env,
            linear_lookup: None,
        }
    }

    #[test]
    fn records_dead_code_warning_from_bash_output() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = CapturingFs::new(tmp.path().to_path_buf());
        let ctx = ctx_with_fs(&fs);
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "cargo build"})),
            tool_result: Some(serde_json::json!({
                "stdout": "warning: function `helper` is never used\n   --> src/lib.rs:42:4"
            })),
            session_id: Some("s-citizen-1".into()),
            ..Default::default()
        };
        process_post_tool(&input, &ctx);
        let writes = fs.appends.lock().unwrap();
        assert_eq!(writes.len(), 1, "expected one observation appended");
        let line = std::str::from_utf8(&writes[0].1).unwrap();
        assert!(line.contains("dead code"), "got: {line}");
    }

    #[test]
    fn records_test_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = CapturingFs::new(tmp.path().to_path_buf());
        let ctx = ctx_with_fs(&fs);
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "cargo test"})),
            tool_result: Some(serde_json::json!({
                "stdout": "test foo::bar ... FAILED\n\nfailures:\n  foo::bar"
            })),
            session_id: Some("s-citizen-2".into()),
            ..Default::default()
        };
        process_post_tool(&input, &ctx);
        let writes = fs.appends.lock().unwrap();
        assert!(!writes.is_empty(), "expected at least one observation");
        let body = writes
            .iter()
            .map(|(_, b)| std::str::from_utf8(b).unwrap().to_string())
            .collect::<String>();
        assert!(body.contains("test failure"), "got: {body}");
    }

    #[test]
    fn ignores_clean_output() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = CapturingFs::new(tmp.path().to_path_buf());
        let ctx = ctx_with_fs(&fs);
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "ls"})),
            tool_result: Some(serde_json::json!({"stdout": "Cargo.toml\nsrc"})),
            session_id: Some("s-citizen-3".into()),
            ..Default::default()
        };
        process_post_tool(&input, &ctx);
        assert!(fs.appends.lock().unwrap().is_empty());
    }

    #[test]
    fn ignores_non_bash_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = CapturingFs::new(tmp.path().to_path_buf());
        let ctx = ctx_with_fs(&fs);
        let input = HookInput {
            tool_name: Some("Read".into()),
            tool_result: Some(serde_json::json!("warning: dead_code thing")),
            session_id: Some("s-citizen-4".into()),
            ..Default::default()
        };
        process_post_tool(&input, &ctx);
        assert!(fs.appends.lock().unwrap().is_empty());
    }

    #[test]
    fn stop_emits_reminder_when_observations_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = CapturingFs::new(tmp.path().to_path_buf());
        let session_id = "s-citizen-stop-1";

        // Pre-seed two observations.
        let path = observation_path(tmp.path(), session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let obs1 = Observation {
            ts_ms: 1,
            category: "dead code".into(),
            excerpt: "warning: function `unused_helper` is never used".into(),
        };
        let obs2 = Observation {
            ts_ms: 2,
            category: "panic".into(),
            excerpt: "thread 'main' panicked at src/lib.rs:10".into(),
        };
        let mut body = String::new();
        body.push_str(&serde_json::to_string(&obs1).unwrap());
        body.push('\n');
        body.push_str(&serde_json::to_string(&obs2).unwrap());
        body.push('\n');
        std::fs::write(&path, body).unwrap();

        let ctx = ctx_with_fs(&fs);
        let input = HookInput {
            session_id: Some(session_id.into()),
            ..Default::default()
        };
        let out = process_stop(&input, &ctx);
        let ctx_text = out
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref())
            .unwrap_or("");
        assert!(ctx_text.contains("Good Citizen"), "got: {ctx_text}");
        assert!(ctx_text.contains("2 potential issue"), "got: {ctx_text}");
        assert!(ctx_text.contains("dead code"), "got: {ctx_text}");
        assert!(ctx_text.contains("panic"), "got: {ctx_text}");
    }

    #[test]
    fn stop_is_quiet_when_no_observations() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = CapturingFs::new(tmp.path().to_path_buf());
        let ctx = ctx_with_fs(&fs);
        let input = HookInput {
            session_id: Some("s-citizen-stop-2".into()),
            ..Default::default()
        };
        let out = process_stop(&input, &ctx);
        assert!(out.hook_specific_output.is_none());
        assert!(out.blocked.is_none());
    }
}
