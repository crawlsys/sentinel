//! Per-call hook telemetry.
//!
//! Each hook invocation appends a single JSON line to
//! `~/.claude/sentinel/metrics/hook-invocations.jsonl` so local clients can
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
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use sentinel_domain::events::{HookOutput, PermissionDecision};

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
    /// Raw (pre-downgrade) verdict class. Equals `outcome` unless a policy
    /// mode (e.g. `SENTINEL_AUTOPILOT=1`) downgraded the gate's verdict
    /// before the output was returned — then this preserves the original
    /// class (e.g. `ask` while `outcome` is `allow`/`inject`), keeping
    /// "would have escalated to a human" firings measurable in headless
    /// runs. Genuine (non-downgraded) `ask` verdicts are also canonicalized
    /// as `ask` here, while `outcome` keeps its historical `deny`
    /// classification for reader compatibility. Historical rows written
    /// before this field existed deserialize as "" — readers should fall
    /// back to `outcome`.
    #[serde(default)]
    pub raw_outcome: String,
    /// First 120 chars of the block/deny reason, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Which agent harness produced this row — `claude` (default), `codex`,
    /// `opencode`, etc. Driven by the `SENTINEL_SOURCE_HARNESS` env the caller
    /// sets, so attribution is INTRINSIC to the row, not inferred from which
    /// ledger file it landed in. Always present so the report can group on
    /// it without positional guessing. Defaults to `claude` when reading back
    /// historical rows written before this field existed.
    #[serde(default = "default_source_harness")]
    pub source_harness: String,
    /// Stable per-machine identity for the reporting client — the seam that
    /// makes "how many unique clients shipped into the lake this window?"
    /// answerable (every machine's rows otherwise merge into one harness
    /// prefix). Resolution order: `SENTINEL_CLIENT_ID` env → the persisted
    /// `~/.claude/sentinel/client-id` → a `machine-id`-derived id. Always
    /// present on new rows; historical rows written before this field default
    /// to `unknown` (the aggregator falls back to a session-count estimate for
    /// those). Intrinsic to the row, like `source_harness`.
    #[serde(default = "default_client_id")]
    pub client_id: String,
}

fn default_source_harness() -> String {
    "claude".to_string()
}

/// Sentinel value for rows with no resolved client identity — historical rows
/// (predating the field) and any environment where neither the env override,
/// the persisted file, nor a machine-id could be obtained. The aggregator
/// treats this as "unattributed" and falls back to a session-based estimate.
pub const UNKNOWN_CLIENT_ID: &str = "unknown";

fn default_client_id() -> String {
    UNKNOWN_CLIENT_ID.to_string()
}

/// Process-cached client id so the hot path resolves (and any first-use
/// file write) exactly once.
static CLIENT_ID: OnceLock<String> = OnceLock::new();

/// Stable per-machine client id, resolved once per process.
fn resolve_client_id() -> String {
    CLIENT_ID.get_or_init(compute_client_id).clone()
}

/// Resolve precedence: explicit `SENTINEL_CLIENT_ID` env → persisted
/// `<claude_dir>/sentinel/client-id` (generated + written 0600 on first use) →
/// [`UNKNOWN_CLIENT_ID`] when the claude dir can't be resolved. The directory
/// comes from [`crate::paths::try_claude_dir`], so `SENTINEL_CLAUDE_DIR` /
/// `SENTINEL_HOME` isolation is honored (no writes to a real home under
/// isolated profiles/tests). Errors are swallowed — client identity must never
/// break a hook.
fn compute_client_id() -> String {
    let env = std::env::var_os("SENTINEL_CLIENT_ID")
        .and_then(|s| s.into_string().ok())
        .and_then(nonempty_trimmed);
    if let Some(v) = env {
        return v;
    }
    let Ok(claude_dir) = crate::paths::try_claude_dir() else {
        return default_client_id();
    };
    let path = claude_dir.join("sentinel").join("client-id");
    if let Some(v) = std::fs::read_to_string(&path)
        .ok()
        .and_then(nonempty_trimmed)
    {
        return v;
    }
    let id = generate_client_id();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = write_client_id_file(&path, &id);
    id
}

