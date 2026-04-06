//! Account Cascade hook — auto-switch all MCP servers after account change.
//!
//! Fires on `PostToolUse`. When the completed tool is `account_switch`,
//! `account_rotate`, or `project_switch`, this hook looks up the matching
//! project config and injects `additionalContext` telling Claude to cascade
//! the switch to Linear, Doppler, Blacksmith, and any other mapped services.
//!
//! This turns `project_switch` from "here are the commands to run" into
//! "Claude, run these commands now", and makes `account_switch`/`account_rotate`
//! automatically cascade to all related services.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Tool name suffixes that trigger cascade behavior.
/// Note: `account_rotate` is intentionally excluded — rate limit rotation
/// swaps Claude credentials only; it should NOT cascade to other services.
const TRIGGER_SUFFIXES: &[&str] = &[
    "account_switch",
    "project_switch",
];

/// Process a `PostToolUse` event. Returns context injection if a cascade is needed.
pub fn process(input: &HookInput) -> HookOutput {
    let Some(tool_name) = input.tool_name.as_deref() else {
        return HookOutput::allow();
    };

    // Only fire for account/project switch tools
    let is_trigger = TRIGGER_SUFFIXES.iter().any(|suffix| tool_name.ends_with(suffix));
    if !is_trigger {
        return HookOutput::allow();
    }

    // Get the tool result text to determine what was switched to
    let result_text = input
        .tool_result
        .as_ref()
        .map(extract_result_text)
        .unwrap_or_default();

    // If the tool errored, don't cascade.
    // Check for known error prefixes from accounts-mcp, not bare "error".
    if result_text.is_empty() || is_error_result(input, &result_text) {
        return HookOutput::allow();
    }

    let Some(home) = dirs::home_dir() else {
        return HookOutput::allow();
    };

    let projects_dir = home.join(".claude").join("projects");

    if tool_name.ends_with("project_switch") {
        // project_switch already returns instructions — enhance with auto-execute directive
        return inject_auto_execute(&result_text);
    }

    // For account_switch / account_rotate: find matching project and cascade
    let switched_account = parse_switched_account(&result_text);
    if switched_account.is_empty() {
        return HookOutput::allow();
    }

    let instructions = build_cascade_instructions(&projects_dir, &switched_account);
    if instructions.is_empty() {
        tracing::debug!(
            switched_account,
            "No project config maps to this account — no cascade"
        );
        return HookOutput::allow();
    }

    tracing::info!(
        switched_account,
        services = instructions.len(),
        "Cascading account switch to mapped services"
    );

    let context = format!(
        "[Account Cascade] Account switched to **{switched_account}**. \
         Execute these service switches NOW to stay in sync:\n\n{}",
        instructions.join("\n")
    );

    HookOutput::inject_context(HookEvent::PostToolUse, context)
}

/// Check if the tool result indicates an error.
/// Checks the JSON `isError` field first (Claude Code standard), then known text patterns.
fn is_error_result(input: &HookInput, result_text: &str) -> bool {
    // Claude Code sets isError on MCP tool failures
    if let Some(result) = &input.tool_result {
        if result.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
            return true;
        }
    }

    // Known error prefixes from accounts-mcp
    result_text.starts_with("Error:")
        || result_text.contains("not found")
        || result_text.contains("No saved accounts")
        || result_text.contains("ALL accounts are exhausted")
}

/// Extract text content from `tool_result` JSON.
/// Claude Code sends tool results as either a string or `{ content: [{ text: "..." }] }`.
fn extract_result_text(value: &serde_json::Value) -> String {
    // Direct string
    if let Some(s) = value.as_str() {
        return s.to_string();
    }

    // { content: [{ text: "..." }] }
    if let Some(content) = value.get("content").and_then(|c| c.as_array()) {
        let texts: Vec<&str> = content
            .iter()
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect();
        if !texts.is_empty() {
            return texts.join("\n");
        }
    }

    // Fallback: serialize the whole thing
    value.to_string()
}

/// Parse the account name from the tool result.
///
/// `account_switch` returns: "Switched to **name** (email, plan)"
fn parse_switched_account(result_text: &str) -> String {
    extract_bold_text(result_text).unwrap_or_default()
}

