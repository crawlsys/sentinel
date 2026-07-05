//! Memory Turn-Capture Hook — auto-capture atoms from the conversation.
//!
//! Fires on Stop. Replaces the legacy flat-`.md` ingest: instead of the agent
//! hand-writing memory files, this hook captures durable facts straight from
//! the conversation turn.
//!
//! **Why it spawns a detached process instead of calling the LLM inline:**
//! extraction uses Opus 4.7 (a reasoning model — 10-90s), but every sentinel
//! hook runs under a hard 3s wall-clock budget (`run_async`). A slow LLM call
//! can't complete in that window — it would always be cancelled and capture
//! nothing. So the hook stays fast: it builds the turn text, gates on length,
//! and fires `memory-rs turn-capture` as a **detached** background process that
//! runs the Opus extraction + dual-judge capture on its own time, outliving
//! the hook. Fire-and-forget; never blocks the turn.
//!
//! Flow: build turn → gate trivial turns → `spawn_detached("memory-rs",
//! ["turn-capture", "--project", P, "--prompt", U, "--response", A])`.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use tracing::{debug, warn};

/// Minimum combined turn length (chars) worth extracting from. Below this, a
/// turn is almost certainly an ack / tool-noise with no durable fact — skip it
/// so we don't spawn an Opus call for nothing.
const MIN_TURN_CHARS: usize = 200;

/// Cap turn text passed on the command line (the CLI re-caps for the LLM).
const MAX_TURN_CHARS: usize = 12_000;

/// Build the turn text components (user, assistant) if there's enough
/// substance to bother extracting.
fn build_turn(input: &HookInput) -> Option<(String, String)> {
    let prompt = input.prompt.as_deref().unwrap_or("").trim().to_string();
    let assistant = input
        .last_assistant_message
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();

    if prompt.is_empty() && assistant.is_empty() {
        return None;
    }
    if prompt.len() + assistant.len() < MIN_TURN_CHARS {
        return None;
    }
    let cap = |mut s: String| {
        if s.len() > MAX_TURN_CHARS {
            s.truncate(MAX_TURN_CHARS);
        }
        s
    };
    Some((cap(prompt), cap(assistant)))
}

/// Derive a project label from cwd basename (defaults to "global").
fn project_label(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "global".to_string())
}

/// Locate the canonical memory CLI binary. Returns the command string for
/// `spawn_detached`. The binary was renamed `memory-rs` → `memory` (the
/// `memory-cli` crate's `[[bin]] name = "memory"`); the `turn-capture`
/// subcommand lives on it. Probe `memory` first, keep `memory-rs` as a
/// legacy fallback so an older install still works.
fn memory_bin() -> Option<String> {
    let home = dirs::home_dir()?;
    let cargo_bin_dir = home.join(".cargo").join("bin");
    for name in ["memory", "memory.exe", "memory-rs", "memory-rs.exe"] {
        let cand = cargo_bin_dir.join(name);
        if cand.exists() {
            return Some(cand.to_string_lossy().to_string());
        }
    }
    None
}

fn concrete_context_session_id(ctx: &super::HookContext<'_>) -> Option<String> {
    let session_id = ctx.session_id()?;
    let session_id = session_id.trim();
    super::session_path_component(session_id).map(str::to_string)
}

/// Returns true at most once per session: writes a session-scoped marker so
/// the "memory CLI missing" notice surfaces a single time instead of on every
/// Stop. Best-effort — if state can't be written, default to warning (a
/// repeated visible warning is far better than a silent capture outage).
fn first_warn_this_session(ctx: &super::HookContext<'_>) -> bool {
    let Some(sid) = concrete_context_session_id(ctx) else {
        return true;
    };
    let Some(home) = ctx.fs.home_dir() else {
        return true;
    };
    let dir = home.join(".claude").join("sentinel").join("state");
    let path = dir.join(format!("memory-bin-missing-warned-{sid}"));
    if ctx.fs.read_to_string(&path).is_ok() {
        return false; // already warned this session
    }
    let _ = ctx.fs.create_dir_all(&dir);
    let _ = ctx.fs.write(&path, b"1");
    true
}

