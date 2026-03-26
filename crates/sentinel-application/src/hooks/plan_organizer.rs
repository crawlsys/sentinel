//! Plan Organizer Hook
//!
//! Fires on PostToolUse when tool_name == "ExitPlanMode".
//! Injects instructions telling Claude to organize the plan file
//! into ~/.claude/plans/{project}/{descriptive-name}.md instead of
//! leaving it with Claude Code's random filename.
//!
//! The hook detects the project name from the working directory
//! and injects context with the exact move/rename instructions.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::path::Path;

/// Known project directory names → plan subdirectory mappings.
/// Falls back to extracting the last path component of `cwd`.
fn detect_project(cwd: &str) -> String {
    let path = Path::new(cwd);

    // Use the last directory component as the project name
    let dir_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("general");

    // Normalize common repo name patterns to plan folder names
    let project = match dir_name {
        "claude-code-marketplace" => "marketplace",
        "firefly-pro-crm" | "firefly-pro-web-app" => "firefly-pro",
        "sentinel" | "sentinel-launcher" => "sentinel",
        _ => dir_name,
    };

    project.to_string()
}

/// Process an ExitPlanMode PostToolUse event.
/// Injects context instructing Claude to organize the plan file.
pub fn process(input: &HookInput) -> HookOutput {
    // Only fire on ExitPlanMode
    if input.tool_name.as_deref() != Some("ExitPlanMode") {
        return HookOutput::allow();
    }

    let cwd = input.cwd.as_deref().unwrap_or(".");
    let project = detect_project(cwd);

    let context = format!(
        "[Plan Organizer] MANDATORY: Organize the plan file now.\n\
         \n\
         Detected project: \"{project}\"\n\
         \n\
         After the user approves the plan, you MUST:\n\
         1. Create the project subdirectory: mkdir -p ~/.claude/plans/{project}\n\
         2. Move the plan file from its random name to: ~/.claude/plans/{project}/{{descriptive-name}}.md\n\
         3. Use kebab-case for the filename (e.g., add-auth-flow.md, fix-routing-bug.md)\n\
         4. Verify: ls ~/.claude/plans/{project}/\n\
         5. If ~/.claude is a git repo, commit: git -C ~/.claude add plans/{project}/{{name}}.md && git -C ~/.claude commit -m \"plan: {{brief description}}\"\n\
         \n\
         NEVER leave plan files with random names (like squishy-gathering-fog.md) in ~/.claude/plans/."
    );

    HookOutput::inject_context(HookEvent::PostToolUse, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ignores_non_exit_plan_mode() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_ignores_no_tool_name() {
        let input = HookInput::default();
        let output = process(&input);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_fires_on_exit_plan_mode() {
        let input = HookInput {
            tool_name: Some("ExitPlanMode".to_string()),
            cwd: Some("/home/gary/Documents/GitHub/firefly-pro-crm".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("[Plan Organizer]"));
        assert!(ctx.contains("firefly-pro"));
        assert!(ctx.contains("MANDATORY"));
    }

    #[test]
    fn test_detects_marketplace_project() {
        assert_eq!(
            detect_project("/home/gary/Documents/GitHub/claude-code-marketplace"),
            "marketplace"
        );
    }

    #[test]
    fn test_detects_firefly_project() {
        assert_eq!(
            detect_project("/home/gary/Documents/GitHub/firefly-pro-crm"),
            "firefly-pro"
        );
        assert_eq!(
            detect_project("/home/gary/Documents/GitHub/firefly-pro-web-app"),
            "firefly-pro"
        );
    }

    #[test]
    fn test_detects_sentinel_project() {
        assert_eq!(
            detect_project("/home/gary/Documents/GitHub/sentinel"),
            "sentinel"
        );
        assert_eq!(
            detect_project("/home/gary/Documents/GitHub/sentinel-launcher"),
            "sentinel"
        );
    }

    #[test]
    fn test_detects_generic_project() {
        assert_eq!(
            detect_project("/home/gary/Documents/GitHub/legatus"),
            "legatus"
        );
        assert_eq!(detect_project("/home/gary/Documents/GitHub/velo"), "velo");
    }

    #[test]
    fn test_fallback_to_general() {
        // Root path or empty — should get something reasonable
        assert_eq!(detect_project("/"), "general");
    }

    #[test]
    #[cfg(windows)]
    fn test_windows_paths() {
        assert_eq!(
            detect_project("C:\\Users\\gary\\Documents\\GitHub\\sentinel"),
            "sentinel"
        );
        assert_eq!(
            detect_project("C:\\Users\\gary\\Documents\\GitHub\\claude-code-marketplace"),
            "marketplace"
        );
    }

    #[test]
    fn test_context_includes_project_in_path() {
        let input = HookInput {
            tool_name: Some("ExitPlanMode".to_string()),
            cwd: Some("/home/gary/Documents/GitHub/legatus".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("plans/legatus"));
    }

    #[test]
    fn test_hook_event_name_is_post_tool_use() {
        let input = HookInput {
            tool_name: Some("ExitPlanMode".to_string()),
            cwd: Some("/tmp".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        let hso = output.hook_specific_output.unwrap();
        assert_eq!(hso.hook_event_name, "PostToolUse");
    }
}
