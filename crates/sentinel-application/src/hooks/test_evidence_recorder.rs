//! Test Evidence Recorder (`PostToolUse`)
//!
//! When a Bash tool call matches a test/build pattern, append a line to
//! `~/.claude/sentinel/state/test-evidence/{session_id}.jsonl`. The
//! `pre_commit_verification` hook reads from that file instead of trying
//! to parse Claude Code's transcript — which is keyed by an internal
//! session ID we don't have.
//!
//! Best-effort: any IO failure here is silently swallowed so a recorder
//! glitch can never block a tool call.

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::test_evidence::{compile_command_patterns, evidence_path, TestEvidenceEntry};

pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    // Bash only — every other tool is irrelevant for test evidence.
    if input.tool_name.as_deref() != Some("Bash") {
        return HookOutput::allow();
    }

    let Some(session_id) = input.session_id.as_deref() else {
        return HookOutput::allow();
    };

    let command = input
        .tool_input
        .as_ref()
        .and_then(|ti| ti.get("command"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    if command.is_empty() {
        return HookOutput::allow();
    }

    let patterns = compile_command_patterns();
    if !patterns.iter().any(|r| r.is_match(command)) {
        return HookOutput::allow();
    }

    // Pull tool result success bit if Claude Code reported one. The field
    // shape is opaque (`serde_json::Value`); we look for an explicit
    // `success: false` and treat anything else as success. Worst case,
    // a failed test still counts as "tests ran" — same semantics as the
    // legacy transcript-based check.
    let success = input
        .tool_result
        .as_ref()
        .and_then(|tr| tr.get("success"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);

    let Some(home) = ctx.fs.home_dir() else {
        return HookOutput::allow();
    };
    let path = evidence_path(&home, session_id);

    if let Some(parent) = path.parent() {
        let _ = ctx.fs.create_dir_all(parent);
    }

    let entry = TestEvidenceEntry {
        ts_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis()),
        session_id: session_id.to_string(),
        cwd: input.cwd.clone().unwrap_or_default(),
        command: command.to_string(),
        success,
    };

    if let Ok(line) = serde_json::to_string(&entry) {
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        let _ = ctx.fs.append(&path, &bytes);
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support;
    use sentinel_domain::events::HookInput;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Minimal real-FS adapter that pins `home_dir()` to a tempdir and
    /// records every `append` so tests can read what the recorder wrote
    /// without poking at the global filesystem.
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
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(&self, p: &Path, c: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(&self, p: &Path) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::create_dir_all(p)?)
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
            Ok(std::fs::metadata(p)?)
        }
        fn append(&self, p: &Path, c: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?;
            f.write_all(c)?;
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
    fn records_cargo_test_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = CapturingFs::new(tmp.path().to_path_buf());
        let ctx = ctx_with_fs(&fs);
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "cargo test --workspace"})),
            session_id: Some("sess-1".into()),
            cwd: Some("/repo".into()),
            ..Default::default()
        };
        let out = process(&input, &ctx);
        assert!(out.blocked.is_none());
        let writes = fs.appends.lock().unwrap();
        assert_eq!(writes.len(), 1, "expected one evidence append");
        let (path, bytes) = &writes[0];
        assert!(
            path.ends_with("test-evidence/sess-1.jsonl"),
            "got: {}",
            path.display()
        );
        let line = std::str::from_utf8(bytes).unwrap();
        let parsed: TestEvidenceEntry = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed.session_id, "sess-1");
        assert_eq!(parsed.command, "cargo test --workspace");
    }

    #[test]
    fn ignores_non_test_command() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = CapturingFs::new(tmp.path().to_path_buf());
        let ctx = ctx_with_fs(&fs);
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "ls -la"})),
            session_id: Some("sess-2".into()),
            ..Default::default()
        };
        process(&input, &ctx);
        assert!(fs.appends.lock().unwrap().is_empty());
    }

    #[test]
    fn ignores_non_bash_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = CapturingFs::new(tmp.path().to_path_buf());
        let ctx = ctx_with_fs(&fs);
        let input = HookInput {
            tool_name: Some("Read".into()),
            tool_input: Some(serde_json::json!({"file_path": "foo.rs"})),
            session_id: Some("sess-3".into()),
            ..Default::default()
        };
        process(&input, &ctx);
        assert!(fs.appends.lock().unwrap().is_empty());
    }

    #[test]
    fn captures_failure_bit_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = CapturingFs::new(tmp.path().to_path_buf());
        let ctx = ctx_with_fs(&fs);
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "cargo build"})),
            tool_result: Some(serde_json::json!({"success": false})),
            session_id: Some("sess-4".into()),
            ..Default::default()
        };
        process(&input, &ctx);
        let writes = fs.appends.lock().unwrap();
        let line = std::str::from_utf8(&writes[0].1).unwrap();
        let parsed: TestEvidenceEntry = serde_json::from_str(line.trim()).unwrap();
        assert!(!parsed.success);
    }
}