/// Trim and drop-if-empty — the shared "a present, non-blank value wins"
/// rule used by every client-id resolution step.
fn nonempty_trimmed(s: String) -> Option<String> {
    let t = s.trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// Deterministic from the host `machine-id` (hashed, so the raw id never
/// leaks) when available — a deleted `client-id` file regenerates the SAME
/// id — else a random uuid. The prefix records which path produced it.
fn generate_client_id() -> String {
    if let Some(mid) = std::fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        let digest = hex::encode(Sha256::digest(mid.as_bytes()));
        return format!("m-{}", &digest[..16]);
    }
    format!("r-{}", uuid::Uuid::new_v4().simple())
}

/// Write the client-id file with owner-only `0600` perms on unix (it is not
/// secret, but it is per-machine identity — no reason for it to be
/// world-readable). On non-unix targets, fall back to a plain write.
#[cfg(unix)]
fn write_client_id_file(path: &std::path::Path, id: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(id.as_bytes())
}

#[cfg(not(unix))]
fn write_client_id_file(path: &std::path::Path, id: &str) -> std::io::Result<()> {
    std::fs::write(path, id)
}

/// Resolve the harness attribution from the process environment. Reads
/// `SENTINEL_SOURCE_HARNESS` via `var_os` so a non-UTF8 (or empty) value falls
/// back to the canonical default instead of silently dropping attribution.
fn resolve_source_harness() -> String {
    source_harness_from(std::env::var_os("SENTINEL_SOURCE_HARNESS"))
}

