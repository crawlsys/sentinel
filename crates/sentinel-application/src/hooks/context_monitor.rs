//! Context Monitor
//!
//! Tracks context window usage and warns at thresholds.

use sentinel_domain::events::{HookInput, HookOutput};

/// Process a context monitor hook event
pub fn process(input: &HookInput) -> HookOutput {
    // Context window info comes in the stop event
    let context = match &input.context_window {
        Some(ctx) => ctx,
        None => return HookOutput::allow(),
    };

    // Extract usage percentage if available
    let usage_pct = context
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
        });

    if let Some(pct) = usage_pct {
        if pct > 75.0 {
            tracing::warn!(usage = pct, "Context window usage critical (>75%)");
        } else if pct > 65.0 {
            tracing::info!(usage = pct, "Context window usage high (>65%)");
        }
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_no_context_window() {
        let input = HookInput::default();
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_with_percent_used() {
        let input = HookInput {
            context_window: Some(json!({ "percentUsed": 50.0 })),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_with_used_and_total() {
        let input = HookInput {
            context_window: Some(json!({ "used": 80000, "total": 100000 })),
            ..Default::default()
        };
        let output = process(&input);
        // Should still allow, just log a warning
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_empty_context_object() {
        let input = HookInput {
            context_window: Some(json!({})),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }
}