/// Stop-hook entry point. Fast: spawns the detached extractor and returns.
/// Always `allow()` — never blocks the turn.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let Some((prompt, response)) = build_turn(input) else {
        return HookOutput::allow();
    };

    let Some(bin) = memory_bin() else {
        // The `memory-rs` CLI is a HARD dependency of auto-capture. If it's missing
        // every turn silently captures nothing — an undetectable memory outage
        // (this exact gap hid a multi-session capture loss). Surface it LOUDLY,
        // but only once per session so we don't spam every Stop.
        warn!("memory_turn_capture: `memory` CLI binary not found — auto-capture is DISABLED");
        if first_warn_this_session(ctx) {
            let msg = "🧠 [memory] auto-capture DISABLED: the `memory` CLI binary \
                is not installed. Memories are NOT being saved this session. Fix: \
                `cargo install --path ~/Documents/GitHub/memory-cli-rust/crates/memory-cli --bin memory`.";
            let mut out = HookOutput::inject_context(HookEvent::Stop, msg);
            out.system_message = Some(msg.to_string());
            return out;
        }
        return HookOutput::allow();
    };

    let cwd = input.cwd.as_deref().unwrap_or(".");
    let project = project_label(cwd);

    // Fire-and-forget: the detached process runs Opus extraction + dual-judge
    // capture on its own time (no 3s hook budget). Errors there are logged by
    // the CLI, not here.
    let args = [
        "turn-capture",
        "--project",
        &project,
        "--prompt",
        &prompt,
        "--response",
        &response,
    ];
    match ctx.process.spawn_detached(&bin, &args) {
        Ok(()) => debug!(project = %project, "memory_turn_capture: spawned detached extractor"),
        Err(e) => debug!(error = %e, "memory_turn_capture: spawn failed"),
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, StubEnv, TestHomeFs};
    use crate::hooks::HookContext;
    use sentinel_domain::events::HookInput;

    fn ctx_with_fs_env<'a>(
        fs: &'a dyn crate::hooks::FileSystemPort,
        env: &'a dyn crate::hooks::EnvPort,
    ) -> HookContext<'a> {
        let base = stub_ctx_with_fs(fs);
        HookContext {
            git: base.git,
            vector_store: base.vector_store,
            fs,
            process: base.process,
            llm: base.llm,
            memory_mcp: base.memory_mcp,
            env,
            linear_lookup: base.linear_lookup,
        }
    }

    #[test]
    fn skips_trivial_turn() {
        let input = HookInput {
            prompt: Some("ok".into()),
            last_assistant_message: Some("done".into()),
            ..Default::default()
        };
        assert!(build_turn(&input).is_none());
    }

    #[test]
    fn builds_substantial_turn() {
        let input = HookInput {
            prompt: Some("x".repeat(150)),
            last_assistant_message: Some("y".repeat(100)),
            ..Default::default()
        };
        let (u, a) = build_turn(&input).expect("should build");
        assert_eq!(u.len(), 150);
        assert_eq!(a.len(), 100);
    }

    #[test]
    fn empty_turn_is_none() {
        let input = HookInput {
            prompt: Some("".into()),
            last_assistant_message: Some("".into()),
            ..Default::default()
        };
        assert!(build_turn(&input).is_none());
    }

    #[test]
    fn project_label_from_cwd() {
        assert_eq!(project_label("/c/Users/x/GitHub/memory"), "memory");
        assert_eq!(project_label(""), "global");
    }

    #[test]
    fn missing_session_warns_without_writing_unknown_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let env = StubEnv::new();
        let ctx = ctx_with_fs_env(&fs, &env);
        let marker = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join("memory-bin-missing-warned-unknown");

        assert!(first_warn_this_session(&ctx));
        assert!(!marker.exists());
    }

    #[test]
    fn synthetic_unknown_session_warns_without_writing_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let env = StubEnv::with(&[("CLAUDE_SESSION_ID", " unknown ")]);
        let ctx = ctx_with_fs_env(&fs, &env);
        let raw_marker = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join("memory-bin-missing-warned- unknown ");
        let trimmed_marker = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join("memory-bin-missing-warned-unknown");

        assert!(first_warn_this_session(&ctx));
        assert!(!raw_marker.exists());
        assert!(!trimmed_marker.exists());
    }

    #[test]
    fn concrete_session_writes_marker_once() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let env = StubEnv::with(&[("CLAUDE_SESSION_ID", "memory-session-123")]);
        let ctx = ctx_with_fs_env(&fs, &env);
        let marker = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join("memory-bin-missing-warned-memory-session-123");

        assert!(first_warn_this_session(&ctx));
        assert!(marker.exists());
        assert!(!first_warn_this_session(&ctx));
    }
}
