//! Memory Feedback Hook — track which injected memories were useful vs wrong
//!
//! Fires on Stop. Reads the state file written by `memory_inject` to find
//! which memories were injected this turn, then checks the assistant response
//! and the user's recent prompt for signals:
//!
//! - **Usage signal**: Memory name appears in assistant response -> boost `access_count`
//! - **Correction signal**: User prompt contains "no", "that's wrong", "actually",
//!   etc. after memories were injected -> flag for manual review
//!
//! Boost: increments `access_count` and updates `accessed_at` via Qdrant `set_payload`.
//! Corrections: appended to `~/.claude/sentinel/state/memory-corrections.jsonl`.

use sentinel_domain::events::{HookInput, HookOutput};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use super::{FileSystemPort, HookContext, MemoryMcpPort};

// ---------------------------------------------------------------------------
// State file types
// ---------------------------------------------------------------------------

/// One injected memory entry from the state file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct InjectedMemory {
    id: String,
    /// Retrieval-event id captured by `memory_inject` from `memory_search`. This —
    /// not `id` (the atom id) — is what `memory_record_outcome` expects, since
    /// the retrieval log is keyed by event id.
    #[serde(default)]
    event_id: Option<String>,
    name: String,
    score: f64,
}

/// The full state file written by `memory_inject` on each `UserPromptSubmit`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct InjectedState {
    memories: Vec<InjectedMemory>,
    timestamp: String,
    #[serde(default)]
    user_prompt: Option<String>,
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn state_dir(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    fs.home_dir()
        .map(|h| h.join(".claude").join("sentinel").join("state"))
}

fn injected_state_path(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    state_dir(fs).map(|d| d.join("last-injected-memories.json"))
}

// ---------------------------------------------------------------------------
// Correction detection
// ---------------------------------------------------------------------------

/// Phrases that signal the user is correcting a previous assistant response.
const CORRECTION_SIGNALS: &[&str] = &[
    "no,",
    "no.",
    "that's wrong",
    "thats wrong",
    "not anymore",
    "that's outdated",
    "thats outdated",
    "actually,",
    "actually ",
    "incorrect",
    "that's not right",
    "thats not right",
    "wrong",
    "that was wrong",
    "not true",
    "that's old",
    "thats old",
    "out of date",
    "no longer",
    "not correct",
];

/// Check if the user prompt contains any correction signal.
/// Returns the first matching signal, or None.
fn detect_correction(prompt: &str) -> Option<&'static str> {
    let lower = prompt.to_ascii_lowercase();
    CORRECTION_SIGNALS
        .iter()
        .find(|&&signal| lower.contains(signal))
        .copied()
}

// ---------------------------------------------------------------------------
// Memory usage detection
// ---------------------------------------------------------------------------

/// Tokens too common to be evidence an atom was *used* — matching one of these
/// in the response says nothing about whether the recalled fact was drawn on.
/// Deliberately small: over-filtering would discard genuine domain terms.
const USAGE_STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "are", "was", "were", "has",
    "have", "had", "its", "via", "per", "use", "uses", "used", "default",
    "value", "count", "runs", "run", "set", "via", "into", "from", "when",
    "not", "all", "any", "via",
];

/// Distinctive lowercase tokens from an atom's rendered name (`subject/pred=value`).
/// Splits on non-alphanumeric, drops short tokens and stopwords, but KEEPS
/// pure-digit tokens (e.g. a port `3011` is highly distinctive evidence).
fn distinctive_tokens(name: &str) -> Vec<String> {
    let lower = name.to_ascii_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for tok in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        if tok.is_empty() {
            continue;
        }
        let is_digits = tok.chars().all(|c| c.is_ascii_digit());
        // Keep multi-char words and any all-digit token; drop short words + stopwords.
        let keep = (is_digits && tok.len() >= 2) || (tok.len() > 3 && !USAGE_STOPWORDS.contains(&tok));
        if keep && seen.insert(tok.to_string()) {
            out.push(tok.to_string());
        }
    }
    out
}

