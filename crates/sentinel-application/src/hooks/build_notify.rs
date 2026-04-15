//! Build & Deploy Notification — PostToolUse hook
//!
//! Detects when Bash tool calls complete cargo builds, test suites,
//! or git pushes, and emits channel events for real-time notification.

use sentinel_domain::events::{HookInput, HookOutput};

/// Patterns that indicate a build command
const BUILD_PATTERNS: &[&str] = &[
    "cargo build --release",
    "cargo build -r",
    "cargo test",
    "npm run build",
    "pnpm build",
    "next build",
];

/// Patterns that indicate a deploy/push command
const DEPLOY_PATTERNS: &[&str] = &[
    "git push",
    "wrangler deploy",
    "wrangler publish",
    "netlify deploy",
    "vercel --prod",
    "railway up",
];

/// Process PostToolUse — emit channel events for builds and deploys.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    // Only care about Bash tool completions
    if input.tool_name.as_deref() != Some("Bash") {
        return HookOutput::allow();
    }

    let command = input
        .tool_input
        .as_ref()
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if command.is_empty() {
        return HookOutput::allow();
    }

    let result_text = input
        .tool_result
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Check for build/test commands
    if let Some(pattern) = BUILD_PATTERNS.iter().find(|p| command.contains(**p)) {
        let succeeded = !result_text.contains("error[E")
            && !result_text.contains("FAILED")
            && !result_text.contains("could not compile");

        let summary = if succeeded {
            format!("Build/test completed successfully: `{}`", truncate(command, 80))
        } else {
            format!("Build/test FAILED: `{}`", truncate(command, 80))
        };

        let mut meta = serde_json::Map::new();
        meta.insert(
            "command".to_string(),
            serde_json::Value::String(truncate(command, 200).to_string()),
        );
        meta.insert(
            "pattern".to_string(),
            serde_json::Value::String((*pattern).to_string()),
        );
        meta.insert(
            "success".to_string(),
            serde_json::Value::Bool(succeeded),
        );
        crate::channel_events::emit("build_completed", &summary, meta);
        return HookOutput::allow();
    }

    // Check for deploy/push commands
    if let Some(pattern) = DEPLOY_PATTERNS.iter().find(|p| command.contains(**p)) {
        let target = extract_push_target(command);
        let succeeded = !result_text.contains("rejected")
            && !result_text.contains("error:")
            && !result_text.contains("fatal:");

        let summary = if succeeded {
            format!("Deploy completed: `{}`{}", truncate(command, 80), target)
        } else {
            format!("Deploy FAILED: `{}`{}", truncate(command, 80), target)
        };

        let mut meta = serde_json::Map::new();
        meta.insert(
            "command".to_string(),
            serde_json::Value::String(truncate(command, 200).to_string()),
        );
        meta.insert(
            "pattern".to_string(),
            serde_json::Value::String((*pattern).to_string()),
        );
        meta.insert(
            "success".to_string(),
            serde_json::Value::Bool(succeeded),
        );
        if !target.is_empty() {
            meta.insert(
                "target".to_string(),
                serde_json::Value::String(target.trim_start_matches(" → ").to_string()),
            );
        }
        crate::channel_events::emit("deploy_completed", &summary, meta);
    }

    HookOutput::allow()
}

/// Extract push target from a git push command (e.g. "origin main" → " → origin/main")
fn extract_push_target(command: &str) -> String {
    let parts: Vec<&str> = command.split_whitespace().collect();
    if let Some(idx) = parts.iter().position(|p| *p == "push") {
        let remote = parts.get(idx + 1).unwrap_or(&"");
        let branch = parts.get(idx + 2).unwrap_or(&"");
        if !remote.is_empty() && !remote.starts_with('-') {
            if !branch.is_empty() && !branch.starts_with('-') {
                return format!(" → {remote}/{branch}");
            }
            return format!(" → {remote}");
        }
    }
    String::new()
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_push_target() {
        assert_eq!(extract_push_target("git push origin main"), " → origin/main");
        assert_eq!(extract_push_target("git push origin"), " → origin");
        assert_eq!(extract_push_target("git push --force origin main"), "");
        assert_eq!(extract_push_target("git push"), "");
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello");
    }

    #[test]
    fn test_non_bash_ignored() {
        let input = HookInput {
            tool_name: Some("Read".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_bash_without_build_pattern_ignored() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "ls -la"})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
