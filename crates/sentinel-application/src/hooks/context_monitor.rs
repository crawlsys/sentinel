//! Context Monitor — Two-phase hook
//!
//! **Stop phase:** Reads context window usage from the Stop event payload,
//! writes current zone to `~/.claude/metrics/context-zone.json`.
//!
//! **UserPromptSubmit phase:** Reads zone state, injects zone-specific
//! strategy guidance when usage is above 50%.

use sentinel_domain::constants;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

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

fn state_file() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let dir = home.join(".claude").join("metrics");
    fs::create_dir_all(&dir).ok()?;
    Some(dir.join("context-zone.json"))
}

fn cooldown_file() -> PathBuf {
    std::env::temp_dir().join("claude-context-monitor-last")
}

fn cooldown_expired() -> bool {
    let content = match fs::read_to_string(cooldown_file()) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let last: u64 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return true,
    };
    now_ms().saturating_sub(last) >= COOLDOWN_MS
}

fn write_cooldown() {
    let _ = fs::write(cooldown_file(), now_ms().to_string());
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

pub fn process_stop(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    let context = match &input.context_window {
        Some(ctx) => ctx,
        None => return HookOutput::allow(),
    };

    let pct = match extract_usage_pct(context) {
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

    if let Some(path) = state_file() {
        let _ = fs::write(&path, serde_json::to_string(&state).unwrap_or_default());
    }

    if pct > 65.0 {
        tracing::warn!(
            usage = pct,
            zone = zone.label(),
            "Context window usage elevated"
        );
    }

    HookOutput::allow()
}

// ---------------------------------------------------------------------------
// UserPromptSubmit phase: inject zone-specific strategy
// ---------------------------------------------------------------------------

pub fn process_prompt(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    let path = match state_file() {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HookOutput::allow(),
    };

    let state: ContextState = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => return HookOutput::allow(),
    };

    // Only inject for the current session
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    if state.session_id != session_id {
        return HookOutput::allow();
    }

    let zone = Zone::from_pct(state.percent_used);

    // Green zone — no guidance needed
    if zone == Zone::Green {
        return HookOutput::allow();
    }

    if !cooldown_expired() {
        return HookOutput::allow();
    }

    write_cooldown();

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
        let ctx = json!({ "percentUsed": 42.5 });
        assert_eq!(extract_usage_pct(&ctx), Some(42.5));
    }

    #[test]
    fn test_extract_usage_pct_used_total() {
        let ctx = json!({ "used": 75000, "total": 100000 });
        assert_eq!(extract_usage_pct(&ctx), Some(75.0));
    }

    #[test]
    fn test_extract_usage_pct_empty() {
        let ctx = json!({});
        assert_eq!(extract_usage_pct(&ctx), None);
    }

    #[test]
    fn test_stop_no_context_window() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_stop_writes_state() {
        let input = HookInput {
            context_window: Some(json!({ "percentUsed": 60.0 })),
            session_id: Some("test-ctx".into()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());

        if let Some(path) = state_file() {
            if path.exists() {
                let state: ContextState =
                    serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
                assert_eq!(state.zone, "Yellow");
            }
        }
    }

    #[test]
    fn test_prompt_green_zone_no_inject() {
        // Write green zone state
        if let Some(path) = state_file() {
            let state = ContextState {
                percent_used: 30.0,
                zone: "Green".into(),
                session_id: "test-green".into(),
                ts: "2026-03-05".into(),
            };
            let _ = fs::write(&path, serde_json::to_string(&state).unwrap());
        }

        let input = HookInput {
            session_id: Some("test-green".into()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process_prompt(&input, &ctx);
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
        let _ = fs::remove_file(cooldown_file());
        assert!(cooldown_expired());
        write_cooldown();
        assert!(!cooldown_expired());
        let _ = fs::remove_file(cooldown_file());
    }
}
