//! Context Monitor — Two-phase hook
//!
//! **Stop phase:** Reads context window usage from the Stop event payload,
//! writes current zone to `~/.claude/metrics/context-zone.json`.
//!
//! **`UserPromptSubmit` phase:** Reads zone state, injects zone-specific
//! strategy guidance when usage is above 50%.

use sentinel_domain::constants;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    concrete_input_session_id as concrete_session_id, session_path_component, FileSystemPort,
    HookContext,
};

/// Cooldown between context warnings.
const COOLDOWN_MS: u64 = constants::HOOK_COOLDOWN_SHORT_MS;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Zone {
    Green,  // 0-50%
    Yellow, // 50-65%
    Orange, // 65-75%
    Red,    // 75%+
}

impl Zone {
    fn from_pct(pct: f64) -> Self {
        if pct >= 75.0 {
            Self::Red
        } else if pct >= 65.0 {
            Self::Orange
        } else if pct >= 50.0 {
            Self::Yellow
        } else {
            Self::Green
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Green => "Green",
            Self::Yellow => "Yellow",
            Self::Orange => "Orange",
            Self::Red => "Red",
        }
    }

    const fn strategy(self) -> &'static str {
        match self {
            Self::Green => "",
            Self::Yellow => "Start delegating exploration to agents. Avoid reading large files directly — use agents with targeted queries instead.",
            Self::Orange => "Use agents for ALL exploration and file reads. Keep responses concise. Summarize rather than quote. Prepare for auto-compact.",
            Self::Red => "CRITICAL: Agents only for everything. Do not read files directly. Keep all responses under 3 sentences. Auto-compact is imminent — finish current task and commit.",
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ContextState {
    percent_used: f64,
    zone: String,
    session_id: String,
    ts: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

// Session-id validation centralized in `super::session_path_component` /
// `super::concrete_input_session_id` (imported at top, latter aliased). The
// canonical validator adds path-traversal (`..`) rejection the inline copy
// lacked.

fn state_file(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    let session_id = session_path_component(session_id)?;
    let home = fs.home_dir()?;
    let dir = super::metrics_dir(&home);
    fs.create_dir_all(&dir).ok()?;
    Some(dir.join(format!("context-zone-{session_id}.json")))
}

fn cooldown_file(session_id: &str) -> Option<PathBuf> {
    let session_id = session_path_component(session_id)?;
    Some(std::env::temp_dir().join(format!("claude-context-monitor-{session_id}-last")))
}

fn cooldown_expired(fs: &dyn FileSystemPort, session_id: &str) -> bool {
    let Some(path) = cooldown_file(session_id) else {
        return true;
    };
    let content = match fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let last: u64 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return true,
    };
    now_ms().saturating_sub(last) >= COOLDOWN_MS
}

fn write_cooldown(fs: &dyn FileSystemPort, session_id: &str) {
    let Some(path) = cooldown_file(session_id) else {
        return;
    };
    let _ = fs.write(&path, now_ms().to_string().as_bytes());
}

/// Extract usage percentage from `context_window` payload.
fn extract_usage_pct(context: &serde_json::Value) -> Option<f64> {
    context
        .get("percentUsed")
        .and_then(serde_json::Value::as_f64)
        .or_else(|| {
            context.get("used").and_then(|used| {
                context.get("total").and_then(|total| {
                    let u = used.as_f64()?;
                    let t = total.as_f64()?;
                    if t > 0.0 {
                        Some(u / t * 100.0)
                    } else {
                        None
                    }
                })
            })
        })
}

// ---------------------------------------------------------------------------
// Stop phase: capture context usage and write zone state
// ---------------------------------------------------------------------------

pub fn process_stop(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cw = match &input.context_window {
        Some(c) => c,
        None => return HookOutput::allow(),
    };

    let pct = match extract_usage_pct(cw) {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    let Some(session_id) = concrete_session_id(input) else {
        return HookOutput::allow();
    };
    let zone = Zone::from_pct(pct);

    let state = ContextState {
        percent_used: pct,
        zone: zone.label().to_string(),
        session_id: session_id.to_string(),
        ts: chrono::Utc::now().to_rfc3339(),
    };

    if let Some(path) = state_file(ctx.fs, session_id) {
        let _ = ctx.fs.write(
            &path,
            serde_json::to_string(&state).unwrap_or_default().as_bytes(),
        );
    }

    if pct > 65.0 {
        tracing::warn!(
            usage = pct,
            zone = zone.label(),
            "Context window usage elevated"
        );

        // Push real-time channel notification for orange/red zone
        let summary = format!(
            "Context usage at {pct:.0}% ({z} zone). {strategy}",
            z = zone.label(),
            strategy = if zone == Zone::Red {
                "Auto-compact imminent!"
            } else {
                "Consider delegating to agents."
            },
        );
        let mut meta = serde_json::Map::new();
        meta.insert(
            "percent".to_string(),
            serde_json::Value::Number(
                serde_json::Number::from_f64(pct).unwrap_or_else(|| serde_json::Number::from(0)),
            ),
        );
        meta.insert(
            "zone".to_string(),
            serde_json::Value::String(zone.label().to_string()),
        );
        crate::channel_events::emit(
            ctx.fs,
            ctx.env,
            "context_threshold",
            &summary,
            meta,
            input.session_id.as_deref(),
            input.cwd.as_deref(),
            Some("context_monitor"),
        );
    }

    HookOutput::allow()
}

// ---------------------------------------------------------------------------
// UserPromptSubmit phase: inject zone-specific strategy
// ---------------------------------------------------------------------------

pub fn process_prompt(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let Some(session_id) = concrete_session_id(input) else {
        return HookOutput::allow();
    };
    let path = match state_file(ctx.fs, session_id) {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    let content = match ctx.fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HookOutput::allow(),
    };

    let state: ContextState = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => return HookOutput::allow(),
    };

    // Defense-in-depth: file path already scopes to session, but double-check
    if state.session_id != session_id {
        return HookOutput::allow();
    }

    let zone = Zone::from_pct(state.percent_used);

    // Green zone — no guidance needed
    if zone == Zone::Green {
        return HookOutput::allow();
    }

    if !cooldown_expired(ctx.fs, session_id) {
        return HookOutput::allow();
    }

    write_cooldown(ctx.fs, session_id);

    let context = format!(
        "[Context Monitor] {zone} zone — {pct:.0}% context used.\n{strategy}",
        zone = zone.label(),
        pct = state.percent_used,
        strategy = zone.strategy(),
    );

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, TestHomeFs};
    use serde_json::json;

    #[test]
    fn test_zone_classification() {
        assert_eq!(Zone::from_pct(30.0), Zone::Green);
        assert_eq!(Zone::from_pct(50.0), Zone::Yellow);
        assert_eq!(Zone::from_pct(55.0), Zone::Yellow);
        assert_eq!(Zone::from_pct(65.0), Zone::Orange);
        assert_eq!(Zone::from_pct(70.0), Zone::Orange);
        assert_eq!(Zone::from_pct(75.0), Zone::Red);
        assert_eq!(Zone::from_pct(90.0), Zone::Red);
    }

    #[test]
    fn test_extract_usage_pct_percent_used() {
        let v = json!({ "percentUsed": 42.5 });
        assert_eq!(extract_usage_pct(&v), Some(42.5));
    }

    #[test]
    fn test_extract_usage_pct_used_total() {
        let v = json!({ "used": 75000, "total": 100000 });
        assert_eq!(extract_usage_pct(&v), Some(75.0));
    }

    #[test]
    fn test_extract_usage_pct_empty() {
        let v = json!({});
        assert_eq!(extract_usage_pct(&v), None);
    }

    #[test]
    fn test_stop_no_context_window() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_stop_writes_state() {
        let input = HookInput {
            context_window: Some(json!({ "percentUsed": 60.0 })),
            session_id: Some("test-ctx".into()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn stop_missing_session_does_not_write_unknown_context_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let unknown_state = crate::hooks::metrics_dir(tmp.path()).join("context-zone-unknown.json");
        let _ = std::fs::remove_file(&unknown_state);

        let input = HookInput {
            context_window: Some(json!({ "percentUsed": 70.0 })),
            session_id: None,
            ..Default::default()
        };
        let output = process_stop(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(
            !unknown_state.exists(),
            "missing session must not write context-zone-unknown state"
        );
    }

    #[test]
    fn stop_concrete_session_writes_scoped_context_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);

        let input = HookInput {
            context_window: Some(json!({ "percentUsed": 70.0 })),
            session_id: Some("context-real".into()),
            ..Default::default()
        };
        let output = process_stop(&input, &ctx);
        let path = state_file(&fs, "context-real").expect("context state path");
        let state: ContextState =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        assert!(output.blocked.is_none());
        assert_eq!(state.session_id, "context-real");
        assert_eq!(state.zone, "Orange");
    }

    #[test]
    fn test_prompt_green_zone_no_inject() {
        let input = HookInput {
            session_id: Some("test-green".into()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_prompt(&input, &ctx);
        // StubFs returns error on read → allow
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn prompt_without_concrete_session_ignores_legacy_unknown_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let metrics = crate::hooks::metrics_dir(tmp.path());
        std::fs::create_dir_all(&metrics).unwrap();
        let unknown_state = metrics.join("context-zone-unknown.json");
        std::fs::write(
            &unknown_state,
            serde_json::to_string(&ContextState {
                percent_used: 80.0,
                zone: "Red".to_string(),
                session_id: "unknown".to_string(),
                ts: chrono::Utc::now().to_rfc3339(),
            })
            .unwrap(),
        )
        .unwrap();

        let output = process_prompt(&HookInput::default(), &ctx);

        assert!(output.hook_specific_output.is_none());
        assert!(
            unknown_state.exists(),
            "missing prompt session must not consume legacy unknown context state"
        );
    }

    #[test]
    fn test_zone_strategies_not_empty() {
        assert!(Zone::Yellow.strategy().contains("delegating"));
        assert!(Zone::Orange.strategy().contains("agents"));
        assert!(Zone::Red.strategy().contains("CRITICAL"));
        assert!(Zone::Green.strategy().is_empty());
    }

    #[test]
    fn test_cooldown_logic() {
        let ctx = crate::hooks::test_support::stub_ctx();
        // StubFs returns error on read → expired
        assert!(cooldown_expired(ctx.fs, "test-context-cooldown"));
    }

    /// Regression test: cooldown stamp files must be scoped per concrete
    /// session, so Session A cannot suppress Session B's prompt guidance.
    #[test]
    fn test_cross_session_cooldown_is_session_scoped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let session_a_path = cooldown_file("session-a").expect("session A cooldown path");
        let session_b_path = cooldown_file("session-b").expect("session B cooldown path");
        let _ = std::fs::remove_file(&session_a_path);
        let _ = std::fs::remove_file(&session_b_path);

        write_cooldown(&fs, "session-a");

        assert_ne!(
            session_a_path, session_b_path,
            "cooldown_file() must produce distinct paths for distinct session_ids"
        );

        assert!(
            cooldown_expired(&fs, "session-b"),
            concat!(
                "Session B's cooldown check must not be suppressed by Session A's stamp. ",
                "The cooldown file path must include the session_id so that each ",
                "session has its own independent cooldown window."
            )
        );

        // Cleanup: remove the stamp file Session A actually wrote.
        let _ = std::fs::remove_file(&session_a_path);
        let _ = std::fs::remove_file(&session_b_path);
    }
}
