//! Per-call hook telemetry.
//!
//! Each hook invocation appends a single JSON line to
//! `~/.claude/sentinel/metrics/hook-invocations.jsonl` so the dashboard can
//! show:
//!   - which hooks fired and when
//!   - which hooks blocked tool calls and why
//!   - per-hook latency distribution
//!
//! Privacy: we record the hook name, the event name, the tool name (e.g.
//! `Bash`, `Edit`), session id, repo root, duration, and a 120-char reason
//! snippet for blocks. We **never** log `tool_input` (which can contain
//! secrets like API keys) or `tool_result`.
//!
//! Append-only; rotation isn't implemented yet — at ~200 invocations / day
//! the file grows ~2.5 MB / year, which is fine for now.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use sentinel_domain::events::HookOutput;

use crate::hooks::{metrics_dir, FileSystemPort};

/// One row in `hook-invocations.jsonl`. Public so the daemon API can
/// deserialize the same shape when it reads the file back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HookInvocation {
    /// RFC3339 timestamp.
    pub ts: String,
    /// Hook lifecycle event (e.g. `PreToolUse`, `Stop`).
    pub event: String,
    /// Hook module name (e.g. `bug_task_gate`, `git_hygiene`).
    pub hook: String,
    /// Tool name when the event is tool-scoped, else None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Session id (already in transcript metadata; safe to log).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Resolved repo root (absolute path); None when cwd isn't a repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    /// Wall-clock hook duration in microseconds.
    pub duration_us: u128,
    /// Outcome class — see `Outcome::as_str` for the canonical values.
    pub outcome: String,
    /// First 120 chars of the block/deny reason, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Outcome classes recorded in the `outcome` column. Keep these stable —
/// the dashboard groups by string value.
pub enum Outcome {
    /// Hook returned without injecting context or blocking. The vast
    /// majority of calls.
    Allow,
    /// `HookOutput::block` — internal block flag set.
    Block,
    /// `HookOutput::deny` — `permissionDecision: deny` set on PreToolUse.
    Deny,
    /// `inject_context` / `inject_envelope` fired but no block.
    Inject,
}

impl Outcome {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Block => "block",
            Self::Deny => "deny",
            Self::Inject => "inject",
        }
    }
}

/// Classify a `HookOutput` into one of the four `Outcome` buckets and
/// extract a truncated block/deny reason if applicable.
fn classify(output: &HookOutput) -> (Outcome, Option<String>) {
    // Deny (PermissionDecision::Deny) takes priority over block —
    // both set `blocked: Some(true)` internally, but a deny carries
    // a permission_decision_reason that's user-visible at the platform
    // level.
    if let Some(hso) = &output.hook_specific_output {
        if let Some(reason) = &hso.permission_decision_reason {
            // Heuristic: any permission_decision_reason at all means deny;
            // the hook would not have set it for an Allow path.
            return (Outcome::Deny, Some(truncate(reason, 120)));
        }
    }
    if output.blocked == Some(true) {
        let reason = output.reason.as_deref().map(|r| truncate(r, 120));
        return (Outcome::Block, reason);
    }
    // Inject path: any additional_context produced by the hook.
    if output
        .hook_specific_output
        .as_ref()
        .and_then(|h| h.additional_context.as_ref())
        .is_some()
    {
        return (Outcome::Inject, None);
    }
    (Outcome::Allow, None)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n - 1).collect();
        out.push('…');
        out
    }
}

/// Resolve the on-disk path for the JSONL log. Returns None when the
/// home directory can't be located (we don't fall back to cwd because
/// that would scatter telemetry across project dirs).
fn invocation_log_path(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = metrics_dir(&home);
    let _ = fs.create_dir_all(&dir);
    Some(dir.join("hook-invocations.jsonl"))
}

/// Append a row to `hook-invocations.jsonl`. Errors are intentionally
/// swallowed — telemetry must never break a hook.
pub fn record(fs: &dyn FileSystemPort, row: &HookInvocation) {
    let Some(path) = invocation_log_path(fs) else {
        return;
    };
    let Ok(json) = serde_json::to_string(row) else {
        return;
    };
    let mut line = json;
    line.push('\n');
    let _ = fs.append(&path, line.as_bytes());
}

/// Inputs the dispatcher already has on hand — bundled into one struct so
/// the timing helper signature stays small.
pub struct InvocationContext<'a> {
    pub event: &'a str,
    pub hook: &'a str,
    pub tool: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub repo_root: Option<&'a str>,
}

/// Run a hook, time it, classify the outcome, append a JSONL row, and
/// return the `HookOutput` so the dispatcher can `output.merge(&result)`
/// as before. The closure shape is intentionally non-async — every
/// existing dispatcher call site is sync.
pub fn time_and_record<F>(
    fs: &dyn FileSystemPort,
    ctx: &InvocationContext<'_>,
    f: F,
) -> HookOutput
where
    F: FnOnce() -> HookOutput,
{
    let started = Instant::now();
    let output = f();
    let elapsed: Duration = started.elapsed();
    let (outcome, reason) = classify(&output);

    let row = HookInvocation {
        ts: chrono::Utc::now().to_rfc3339(),
        event: ctx.event.to_string(),
        hook: ctx.hook.to_string(),
        tool: ctx.tool.map(str::to_string),
        session_id: ctx.session_id.map(str::to_string),
        repo_root: ctx.repo_root.map(str::to_string),
        duration_us: elapsed.as_micros(),
        outcome: outcome.as_str().to_string(),
        reason,
    };
    record(fs, &row);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::events::HookEvent;

    #[test]
    fn test_classify_allow() {
        let output = HookOutput::allow();
        let (outcome, reason) = classify(&output);
        assert!(matches!(outcome, Outcome::Allow));
        assert!(reason.is_none());
    }

    #[test]
    fn test_classify_block_extracts_reason() {
        let output = HookOutput::block("test reason");
        let (outcome, reason) = classify(&output);
        assert!(matches!(outcome, Outcome::Block));
        assert_eq!(reason.as_deref(), Some("test reason"));
    }

    #[test]
    fn test_classify_deny_takes_priority_over_block() {
        // `HookOutput::deny` sets both blocked: Some(true) AND
        // permission_decision_reason — must be classified as Deny, not Block.
        let output = HookOutput::deny("denied for reasons");
        let (outcome, _reason) = classify(&output);
        assert!(matches!(outcome, Outcome::Deny));
    }

    #[test]
    fn test_classify_inject_when_context_present() {
        let output = HookOutput::inject_context(HookEvent::UserPromptSubmit, "hi");
        let (outcome, _) = classify(&output);
        assert!(matches!(outcome, Outcome::Inject));
    }

    #[test]
    fn test_truncate_short_string_unchanged() {
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn test_truncate_long_string_uses_ellipsis() {
        let long: String = "a".repeat(200);
        let t = truncate(&long, 50);
        assert_eq!(t.chars().count(), 50);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn test_outcome_strings_are_stable() {
        // Dashboard groups by these literal strings — changing them is
        // a breaking change. Pin them here so a refactor can't silently
        // rename "allow" → "Allow" etc.
        assert_eq!(Outcome::Allow.as_str(), "allow");
        assert_eq!(Outcome::Block.as_str(), "block");
        assert_eq!(Outcome::Deny.as_str(), "deny");
        assert_eq!(Outcome::Inject.as_str(), "inject");
    }
}
