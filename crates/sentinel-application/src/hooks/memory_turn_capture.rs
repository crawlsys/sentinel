//! Memory Turn-Capture Hook — LLM extraction of atoms from the conversation.
//!
//! Fires on Stop. Replaces the legacy flat-`.md` ingest (`memory_extract`'s
//! file-sync path): instead of the agent hand-writing `~/.claude/projects/*/
//! memory/*.md` files for a separate hook to parse, this hook reads the turn
//! itself, asks an LLM to extract candidate atomic facts, and routes each
//! through the Memory engine's `memory_capture` dual-judge gate.
//!
//! # Flow
//!
//! 1. Build turn text from `prompt` (user) + `last_assistant_message`.
//! 2. Gate cheaply: skip trivial/empty turns before spending an LLM call.
//! 3. LLM extractor (`ctx.llm`) returns a JSON array of candidate atoms
//!    `{subject, predicate, value, qualifier?, tags?}`.
//! 4. Each candidate → `memory_capture` (dual-judge → write/quarantine/drop).
//!
//! Best-effort and non-blocking: every async call is wrapped in `run_async`
//! (wall-clock timeout) and any failure degrades to "no capture this turn".
//! Never returns anything but `HookOutput::allow()`.

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::ports::{LlmModel, LlmRequest};
use tracing::{debug, warn};

/// Minimum combined turn length (chars) worth sending to the extractor.
/// Below this, a turn is almost certainly an ack / tool-noise with no fact.
const MIN_TURN_CHARS: usize = 200;

/// Cap how much turn text we feed the extractor (cost + latency control).
const MAX_TURN_CHARS: usize = 12_000;

/// Max candidate atoms accepted from one turn (defensive against a runaway
/// extractor flooding the judge gate).
const MAX_CANDIDATES: usize = 8;

/// A candidate atom proposed by the LLM extractor.
#[derive(Debug, serde::Deserialize)]
struct CandidateAtom {
    subject: String,
    predicate: String,
    value: String,
    #[serde(default)]
    qualifier: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

/// The extraction prompt. Asks for STRICT JSON so parsing is deterministic.
fn build_extraction_prompt(turn: &str) -> String {
    format!(
        "You extract durable, reusable facts from a coding-assistant conversation \
turn and emit them as atomic memories. Only extract facts worth remembering \
across future sessions: user preferences, project decisions, constraints, \
non-obvious environment quirks, corrections the user made. Do NOT extract \
transient chatter, restated context, or anything trivially re-derivable.\n\n\
Return ONLY a JSON array (no prose, no code fence) of objects with fields:\n\
  subject   (string, the head entity — kebab or short noun)\n\
  predicate (string, the relation — e.g. prefers, requires, is, decided)\n\
  value     (string, the fact)\n\
  qualifier (optional string, scope/condition)\n\
  tags      (optional array of strings)\n\n\
If nothing is worth remembering, return []. Maximum {MAX_CANDIDATES} items.\n\n\
CONVERSATION TURN:\n{turn}"
    )
}

/// Parse the LLM response into candidate atoms. Tolerant of a stray code
/// fence or leading prose: extracts the first top-level JSON array.
fn parse_candidates(raw: &str) -> Vec<CandidateAtom> {
    let trimmed = raw.trim();
    // Strip a ```json ... ``` fence if present.
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map_or(trimmed, |s| s.trim_end_matches("```").trim());

    // Find the first '[' ... matching ']' span to be robust to leading prose.
    let start = match body.find('[') {
        Some(i) => i,
        None => return Vec::new(),
    };
    let end = match body.rfind(']') {
        Some(i) if i > start => i,
        _ => return Vec::new(),
    };
    let slice = &body[start..=end];

    match serde_json::from_str::<Vec<CandidateAtom>>(slice) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "memory_turn_capture: failed to parse extractor JSON");
            Vec::new()
        }
    }
}

/// Build the turn text from the hook input. Returns `None` if there isn't
/// enough substance to bother extracting.
fn build_turn_text(input: &HookInput) -> Option<String> {
    let prompt = input.prompt.as_deref().unwrap_or("").trim();
    let assistant = input.last_assistant_message.as_deref().unwrap_or("").trim();

    if prompt.is_empty() && assistant.is_empty() {
        return None;
    }

    let mut turn = String::new();
    if !prompt.is_empty() {
        turn.push_str("USER:\n");
        turn.push_str(prompt);
        turn.push_str("\n\n");
    }
    if !assistant.is_empty() {
        turn.push_str("ASSISTANT:\n");
        turn.push_str(assistant);
    }

    if turn.len() < MIN_TURN_CHARS {
        return None;
    }

    // Cap length — keep the head (where decisions/corrections usually land).
    if turn.len() > MAX_TURN_CHARS {
        turn.truncate(MAX_TURN_CHARS);
    }
    Some(turn)
}

/// Derive a project label from cwd (basename), defaulting to "global".
fn project_label(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "global".to_string())
}

