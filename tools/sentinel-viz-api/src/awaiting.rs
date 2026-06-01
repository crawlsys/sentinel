use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::transcript;

/// `(kind, question, options)` where kind is one of:
///   - `"question"` — last pending `tool_use` is `AskUserQuestion`
///   - `"reply"`    — agent's last turn ended on text with no user reply
///   - None         — not awaiting
///
/// Mirrors `detect_awaiting_user()` in `viz_server.py`.
pub fn detect(path: &Path) -> (Option<&'static str>, Option<String>, Value) {
    let Ok(file) = std::fs::read_to_string(path) else { return (None, None, Value::Array(vec![])) };

    let mut pending: HashMap<String, PendingTool> = HashMap::new();
    let mut last_pending_id: Option<String> = None;
    let mut last_assistant_text = String::new();
    let mut last_entry_type = String::new();

    for line in file.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(r): Result<Value, _> = serde_json::from_str(line) else { continue };
        let typ = r.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let msg = r.get("message").cloned().unwrap_or(Value::Null);

        match typ {
            "assistant" => {
                last_entry_type = "assistant".to_string();
                if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                    let text_blocks: Vec<&str> = blocks
                        .iter()
                        .filter_map(|c| {
                            if c.get("type").and_then(|v| v.as_str()) == Some("text") {
                                c.get("text")
                                    .and_then(|v| v.as_str())
                                    .filter(|s| !s.trim().is_empty())
                            } else {
                                None
                            }
                        })
                        .collect();
                    if let Some(last) = text_blocks.last() {
                        last_assistant_text = (*last).to_string();
                    }
                    for c in blocks {
                        if c.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                            continue;
                        }
                        let Some(tu_id) = c.get("id").and_then(|v| v.as_str()) else { continue };
                        let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let input = c.get("input").cloned().unwrap_or(Value::Null);
                        pending.insert(tu_id.to_string(), PendingTool { name, input });
                        last_pending_id = Some(tu_id.to_string());
                    }
                }
            }
            "user" => {
                last_entry_type = "user".to_string();
                if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                    for c in content {
                        if c.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                            continue;
                        }
                        let Some(tu_id) = c.get("tool_use_id").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        if pending.remove(tu_id).is_some()
                            && last_pending_id.as_deref() == Some(tu_id)
                        {
                            last_pending_id = None;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // 1) Structured AskUserQuestion still pending
    if let Some(id) = &last_pending_id {
        if let Some(p) = pending.get(id) {
            if p.name == "AskUserQuestion" {
                let qs = p.input.get("questions").and_then(|v| v.as_array());
                if let Some(qs) = qs {
                    if let Some(q0) = qs.first().and_then(|v| v.as_object()) {
                        let q_text = q0
                            .get("question")
                            .and_then(|v| v.as_str())
                            .map(|s| transcript::trim(s, 600));
                        let options = q0.get("options").cloned().unwrap_or(Value::Array(vec![]));
                        return (Some("question"), q_text, options);
                    }
                }
                return (Some("question"), None, Value::Array(vec![]));
            }
            // Non-question tool pending → agent is working
            return (None, None, Value::Array(vec![]));
        }
    }

    // 2) Agent finished cleanly on assistant text
    if last_entry_type == "assistant" && !last_assistant_text.is_empty() {
        return (
            Some("reply"),
            Some(transcript::trim(&last_assistant_text, 600)),
            Value::Array(vec![]),
        );
    }
    (None, None, Value::Array(vec![]))
}

struct PendingTool {
    name: String,
    input: Value,
}
