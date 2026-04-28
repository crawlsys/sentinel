//! Build & Deploy Notification — PostToolUse hook
//!
//! Detects when Bash tool calls complete cargo builds, test suites,
//! or git pushes, and emits channel events for real-time notification.
//! Failures are also pushed to ntfy (`gary-somerhalder-claude-code-attention`
//! for build failures, `gary-somerhalder-deploys` for deploy events).

use sentinel_domain::events::{HookInput, HookOutput};

use crate::ntfy_push;

const TOPIC_DEPLOYS: &str = "gary-somerhalder-deploys";

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
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
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
        crate::channel_events::emit(
            ctx.fs, ctx.env,
            "build_completed", &summary, meta,
            input.session_id.as_deref(), input.cwd.as_deref(), Some("build_notify"),
        );

        // Phone push for FAILURES only — build successes stay quiet.
        if !succeeded {
            let project = project_name(input.cwd.as_deref());
            let title = format!("Build FAILED: {project}");
            let snippet = first_error_line(result_text)
                .unwrap_or_else(|| truncate(command, 120).to_string());
            ntfy_push::push_attention(ctx.fs, ctx.env, &title, &snippet, 4, &["x"]);
        }

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
        crate::channel_events::emit(
            ctx.fs, ctx.env,
            "deploy_completed", &summary, meta,
            input.session_id.as_deref(), input.cwd.as_deref(), Some("build_notify"),
        );

        // Phone push for both success and failure — deploys are infrequent
        // enough that knowing they finished is genuinely useful.
        let project = project_name(input.cwd.as_deref());
        let (title, priority, tag) = if succeeded {
            (format!("Deploy OK: {project}{target}"), 2_u8, "rocket")
        } else {
            (format!("Deploy FAILED: {project}{target}"), 4_u8, "x")
        };
        let body = truncate(command, 120).to_string();
        ntfy_push::push_to_topic(ctx.fs, ctx.env, TOPIC_DEPLOYS, &title, &body, priority, &[tag]);
    }

    HookOutput::allow()
}

/// Best-effort project name from a cwd path (basename), falls back to "unknown".
fn project_name(cwd: Option<&str>) -> String {
    cwd.and_then(|p| {
        std::path::Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
    })
    .unwrap_or_else(|| "unknown".to_string())
}

/// Pull the first `error[E…]:` or `error:` line from build output, if any.
/// Returns the trimmed line capped at 200 chars.
fn first_error_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| l.starts_with("error[E") || l.starts_with("error:"))
        .map(|l| {
            if l.len() > 200 {
                l[..200].to_string()
            } else {
                l.to_string()
            }
        })
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
    fn test_project_name_basename() {
        #[cfg(windows)]
        assert_eq!(project_name(Some(r"C:\Users\garys\Documents\GitHub\sentinel")), "sentinel");
        assert_eq!(project_name(Some("/home/g/repo")), "repo");
        assert_eq!(project_name(None), "unknown");
    }

    #[test]
    fn test_first_error_line_extracts_rustc_error() {
        let out = "   Compiling foo\nerror[E0432]: unresolved import `bar`\n   --> src/lib.rs:1:5";
        let got = first_error_line(out).unwrap();
        assert!(got.starts_with("error[E0432]"), "got: {got}");
    }

    #[test]
    fn test_first_error_line_returns_none_on_clean_output() {
        let out = "   Compiling foo v0.1.0\n    Finished `dev` profile [unoptimized] target(s) in 1.23s";
        assert!(first_error_line(out).is_none());
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