/// Decide whether an injected atom was *used* by the assistant's response.
///
/// The old check required the FULL rendered name (`subject/pred=value`, often
/// 60+ chars) to appear verbatim — which essentially never happened, so the
/// feedback loop only ever produced negative ("ignored") signal and learn could
/// never reinforce a helpful atom. This counts distinctive-token overlap:
///   - ≤2 distinctive tokens → ALL must appear (a tiny atom is all-or-nothing);
///   - otherwise → at least half the distinctive tokens AND at least 2 must
///     appear (the `≥2` guard stops a single common word from false-triggering).
fn atom_used_in(name: &str, lower_response: &str) -> bool {
    let tokens = distinctive_tokens(name);
    if tokens.is_empty() {
        return false;
    }
    let matched = tokens
        .iter()
        .filter(|t| lower_response.contains(t.as_str()))
        .count();
    if tokens.len() <= 2 {
        matched == tokens.len()
    } else {
        matched >= 2 && matched * 2 >= tokens.len()
    }
}

/// Check which injected memories were drawn on by the assistant response,
/// using distinctive-token overlap (see `atom_used_in`).
fn detect_used_memories<'a>(
    memories: &'a [InjectedMemory],
    response: &str,
) -> Vec<&'a InjectedMemory> {
    let lower_response = response.to_ascii_lowercase();
    memories
        .iter()
        .filter(|m| atom_used_in(&m.name, &lower_response))
        .collect()
}

// ---------------------------------------------------------------------------
// Qdrant boost — increment access_count, update accessed_at
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Correction logging
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Loop 4 outcome recording via memory-mcp
// ---------------------------------------------------------------------------

