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
//! and fires `memory turn-capture` as a **detached** background process that
//! runs the Opus extraction + dual-judge capture on its own time, outliving
//! the hook. Fire-and-forget; never blocks the turn.
//!
//! Flow: build turn → gate trivial turns → `spawn_detached("memory",
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

/// Locate the `memory` CLI binary. Prefers `~/.cargo/bin`, falls back to the
/// dev release build. Returns the command string for `spawn_detached`.
fn memory_bin() -> Option<String> {
    let home = dirs::home_dir()?;
    let cargo_bin = home.join(".cargo").join("bin").join("memory.exe");
    if cargo_bin.exists() {
        return Some(cargo_bin.to_string_lossy().to_string());
    }
    let cargo_bin_unix = home.join(".cargo").join("bin").join("memory");
    if cargo_bin_unix.exists() {
        return Some(cargo_bin_unix.to_string_lossy().to_string());
    }
    // Dev fallback: the CLI lives in `memory-cli-rust` (binary `memory`).
    // Check the common clone locations + both platform binary names.
    for repo in ["memory-cli-rust", "memory"] {
        for base in ["Downloads", "Documents/GitHub", "repos"] {
            let mut dir = home.clone();
            for seg in base.split('/') {
                dir = dir.join(seg);
            }
            let root = dir.join(repo).join("target").join("release");
            for name in ["memory", "memory.exe"] {
                let cand = root.join(name);
                if cand.exists() {
                    return Some(cand.to_string_lossy().to_string());
                }
            }
        }
    }
    None
}

/// Returns true at most once per session: writes a session-scoped marker so
/// the "memory CLI missing" notice surfaces a single time instead of on every
/// Stop. Best-effort — if state can't be written, default to warning (a
/// repeated visible warning is far better than a silent capture outage).
fn first_warn_this_session(ctx: &super::HookContext<'_>) -> bool {
    let Some(home) = ctx.fs.home_dir() else {
        return true;
    };
    let dir = home.join(".claude").join("sentinel").join("state");
    let sid = ctx.session_id().unwrap_or_else(|| "unknown".to_string());
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
        // The `memory` CLI is a HARD dependency of auto-capture. If it's missing
        // every turn silently captures nothing — an undetectable memory outage
        // (this exact gap hid a multi-session capture loss). Surface it LOUDLY,
        // but only once per session so we don't spam every Stop.
        warn!("memory_turn_capture: `memory` CLI binary not found — auto-capture is DISABLED");
        if first_warn_this_session(ctx) {
            let msg = "🧠 [memory] auto-capture DISABLED: the `memory` CLI binary \
                is not installed. Memories are NOT being saved this session. Fix: \
                `cargo install --path ~/Downloads/memory-cli-rust/crates/memory-cli --bin memory`.";
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
    use sentinel_domain::events::HookInput;

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
}