/// Pure resolver (unit-testable without touching global env): a present,
/// valid-UTF8, non-empty value wins; anything else yields the serde-canonical
/// [`default_source_harness`].
fn source_harness_from(value: Option<std::ffi::OsString>) -> String {
    value
        .and_then(|s| s.into_string().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(default_source_harness)
}

/// Outcome classes recorded in the `outcome` column. Keep these stable —
/// local clients group by string value.
pub enum Outcome {
    /// Hook returned without injecting context or blocking. The vast
    /// majority of calls.
    Allow,
    /// `HookOutput::block` — internal block flag set.
    Block,
    /// `HookOutput::deny` — `permissionDecision: deny` set on `PreToolUse`.
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

/// Compute the raw (pre-downgrade) verdict class for the ledger row.
///
/// Priority:
/// 1. A hook-declared raw verdict (`HookOutput::with_raw_permission_decision`)
///    — set by autopilot-aware gates at their downgrade site — wins.
/// 2. A native (non-downgraded) `ask` is surfaced as `ask`: `classify`
///    buckets it as `Deny` (any `permission_decision_reason` ⇒ deny), and
///    that string is load-bearing for existing readers, so the truthful
///    class lives here instead.
/// 3. Otherwise the raw verdict equals the effective outcome.
fn raw_outcome_of(output: &HookOutput, effective: &Outcome) -> String {
    if let Some(raw) = output.raw_permission_decision {
        return raw.as_str().to_string();
    }
    if output
        .hook_specific_output
        .as_ref()
        .and_then(|h| h.permission_decision)
        == Some(PermissionDecision::Ask)
    {
        return PermissionDecision::Ask.as_str().to_string();
    }
    effective.as_str().to_string()
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
pub fn time_and_record<F>(fs: &dyn FileSystemPort, ctx: &InvocationContext<'_>, f: F) -> HookOutput
where
    F: FnOnce() -> HookOutput,
{
    let started = Instant::now();
    let output = f();
    let elapsed: Duration = started.elapsed();
    let (outcome, reason) = classify(&output);
    let raw_outcome = raw_outcome_of(&output, &outcome);

    let row = HookInvocation {
        ts: chrono::Utc::now().to_rfc3339(),
        event: ctx.event.to_string(),
        hook: ctx.hook.to_string(),
        tool: ctx.tool.map(str::to_string),
        session_id: ctx.session_id.map(str::to_string),
        repo_root: ctx.repo_root.map(str::to_string),
        duration_us: elapsed.as_micros(),
        outcome: outcome.as_str().to_string(),
        raw_outcome,
        reason,
        // Intrinsic per-harness attribution. The caller (e.g. the OpenCode
        // sentinel plugin) exports SENTINEL_SOURCE_HARNESS; default `claude`
        // covers the global Claude Code hooks that don't set it. Shares the
        // serde-canonical default + tolerates non-UTF8 env values.
        source_harness: resolve_source_harness(),
        // Stable per-machine identity (env → persisted file → machine-id),
        // resolved once per process. Lets the lake report count distinct
        // reporting clients instead of conflating every machine.
        client_id: resolve_client_id(),
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

    // --- source_harness attribution (intrinsic per-harness tagging) ---------

    #[test]
    fn source_harness_defaults_to_claude_when_unset() {
        assert_eq!(source_harness_from(None), "claude");
    }

    #[test]
    fn source_harness_uses_env_value_when_set() {
        assert_eq!(source_harness_from(Some("opencode".into())), "opencode");
        assert_eq!(source_harness_from(Some("codex".into())), "codex");
    }

    #[test]
    fn source_harness_falls_back_when_empty() {
        assert_eq!(
            source_harness_from(Some(std::ffi::OsString::new())),
            "claude"
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_harness_falls_back_on_non_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let bad = std::ffi::OsString::from_vec(vec![0xff, 0xfe]);
        assert_eq!(source_harness_from(Some(bad)), "claude");
    }

    #[test]
    fn historical_row_without_source_harness_deserializes_as_claude() {
        // Rows written before the field existed must read back as `claude`.
        let json = r#"{"ts":"2026-01-01T00:00:00Z","event":"PreToolUse","hook":"phase_gate","duration_us":7,"outcome":"allow"}"#;
        let row: HookInvocation = serde_json::from_str(json).unwrap();
        assert_eq!(row.source_harness, "claude");
    }

    #[test]
    fn source_harness_round_trips_through_serde() {
        let json = r#"{"ts":"2026-01-01T00:00:00Z","event":"PreToolUse","hook":"phase_gate","duration_us":7,"outcome":"allow","source_harness":"opencode"}"#;
        let row: HookInvocation = serde_json::from_str(json).unwrap();
        assert_eq!(row.source_harness, "opencode");
    }

    // --- client_id (per-machine reporting identity) -------------------------

    #[test]
    fn historical_row_without_client_id_defaults_to_unknown() {
        let json = r#"{"ts":"2026-01-01T00:00:00Z","event":"PreToolUse","hook":"phase_gate","duration_us":7,"outcome":"allow"}"#;
        let row: HookInvocation = serde_json::from_str(json).unwrap();
        assert_eq!(row.client_id, UNKNOWN_CLIENT_ID);
    }

    #[test]
    fn client_id_round_trips_through_serde() {
        let json = r#"{"ts":"2026-01-01T00:00:00Z","event":"PreToolUse","hook":"phase_gate","duration_us":7,"outcome":"allow","client_id":"m-deadbeef12345678"}"#;
        let row: HookInvocation = serde_json::from_str(json).unwrap();
        assert_eq!(row.client_id, "m-deadbeef12345678");
    }

    #[test]
    fn nonempty_trimmed_drops_blank_keeps_value() {
        assert_eq!(nonempty_trimmed("  ".to_string()), None);
        assert_eq!(nonempty_trimmed(String::new()), None);
        assert_eq!(
            nonempty_trimmed("  abc \n".to_string()),
            Some("abc".to_string())
        );
    }

    #[test]
    fn generate_client_id_is_prefixed_and_stable_shape() {
        // Either a machine-id-derived `m-<16hex>` or a random `r-<uuid>`;
        // both are non-empty and never the unknown sentinel.
        let id = generate_client_id();
        assert!(id.starts_with("m-") || id.starts_with("r-"), "got {id}");
        assert_ne!(id, UNKNOWN_CLIENT_ID);
        if let Some(hexpart) = id.strip_prefix("m-") {
            assert_eq!(hexpart.len(), 16);
            assert!(hexpart.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }

    // --- raw_outcome (pre-downgrade verdict preservation) --------------------

    #[test]
    fn raw_outcome_preserves_downgraded_ask_on_effective_allow() {
        // The autopilot downgrade shape: gate would have asked, effective
        // output is a plain allow. outcome=allow, raw_outcome=ask.
        let output = HookOutput::allow().with_raw_permission_decision(PermissionDecision::Ask);
        let (outcome, _) = classify(&output);
        assert!(matches!(outcome, Outcome::Allow));
        assert_eq!(raw_outcome_of(&output, &outcome), "ask");
    }

    #[test]
    fn raw_outcome_preserves_downgraded_ask_on_effective_inject() {
        // pr_merge_gate's autopilot shape: ask downgraded to a context-only
        // reminder. outcome=inject, raw_outcome=ask.
        let output = HookOutput::inject_context(HookEvent::PreToolUse, "AUTOPILOT: allowing merge")
            .with_raw_permission_decision(PermissionDecision::Ask);
        let (outcome, _) = classify(&output);
        assert!(matches!(outcome, Outcome::Inject));
        assert_eq!(raw_outcome_of(&output, &outcome), "ask");
    }

    #[test]
    fn raw_outcome_equals_outcome_when_no_downgrade() {
        for output in [HookOutput::allow(), HookOutput::deny("nope")] {
            let (outcome, _) = classify(&output);
            assert_eq!(raw_outcome_of(&output, &outcome), outcome.as_str());
        }
    }

    #[test]
    fn raw_outcome_canonicalizes_native_ask_while_outcome_stays_deny() {
        // classify() buckets a genuine ask as Deny (reader-compat string);
        // the truthful class must surface in raw_outcome.
        let output = HookOutput::ask("confirm?");
        let (outcome, _) = classify(&output);
        assert!(matches!(outcome, Outcome::Deny));
        assert_eq!(raw_outcome_of(&output, &outcome), "ask");
    }

    #[test]
    fn historical_row_without_raw_outcome_deserializes_as_empty() {
        let json = r#"{"ts":"2026-01-01T00:00:00Z","event":"PreToolUse","hook":"phase_gate","duration_us":7,"outcome":"allow"}"#;
        let row: HookInvocation = serde_json::from_str(json).unwrap();
        assert_eq!(row.raw_outcome, "");
    }

    #[test]
    fn raw_outcome_round_trips_through_serde() {
        let json = r#"{"ts":"2026-01-01T00:00:00Z","event":"PreToolUse","hook":"doppler_auth0_gate","duration_us":7,"outcome":"allow","raw_outcome":"ask"}"#;
        let row: HookInvocation = serde_json::from_str(json).unwrap();
        assert_eq!(row.raw_outcome, "ask");
        let back = serde_json::to_string(&row).unwrap();
        assert!(back.contains("\"raw_outcome\":\"ask\""));
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
        // Report groups by these literal strings — changing them is
        // a breaking change. Pin them here so a refactor can't silently
        // rename "allow" → "Allow" etc.
        assert_eq!(Outcome::Allow.as_str(), "allow");
        assert_eq!(Outcome::Block.as_str(), "block");
        assert_eq!(Outcome::Deny.as_str(), "deny");
        assert_eq!(Outcome::Inject.as_str(), "inject");
    }
}