/// Extract the first **bold** text from a string.
fn extract_bold_text(s: &str) -> Option<String> {
    let start = s.find("**")? + 2;
    let rest = &s[start..];
    let end = rest.find("**")?;
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// When `project_switch` returned its instructions, wrap them with an auto-execute directive.
fn inject_auto_execute(result_text: &str) -> HookOutput {
    let context = format!(
        "[Account Cascade] Project switch detected. Execute ALL the following \
         service switches immediately — do not ask for confirmation:\n\n{result_text}"
    );
    HookOutput::inject_context(HookEvent::PostToolUse, context)
}

/// Scan project configs for one whose `claude_account` matches the switched account name.
/// Falls back to matching by project name or aliases if `claude_account` is not set.
/// Returns a list of MCP tool call instructions for all mapped services.
fn build_cascade_instructions(projects_dir: &Path, account_name: &str) -> Vec<String> {
    if !projects_dir.is_dir() {
        return vec![];
    }

    let configs = load_project_configs(projects_dir);

    // Strategy 1: exact match on claude_account field
    let project = configs.iter().find(|p| {
        p.get("claude_account")
            .is_some_and(|v| v == account_name)
    });

    // Strategy 2: match account name against project name or aliases
    let project = project.or_else(|| {
        configs.iter().find(|p| {
            let name_match = p
                .get("name")
                .is_some_and(|n| n.eq_ignore_ascii_case(account_name));
            let alias_match = p.get("aliases").is_some_and(|aliases| {
                // aliases is stored as a raw string like ["foo", "bar"]
                aliases
                    .to_lowercase()
                    .contains(&account_name.to_lowercase())
            });
            name_match || alias_match
        })
    });

    let Some(project) = project else {
        return vec![];
    };

    let mut instructions = Vec::new();
    let mut step = 1;

    if let Some(linear_account) = project.get("linear_account") {
        instructions.push(format!(
            "{step}. `mcp__linear__switch_account(account_name: \"{linear_account}\")` — Linear workspace"
        ));
        step += 1;
    }

    if let Some(doppler_account) = project.get("doppler_account") {
        // Only cascade Doppler if an explicit account mapping exists.
        // doppler_project/doppler_config are project-level, not account-level.
        instructions.push(format!(
            "{step}. `mcp__doppler__switch_account(account_id: \"{doppler_account}\")` — Doppler secrets"
        ));
        step += 1;
    }

    if let Some(blacksmith_account) = project.get("blacksmith_account") {
        instructions.push(format!(
            "{step}. `mcp__blacksmith__switch_account(account_name: \"{blacksmith_account}\")` — Blacksmith CI"
        ));
    }

    instructions
}

/// Load all project configs from `~/.claude/projects/*.md` as flat key-value maps.
/// Only parses YAML frontmatter (between --- fences).
fn load_project_configs(dir: &Path) -> Vec<HashMap<String, String>> {
    let mut configs = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return configs;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('_'))
        {
            continue;
        }
        if let Some(fields) = parse_frontmatter(&path) {
            configs.push(fields);
        }
    }

    configs
}