/// Send one candidate to the dual-judge `memory_capture` gate.
/// Returns true if the engine accepted (written/reinforced/superseded).
fn capture_candidate(
    memory_mcp: &dyn sentinel_domain::ports::MemoryMcpPort,
    c: &CandidateAtom,
    project: &str,
) -> bool {
    let mut args = serde_json::Map::new();
    args.insert("subject".into(), serde_json::Value::String(c.subject.clone()));
    args.insert(
        "predicate".into(),
        serde_json::Value::String(c.predicate.clone()),
    );
    args.insert("value".into(), serde_json::Value::String(c.value.clone()));
    args.insert(
        "project".into(),
        serde_json::Value::String(project.to_string()),
    );
    if let Some(q) = &c.qualifier {
        args.insert("qualifier".into(), serde_json::Value::String(q.clone()));
    }
    if let Some(ts) = &c.tags {
        args.insert(
            "tags".into(),
            serde_json::Value::Array(
                ts.iter()
                    .map(|t| serde_json::Value::String(t.clone()))
                    .collect(),
            ),
        );
    }

    let out: Option<serde_json::Value> = super::run_async(async move {
        match memory_mcp.call_tool("memory_capture", args).await {
            Ok(v) => Some(v),
            Err(e) => {
                warn!(error = %e, "memory_turn_capture: memory_capture port error");
                None
            }
        }
    });

    match out {
        Some(v) => {
            let status = v
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown");
            debug!(subject = %c.subject, status, "memory_turn_capture: capture result");
            status == "ok"
        }
        None => false,
    }
}

/// Stop-hook entry point. Always returns `allow()` — never blocks the turn.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    // No LLM configured → nothing to extract with. Silent no-op.
    let llm = match ctx.llm {
        Some(l) => l,
        None => return HookOutput::allow(),
    };

    let turn = match build_turn_text(input) {
        Some(t) => t,
        None => return HookOutput::allow(),
    };

    let prompt = build_extraction_prompt(&turn);
    let req = LlmRequest {
        model: LlmModel::Opus, // Opus 4.7 via OpenRouter — standardized memory LLM path
        prompt,
        max_tokens: 1024,
    };

    // Extract (timeout-guarded). On any failure, degrade to no capture.
    let raw: String = super::run_async(async move {
        match llm.complete(req).await {
            Ok(text) => text,
            Err(e) => {
                warn!(error = %e, "memory_turn_capture: extractor LLM call failed");
                String::new()
            }
        }
    });

    if raw.trim().is_empty() {
        return HookOutput::allow();
    }

    let mut candidates = parse_candidates(&raw);
    if candidates.is_empty() {
        return HookOutput::allow();
    }
    candidates.truncate(MAX_CANDIDATES);

    let project = project_label(input.cwd.as_deref().unwrap_or("."));
    let mut accepted = 0usize;
    for c in &candidates {
        // Skip obviously empty candidates.
        if c.subject.trim().is_empty() || c.value.trim().is_empty() {
            continue;
        }
        if capture_candidate(ctx.memory_mcp, c, &project) {
            accepted += 1;
        }
    }

    if accepted > 0 {
        debug!(
            accepted,
            proposed = candidates.len(),
            project = %project,
            "memory_turn_capture: atoms captured from turn"
        );
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_array() {
        let raw = r#"[{"subject":"gary","predicate":"prefers","value":"autopilot mode"}]"#;
        let c = parse_candidates(raw);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].subject, "gary");
        assert_eq!(c[0].predicate, "prefers");
    }

    #[test]
    fn parse_fenced_array() {
        let raw = "```json\n[{\"subject\":\"x\",\"predicate\":\"is\",\"value\":\"y\"}]\n```";
        let c = parse_candidates(raw);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].value, "y");
    }

    #[test]
    fn parse_with_leading_prose() {
        let raw = "Here are the facts:\n[{\"subject\":\"a\",\"predicate\":\"b\",\"value\":\"c\"}]";
        let c = parse_candidates(raw);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn parse_empty_array() {
        assert!(parse_candidates("[]").is_empty());
    }

    #[test]
    fn parse_garbage_is_empty() {
        assert!(parse_candidates("no json here").is_empty());
        assert!(parse_candidates("").is_empty());
    }

    #[test]
    fn parse_with_optional_fields() {
        let raw = r#"[{"subject":"s","predicate":"p","value":"v","qualifier":"when X","tags":["t1","t2"]}]"#;
        let c = parse_candidates(raw);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].qualifier.as_deref(), Some("when X"));
        assert_eq!(c[0].tags.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn turn_text_skips_trivial() {
        let input = HookInput {
            prompt: Some("ok".into()),
            last_assistant_message: Some("done".into()),
            ..Default::default()
        };
        assert!(build_turn_text(&input).is_none());
    }

    #[test]
    fn turn_text_builds_substantial() {
        let long = "x".repeat(300);
        let input = HookInput {
            prompt: Some(long.clone()),
            last_assistant_message: Some("reply".into()),
            ..Default::default()
        };
        let t = build_turn_text(&input).expect("should build");
        assert!(t.contains("USER:"));
        assert!(t.contains("ASSISTANT:"));
    }

    #[test]
    fn project_label_from_cwd() {
        assert_eq!(project_label("/c/Users/x/Documents/GitHub/memory"), "memory");
        assert_eq!(project_label(""), "global");
    }
}
