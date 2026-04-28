//! Context Monitor — Two-phase hook
//!
//! **Stop phase:** Reads context window usage from the Stop event payload,
//! writes current zone to `~/.claude/metrics/context-zone.json`.
//!
//! **UserPromptSubmit phase:** Reads zone state, injects zone-specific
//! strategy guidance when usage is above 50%.

use sentinel_domain::constants;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{EnvPort, FileSystemPort, HookContext};

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

    fn label(self) -> &'static str {
        match self {
            Self::Green => "Green",
            Self::Yellow => "Yellow",
            Self::Orange => "Orange",
            Self::Red => "Red",
        }
    }

    fn strategy(self) -> &'static str {
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
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn state_file(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = super::metrics_dir(&home);
    fs.create_dir_all(&dir).ok()?;
    Some(dir.join(format!("context-zone-{session_id}.json")))
}

fn cooldown_file(env: &dyn EnvPort) -> PathBuf {
    let session_id = env
        .var("CLAUDE_SESSION_ID")
        .or_else(|| env.var("SESSION_ID"))
        .unwrap_or_else(|| "default".to_string());
    std::env::temp_dir().join(format!("claude-context-monitor-{session_id}-last"))
}

fn cooldown_expired(fs: &dyn FileSystemPort, env: &dyn EnvPort) -> bool {
    let content = match fs.read_to_string(&cooldown_file(env)) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let last: u64 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return true,
    };
    now_ms().saturating_sub(last) >= COOLDOWN_MS
}

fn write_cooldown(fs: &dyn FileSystemPort, env: &dyn EnvPort) {
    let _ = fs.write(&cooldown_file(env), now_ms().to_string().as_bytes());
}

/// Extract usage percentage from context_window payload.
fn extract_usage_pct(context: &serde_json::Value) -> Option<f64> {
    context
        .get("percentUsed")
        .and_then(|v| v.as_f64())
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

    let zone = Zone::from_pct(pct);
    let session_id = input.session_id.as_deref().unwrap_or("unknown");

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
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
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

    if !cooldown_expired(ctx.fs, ctx.env) {
        return HookOutput::allow();
    }

    write_cooldown(ctx.fs, ctx.env);

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
        assert!(cooldown_expired(ctx.fs, ctx.env));
    }
    /// Regression test: cooldown stamp file is keyed on the process temp_dir
    /// but NOT on session_id. Session A writes the stamp; Session B (different
    /// session_id, same process, same temp_dir) must NOT be suppressed by
    /// Session A's stamp -- but with the current code it IS suppressed.
    ///
    /// Expected (correct) behaviour: `cooldown_expired` returns `true` for
    /// Session B because it is a fresh session.
    ///
    /// Actual (buggy) behaviour: `cooldown_expired` returns `false` because
    /// the stamp file path does not include a session_id, so Session A's
    /// recently-written stamp suppresses Session B.
    ///
    /// This test therefore FAILS on the current codebase (red regression test).
    #[test]
    fn test_cross_session_cooldown_suppression_bug() {
        use super::super::FileSystemPort;
        use std::path::{Path, PathBuf};

        // A minimal FileSystemPort that delegates to real std::fs so that
        // `cooldown_file()` -- which calls std::env::temp_dir() directly --
        // resolves to an actual on-disk path.
        struct RealTestFs;
        impl FileSystemPort for RealTestFs {
            fn home_dir(&self) -> Option<PathBuf> {
                dirs::home_dir()
            }
            fn read_to_string(&self, p: &Path) -> anyhow::Result<String> {
                std::fs::read_to_string(p).map_err(Into::into)
            }
            fn write(&self, p: &Path, data: &[u8]) -> anyhow::Result<()> {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(p, data).map_err(Into::into)
            }
            fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> {
                std::fs::create_dir_all(p).map_err(Into::into)
            }
            fn read_dir(&self, p: &Path) -> anyhow::Result<Vec<PathBuf>> {
                Ok(std::fs::read_dir(p)?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .collect())
            }
            fn exists(&self, p: &Path) -> bool {
                p.exists()
            }
            fn is_dir(&self, p: &Path) -> bool {
                p.is_dir()
            }
            fn metadata(&self, p: &Path) -> anyhow::Result<std::fs::Metadata> {
                std::fs::metadata(p).map_err(Into::into)
            }
            fn append(&self, p: &Path, data: &[u8]) -> anyhow::Result<()> {
                use std::io::Write;
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)?;
                f.write_all(data).map_err(Into::into)
            }
        }

        let fs = RealTestFs;
        use crate::hooks::test_support::StubEnv;

        // -- Session A fires: write a cooldown stamp right now
        // Each session has its own EnvPort so the test isolates cleanly
        // without touching process-global env state.
        let env_a = StubEnv::with(&[("CLAUDE_SESSION_ID", "session-a")]);
        let session_a_path = cooldown_file(&env_a);
        write_cooldown(&fs, &env_a);

        // -- Session B arrives immediately after
        // Session B is a completely different session. With the session-id-
        // scoped stamp file, Session A's stamp must not suppress Session B's
        // cooldown check.
        let env_b = StubEnv::with(&[("CLAUDE_SESSION_ID", "session-b")]);
        let session_b_path = cooldown_file(&env_b);

        assert_ne!(
            session_a_path, session_b_path,
            "cooldown_file() must produce distinct paths for distinct session_ids"
        );

        assert!(
            cooldown_expired(&fs, &env_b),
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