/// Parse YAML frontmatter from a markdown file into a flat key-value map.
fn parse_frontmatter(path: &Path) -> Option<HashMap<String, String>> {
    let content = fs::read_to_string(path).ok()?;
    let mut lines = content.lines();

    if lines.next()?.trim() != "---" {
        return None;
    }

    let mut fields: HashMap<String, String> = HashMap::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        // Skip comments, list items, and indented lines
        if trimmed.starts_with('#')
            || trimmed.starts_with('-')
            || line.starts_with("  ")
            || line.starts_with('\t')
        {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim().to_string();
            let value = value.trim().trim_matches('"').trim_matches('\'').to_string();
            if !value.is_empty() {
                fields.insert(key, value);
            }
        }
    }

    if fields.is_empty() {
        None
    } else {
        Some(fields)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_bold_text() {
        assert_eq!(
            extract_bold_text("Switched to **gary-max** (gary@example.com, Max 20x)"),
            Some("gary-max".to_string())
        );
        assert_eq!(
            extract_bold_text("no bold here"),
            None
        );
        assert_eq!(
            extract_bold_text("****"),
            None
        );
    }

    #[test]
    fn test_parse_switched_account_switch() {
        let result = parse_switched_account(
            "Switched to **gary-max** (gary@example.com, Max 20x)",
        );
        assert_eq!(result, "gary-max");
    }

    #[test]
    fn test_parse_switched_account_no_bold() {
        let result = parse_switched_account(
            "Error: profile not found",
        );
        assert_eq!(result, "");
    }

    #[test]
    fn test_trigger_detection() {
        assert!(TRIGGER_SUFFIXES.iter().any(|s| "mcp__accounts__account_switch".ends_with(s)));
        assert!(TRIGGER_SUFFIXES.iter().any(|s| "mcp__accounts__project_switch".ends_with(s)));
        // account_rotate is intentionally NOT a trigger — rate limit rotation shouldn't cascade
        assert!(!TRIGGER_SUFFIXES.iter().any(|s| "mcp__accounts__account_rotate".ends_with(s)));
        assert!(!TRIGGER_SUFFIXES.iter().any(|s| "mcp__accounts__account_list".ends_with(s)));
    }

    #[test]
    fn test_process_ignores_non_trigger_tools() {
        let mut input = HookInput::default();
        input.tool_name = Some("mcp__linear__list_issues".to_string());
        let output = process(&input);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_process_ignores_empty_result() {
        let mut input = HookInput::default();
        input.tool_name = Some("mcp__accounts__account_switch".to_string());
        input.tool_result = None;
        let output = process(&input);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_process_ignores_error_result() {
        let mut input = HookInput::default();
        input.tool_name = Some("mcp__accounts__account_switch".to_string());
        input.tool_result = Some(serde_json::json!("Error: profile 'bad' not found"));
        let output = process(&input);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_is_error_result_checks_is_error_field() {
        let mut input = HookInput::default();
        input.tool_result = Some(serde_json::json!({"isError": true, "content": [{"text": "something failed"}]}));
        assert!(is_error_result(&input, "something failed"));
    }

    #[test]
    fn test_is_error_result_allows_normal_text_containing_error_word() {
        let input = HookInput::default();
        // "error" as a substring should NOT trigger false positive
        assert!(!is_error_result(&input, "Switched to **gary-max** — 0 errors in config"));
    }

    #[test]
    fn test_is_error_result_catches_known_patterns() {
        let input = HookInput::default();
        assert!(is_error_result(&input, "Error: profile 'bad' not found"));
        assert!(is_error_result(&input, "Profile 'x' not found"));
        assert!(is_error_result(&input, "No saved accounts"));
    }

    #[test]
    fn test_parse_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.md");
        fs::write(&file, "---\nname: myproject\nclaude_account: gary-max\nlinear_account: gary@test.com (workspace)\n---\n# Hello").unwrap();

        let fields = parse_frontmatter(&file).unwrap();
        assert_eq!(fields.get("name").unwrap(), "myproject");
        assert_eq!(fields.get("claude_account").unwrap(), "gary-max");
        assert_eq!(fields.get("linear_account").unwrap(), "gary@test.com (workspace)");
    }

    #[test]
    fn test_build_cascade_instructions_with_claude_account() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.md");
        fs::write(&file, "---\nname: testproject\nclaude_account: gary-max\nlinear_account: gary@test.com (ws)\ndoppler_account: gary@workplace\nblacksmith_account: myorg\n---\n").unwrap();

        let instructions = build_cascade_instructions(dir.path(), "gary-max");
        assert_eq!(instructions.len(), 3);
        assert!(instructions[0].contains("linear"));
        assert!(instructions[1].contains("doppler"));
        assert!(instructions[2].contains("blacksmith"));
    }

    #[test]
    fn test_build_cascade_doppler_project_does_not_cascade() {
        // doppler_project is project-level config, not account-level — should NOT cascade
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.md");
        fs::write(&file, "---\nname: testproject\nclaude_account: gary-max\nlinear_account: gary@test.com (ws)\ndoppler_project: myapp\n---\n").unwrap();

        let instructions = build_cascade_instructions(dir.path(), "gary-max");
        assert_eq!(instructions.len(), 1); // only linear, no doppler
        assert!(instructions[0].contains("linear"));
    }

    #[test]
    fn test_build_cascade_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.md");
        fs::write(&file, "---\nname: testproject\nclaude_account: other-account\n---\n").unwrap();

        let instructions = build_cascade_instructions(dir.path(), "gary-max");
        assert!(instructions.is_empty());
    }

    #[test]
    fn test_build_cascade_fallback_matches_by_project_name() {
        // No claude_account field, but project name matches account name
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("corvus.md");
        fs::write(&file, "---\nname: corvus\nlinear_account: gary@test.com (corvus)\n---\n").unwrap();

        let instructions = build_cascade_instructions(dir.path(), "corvus");
        assert_eq!(instructions.len(), 1);
        assert!(instructions[0].contains("linear"));
    }

    #[test]
    fn test_build_cascade_fallback_matches_by_alias() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("firefly.md");
        fs::write(&file, "---\nname: firefly-pro\naliases: [\"firefly\", \"crm\", \"fir\"]\nlinear_account: gary@fp.com (firefly-pro)\n---\n").unwrap();

        let instructions = build_cascade_instructions(dir.path(), "firefly");
        assert_eq!(instructions.len(), 1);
        assert!(instructions[0].contains("linear"));
    }

    #[test]
    fn test_extract_result_text_string() {
        let val = serde_json::json!("Switched to **gary-max**");
        assert_eq!(extract_result_text(&val), "Switched to **gary-max**");
    }

    #[test]
    fn test_extract_result_text_content_array() {
        let val = serde_json::json!({
            "content": [{ "type": "text", "text": "Switched to **gary-max**" }]
        });
        assert_eq!(extract_result_text(&val), "Switched to **gary-max**");
    }
}
