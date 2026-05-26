//! Evidence Collector
//!
//! Captures tool call results as evidence for the proof chain.
//! Runs on every `PostToolUse` event.

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::state::PhaseCollectionState;

/// Process evidence collection for a tool result
pub fn process(input: &HookInput, collection: Option<&mut PhaseCollectionState>) -> HookOutput {
    // Tool-call trace for the legatus per-instruction Result
    // (Item E): every PostToolUse — even those with no active
    // phase — gets recorded so the Stop hook can attribute the
    // tool list to the operator-relayed instructions for this
    // turn. Cheap append; legatus_client::take_tool_calls drains.
    if let (Some(session_id), Some(tool_name)) =
        (input.session_id.as_deref(), input.tool_name.as_deref())
    {
        crate::legatus_client::note_tool_call(session_id, tool_name);
    }

    let collection = match collection {
        Some(c) => c,
        None => return HookOutput::allow(), // No active phase collection
    };

    let tool = input.tool_name.as_deref().unwrap_or("unknown");

    // Record the tool call
    let args_summary = input
        .tool_input
        .as_ref()
        .map(|v| {
            serde_json::to_string(v)
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>()
        })
        .unwrap_or_default();

    collection.collector.record_tool_call(tool, &args_summary);

    // Record tool result if available
    if let Some(result) = &input.tool_result {
        let result_summary = serde_json::to_string(result)
            .unwrap_or_default()
            .chars()
            .take(500)
            .collect::<String>();
        collection
            .collector
            .record_tool_result(tool, &result_summary, true);
    }

    // Check if this is a Read() of a phase file
    if tool == "Read" {
        if let Some(args) = &input.tool_input {
            if let Some(path) = args.get("file_path").and_then(|v| v.as_str()) {
                if path.contains("/phases/") || path.contains("\\phases\\") {
                    collection.collector.record_phase_file_read();
                }
            }
        }
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_no_collection_returns_allow() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, None);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_records_tool_call() {
        let mut collection = PhaseCollectionState::new("claim", "linear");
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(json!({ "command": "npm test" })),
            ..Default::default()
        };
        let output = process(&input, Some(&mut collection));
        assert!(output.blocked.is_none());
        assert!(!collection.collector.is_empty());
    }

    #[test]
    fn test_records_tool_result() {
        let mut collection = PhaseCollectionState::new("claim", "linear");
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(json!({ "command": "npm test" })),
            tool_result: Some(json!("5 passing")),
            ..Default::default()
        };
        let output = process(&input, Some(&mut collection));
        assert!(output.blocked.is_none());
        // 1 tool call + 1 tool result = 2 entries
        assert_eq!(collection.collector.len(), 2);
    }

    #[test]
    fn test_detects_phase_file_read() {
        let mut collection = PhaseCollectionState::new("claim", "linear");
        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(json!({ "file_path": "~/.claude/skills/linear/phases/claim.md" })),
            ..Default::default()
        };
        process(&input, Some(&mut collection));
        let evidence = collection.collector.finalize();
        assert!(evidence.phase_file_read);
    }

    #[test]
    fn test_non_phase_read_no_phase_flag() {
        let mut collection = PhaseCollectionState::new("claim", "linear");
        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(json!({ "file_path": "src/main.rs" })),
            ..Default::default()
        };
        process(&input, Some(&mut collection));
        let evidence = collection.collector.finalize();
        assert!(!evidence.phase_file_read);
    }

    #[test]
    fn test_windows_path_phase_detection() {
        let mut collection = PhaseCollectionState::new("claim", "linear");
        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(
                json!({ "file_path": "C:\\Users\\gary\\.claude\\skills\\linear\\phases\\claim.md" }),
            ),
            ..Default::default()
        };
        process(&input, Some(&mut collection));
        let evidence = collection.collector.finalize();
        assert!(evidence.phase_file_read);
    }
}