/// Classify each injected memory into a Loop 4 outcome label and send
/// them to the Memory engine in a single batch of MCP calls.
///
/// Classification heuristic:
///   - In `used` set → "used"
///   - Correction detected globally, memory NOT in `used` → "contradicted"
///     (the user is correcting, and this memory didn't make it into the
///     assistant's response, so it was at best unhelpful and at worst wrong)
///   - Otherwise → "ignored"
///
/// We never emit "unknown" from this path — the classifier is precision-
/// first but not recall-perfect, and an "ignored" signal is a weaker
/// negative than "contradicted" in the EMA (see
/// `OutcomeSignal::WeakNegative` vs `StrongNegative`).
fn record_outcomes_unified(
    fs: &dyn FileSystemPort,
    memory_mcp: &dyn MemoryMcpPort,
    session_id: Option<&str>,
    injected: &[InjectedMemory],
    used: &[&InjectedMemory],
    correction_detected: bool,
    correction_signal: Option<&str>,
) {
    let used_ids: std::collections::HashSet<&str> = used.iter().map(|m| m.id.as_str()).collect();

    let mut outcomes: Vec<(String, &'static str)> = Vec::with_capacity(injected.len());
    for memory in injected {
        // The outcome must attach to the retrieval EVENT, not the atom. If
        // memory_inject didn't capture an event_id (e.g. an older state file),
        // there's no event to record against — skip rather than send an atom
        // id the retrieval log can't resolve.
        let Some(event_id) = memory.event_id.clone() else {
            continue;
        };
        let label = if used_ids.contains(memory.id.as_str()) {
            "used"
        } else if correction_detected {
            "contradicted"
        } else {
            "ignored"
        };
        outcomes.push((event_id, label));
    }

    // Telemetry: emit a `feedback` event with the per-atom classification so
    // the end-to-end loop trace shows used/ignored/contradicted ratios. Emitted
    // before the async move consumes `outcomes`. See crate::memory_telemetry.
    crate::memory_telemetry::record_feedback(
        fs,
        session_id,
        injected.len(),
        &outcomes,
        correction_signal,
    );

    // Fire-and-forget, but NOT under the default 3s budget: this loops one
    // `memory_record_outcome` call per injected atom (up to ~8), and each one
    // cold-spawns memory-mcp (~2s). Under 3s only the first call or two would
    // land before the timeout killed the rest — which is exactly why the
    // feedback loop never closed and Loop 4 saw `used=0`. Give it a generous
    // budget (Stop is async; this never blocks the user's turn). Errors per
    // call are logged at WARN inside the loop.
    crate::hooks::run_async_timeout(async move {
        for (event_id, outcome) in outcomes {
            let mut args = serde_json::Map::new();
            args.insert(
                "event_id".into(),
                serde_json::Value::String(event_id.clone()),
            );
            args.insert(
                "outcome".into(),
                serde_json::Value::String(outcome.to_string()),
            );
            if let Err(e) = memory_mcp.call_tool("memory_record_outcome", args).await {
                warn!(
                    event_id = %event_id,
                    outcome = %outcome,
                    error = %e,
                    "memory_record_outcome call failed"
                );
            }
        }
    }, std::time::Duration::from_secs(30));
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Process Stop — classify each injected memory into a Loop 4 outcome and
/// record via memory-mcp. Unconditional — there is no "legacy" path.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    // 1. Read the state file written by memory_inject on the matching
    //    UserPromptSubmit turn.
    let state_path = match injected_state_path(ctx.fs) {
        Some(p) if ctx.fs.exists(&p) => p,
        _ => {
            debug!("No injected-memories state file — skipping feedback");
            return HookOutput::allow();
        }
    };

    let state_content = match ctx.fs.read_to_string(Path::new(&state_path)) {
        Ok(c) => c,
        Err(_) => return HookOutput::allow(),
    };

    let state: InjectedState = if let Ok(s) = serde_json::from_str(&state_content) { s } else {
        debug!("Invalid injected-memories state file — skipping");
        return HookOutput::allow();
    };

    if state.memories.is_empty() {
        return HookOutput::allow();
    }

    // 2. Classify + record. record_outcomes_unified is fire-and-forget;
    //    a failing memory-mcp call must not block the Stop hook.
    let response = input.last_assistant_message.as_deref().unwrap_or("");
    let used = detect_used_memories(&state.memories, response);
    let correction = state.user_prompt.as_deref().and_then(detect_correction);
    record_outcomes_unified(
        ctx.fs,
        ctx.memory_mcp,
        input.session_id.as_deref(),
        &state.memories,
        &used,
        correction.is_some(),
        correction,
    );

    // Never block
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_correction_positive() {
        assert!(detect_correction("No, that's not what I meant").is_some());
        assert!(detect_correction("That's wrong, the API uses POST").is_some());
        assert!(detect_correction("actually, it should be v2").is_some());
        assert!(detect_correction("that's outdated info").is_some());
        assert!(detect_correction("Not anymore, we migrated").is_some());
        assert!(detect_correction("incorrect — we use Rust now").is_some());
        assert!(detect_correction("That's not right").is_some());
        assert!(detect_correction("no longer relevant").is_some());
    }

    #[test]
    fn test_detect_correction_negative() {
        assert!(detect_correction("Tell me about the deploy process").is_none());
        assert!(detect_correction("How do I fix this error?").is_none());
        assert!(detect_correction("What's the architecture?").is_none());
        assert!(detect_correction("List all the hooks").is_none());
        // "know" contains "no" but we match "no," and "no." specifically
        assert!(detect_correction("I know the answer").is_none());
    }

    #[test]
    fn test_detect_used_memories_found() {
        let memories = vec![
            InjectedMemory {
                id: "id1".to_string(),
                event_id: None,
                name: "Firefly Pro CRM".to_string(),
                score: 0.85,
            },
            InjectedMemory {
                id: "id2".to_string(),
                event_id: None,
                name: "Sentinel Hook Engine".to_string(),
                score: 0.75,
            },
        ];

        let response = "The Firefly Pro CRM uses Next.js 15 with Material UI.";
        let used = detect_used_memories(&memories, response);
        assert_eq!(used.len(), 1);
        assert_eq!(used[0].name, "Firefly Pro CRM");
    }

    #[test]
    fn test_detect_used_memories_none() {
        let memories = vec![InjectedMemory {
            id: "id1".to_string(),
            event_id: None,
            name: "Firefly Pro CRM".to_string(),
            score: 0.85,
        }];

        let response = "The deployment process uses Railway for hosting.";
        let used = detect_used_memories(&memories, response);
        assert!(used.is_empty());
    }

    #[test]
    fn test_detect_used_memories_short_name_skipped() {
        // A 3-char token yields no distinctive tokens → never "used".
        let memories = vec![InjectedMemory {
            id: "id1".to_string(),
            event_id: None,
            name: "api".to_string(),
            score: 0.80,
        }];

        let response = "The API endpoint returns JSON";
        let used = detect_used_memories(&memories, response);
        assert!(used.is_empty());
    }

    #[test]
    fn test_detect_used_realistic_atom_name_via_token_overlap() {
        // The real shape: subject/predicate=value. The OLD verbatim-substring
        // check would never match this against a paraphrasing response; the
        // token-overlap check must.
        let memories = vec![InjectedMemory {
            id: "id1".to_string(),
            event_id: Some("e1".to_string()),
            name: "memory daemon/runs on port=3011".to_string(),
            score: 0.5,
        }];
        let response = "The memory daemon binds port 3011 for its HTTP endpoints.";
        let used = detect_used_memories(&memories, response);
        assert_eq!(used.len(), 1, "daemon + port + 3011 overlap → used");
    }

    #[test]
    fn test_detect_used_number_is_distinctive() {
        // A port number alone is strong evidence.
        assert!(atom_used_in(
            "memory daemon/runs on port=3011",
            &"we exposed it on 3011 and the daemon answered".to_ascii_lowercase()
        ));
    }

    #[test]
    fn test_detect_used_single_common_word_does_not_false_positive() {
        // Response mentions "memory" (common in these sessions) but nothing else
        // from the atom — must NOT register as used.
        let memories = vec![InjectedMemory {
            id: "id1".to_string(),
            event_id: None,
            name: "memory daemon/has registered cron loops=consolidate, ingest-telemetry, learn, loop6".to_string(),
            score: 0.4,
        }];
        let response = "I improved my working memory of the conversation.";
        let used = detect_used_memories(&memories, response);
        assert!(used.is_empty(), "one common token must not trigger 'used'");
    }

    #[test]
    fn test_distinctive_tokens_drops_stopwords_keeps_digits() {
        let toks = distinctive_tokens("memory daemon/runs on port=3011");
        assert!(toks.contains(&"memory".to_string()));
        assert!(toks.contains(&"daemon".to_string()));
        assert!(toks.contains(&"3011".to_string()));
        // "runs", "on", "port" are stopword/short → dropped.
        assert!(!toks.contains(&"runs".to_string()));
        assert!(!toks.contains(&"on".to_string()));
    }

    #[test]
    fn test_detect_used_memories_case_insensitive() {
        let memories = vec![InjectedMemory {
            id: "id1".to_string(),
            event_id: None,
            name: "Qdrant Vector Database".to_string(),
            score: 0.90,
        }];

        let response = "We use qdrant vector database for semantic search.";
        let used = detect_used_memories(&memories, response);
        assert_eq!(used.len(), 1);
    }

    #[test]
    fn test_process_no_state_file() {
        let input = HookInput {
            last_assistant_message: Some("response text".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_process_no_assistant_message() {
        let input = HookInput {
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_injected_state_deserializes() {
        let json = r#"{
            "memories": [
                {"id": "abc", "name": "Test", "score": 0.85}
            ],
            "timestamp": "2026-04-04T12:00:00Z",
            "user_prompt": "test prompt"
        }"#;
        let state: InjectedState = serde_json::from_str(json).unwrap();
        assert_eq!(state.memories.len(), 1);
        assert_eq!(state.memories[0].id, "abc");
        assert_eq!(state.user_prompt.as_deref(), Some("test prompt"));
    }

    #[test]
    fn test_injected_state_no_prompt() {
        let json = r#"{
            "memories": [],
            "timestamp": "2026-04-04T12:00:00Z"
        }"#;
        let state: InjectedState = serde_json::from_str(json).unwrap();
        assert!(state.user_prompt.is_none());
    }
}
