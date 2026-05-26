use std::fmt::Write as _;

use sentinel_domain::events::HookInput;

pub(super) fn append_block_context(message: impl Into<String>, input: &HookInput) -> String {
    let mut message = message.into();
    let session_id = input.session_id.as_deref().and_then(sanitize_single_line);
    let cwd = input.cwd.as_deref().and_then(sanitize_single_line);
    let tool_name = input.tool_name.as_deref().and_then(sanitize_single_line);
    let target = extract_target(input);
    let command = extract_command(input);

    if session_id.is_none()
        && cwd.is_none()
        && tool_name.is_none()
        && target.is_none()
        && command.is_none()
    {
        return message;
    }

    if !message.ends_with('\n') {
        message.push('\n');
    }

    if let Some(session_id) = session_id {
        let _ = writeln!(message, "[sentinel] session: {session_id}");
        let _ = writeln!(
            message,
            "[sentinel] state: ~/.claude/sentinel/state/{session_id}.json"
        );
    }

    if let Some(cwd) = cwd {
        let _ = writeln!(message, "[sentinel] cwd: {cwd}");
    }

    if let Some(tool_name) = tool_name {
        let _ = writeln!(message, "[sentinel] tool: {tool_name}");
    }

    if let Some(target) = target {
        let _ = writeln!(message, "[sentinel] target: {target}");
    }

    if let Some(command) = command {
        let _ = writeln!(message, "[sentinel] command: {command}");
    }

    if message.ends_with('\n') {
        message.pop();
    }

    message
}

fn extract_target(input: &HookInput) -> Option<String> {
    let tool_input = input.tool_input.as_ref()?;
    let target = tool_input
        .get("file_path")
        .or_else(|| tool_input.get("path"))
        .or_else(|| tool_input.get("uri"))
        .and_then(|value| value.as_str())?;

    sanitize_single_line(target)
}

fn extract_command(input: &HookInput) -> Option<String> {
    let tool_input = input.tool_input.as_ref()?;
    let command = tool_input.get("command").and_then(|value| value.as_str())?;
    sanitize_single_line(command)
}

fn sanitize_single_line(value: &str) -> Option<String> {
    let sanitized = value
        .chars()
        .map(|ch| match ch {
            '\r' | '\n' | '\t' => ' ',
            _ => ch,
        })
        .collect::<String>();

    let mut trimmed = sanitized.trim().to_string();
    if trimmed.len() > 400 {
        trimmed.truncate(397);
        trimmed.push_str("...");
    }

    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::append_block_context;
    use sentinel_domain::events::HookInput;

    #[test]
    fn appends_session_state_and_cwd() {
        let input = HookInput {
            session_id: Some("abc-123".to_string()),
            cwd: Some("C:\\Users\\garys\\repo".to_string()),
            tool_name: Some("Edit".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": "C:\\Users\\garys\\.claude.json"
            })),
            ..Default::default()
        };

        let message = append_block_context("blocked", &input);

        assert!(message.contains("[sentinel] session: abc-123"));
        assert!(message.contains("[sentinel] state: ~/.claude/sentinel/state/abc-123.json"));
        assert!(message.contains("[sentinel] cwd: C:\\Users\\garys\\repo"));
        assert!(message.contains("[sentinel] tool: Edit"));
        assert!(message.contains("[sentinel] target: C:\\Users\\garys\\.claude.json"));
    }

    #[test]
    fn omits_empty_context_fields() {
        let input = HookInput {
            session_id: Some(" \n\t ".to_string()),
            cwd: None,
            ..Default::default()
        };

        assert_eq!(append_block_context("blocked", &input), "blocked");
    }

    #[test]
    fn appends_command_for_shell_blocks() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({
                "command": "git commit -m \"feat: add hookdeck env\""
            })),
            ..Default::default()
        };

        let message = append_block_context("blocked", &input);

        assert!(message.contains("[sentinel] tool: Bash"));
        assert!(message.contains("[sentinel] command: git commit -m \"feat: add hookdeck env\""));
    }
}
