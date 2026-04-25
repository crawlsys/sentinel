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

use chrono::Utc;
use sentinel_domain::events::{HookInput, HookOutput};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use super::{FileSystemPort, HookContext};

// ---------------------------------------------------------------------------
// State file types
// ---------------------------------------------------------------------------

/// One injected memory entry from the state file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct InjectedMemory {
    id: String,
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
    fs.home_dir().map(|h| h.join(".claude").join("sentinel").join("state"))
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

/// Check if any injected memory name appears in the assistant response text.
fn detect_used_memories<'a>(
    memories: &'a [InjectedMemory],
    response: &str,
) -> Vec<&'a InjectedMemory> {
    let lower_response = response.to_ascii_lowercase();
    memories
        .iter()
        .filter(|m| {
            // Only match if the name is non-trivial (>3 chars) to avoid false positives
            let name_lower = m.name.to_ascii_lowercase();
            name_lower.len() > 3 && lower_response.contains(&name_lower)
        })
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
    injected: &[InjectedMemory],
    used: &[&InjectedMemory],
    correction_detected: bool,
) {
    let used_ids: std::collections::HashSet<&str> =
        used.iter().map(|m| m.id.as_str()).collect();

    let mut outcomes: Vec<(String, &'static str)> = Vec::with_capacity(injected.len());
    for memory in injected {
        let label = if used_ids.contains(memory.id.as_str()) {
            "used"
        } else if correction_detected {
            "contradicted"
        } else {
            "ignored"
        };
        outcomes.push((memory.id.clone(), label));
    }

    // Fire-and-forget: outcomes are best-effort. A transient memory-mcp
    // failure must not block the Stop hook. Aggregate all calls under a
    // single tokio runtime to minimise subprocess overhead.
    //
    // `run_async` requires `T: Send + Default`. `()` satisfies both, and
    // the whole path is inherently fire-and-forget (errors from
    // individual calls are already logged at WARN inside the loop), so
    // we unwrap to () here.
    crate::hooks::run_async(async move {
        for (event_id, outcome) in outcomes {
            if let Err(e) = call_memory_record_outcome(&event_id, outcome).await {
                warn!(
                    event_id = %event_id,
                    outcome = %outcome,
                    error = %e,
                    "memory_record_outcome call failed"
                );
            }
        }
    });
}

/// Spawn `mcp-router --single memory-mcp`, perform the MCP handshake,
/// and call `memory_record_outcome(event_id, outcome)`. Mirror of
/// `call_memory_mcp_search` in memory_inject; keep the two in lockstep
/// with `sentinel-infrastructure::memory_mcp_client` as the source of
/// truth for JSON-RPC framing.
async fn call_memory_record_outcome(
    event_id: &str,
    outcome: &str,
) -> anyhow::Result<()> {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::BufReader;
    use tokio::process::Command;
    use tokio::time::timeout as tokio_timeout;

    const PROTOCOL_VERSION: &str = "2024-11-05";
    let timeout_secs: u64 = std::env::var("MEMORY_MCP_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let cmd_str = std::env::var("MEMORY_MCP_CMD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "mcp-router --single memory-mcp".to_string());
    let argv: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    if argv.is_empty() {
        return Err(anyhow::anyhow!("MEMORY_MCP_CMD is empty"));
    }

    let event_id = event_id.to_string();
    let outcome = outcome.to_string();

    let call = async move {
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("child stdin missing"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("child stdout missing"))?;
        let mut reader = BufReader::new(stdout);

        let init_req = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "sentinel-memory-feedback", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        write_line(&mut stdin, &init_req).await?;
        let _ = read_json_line(&mut reader).await?;

        let initialized = serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized", "params": {}
        });
        write_line(&mut stdin, &initialized).await?;

        let call_req = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {
                "name": "memory_record_outcome",
                "arguments": {
                    "event_id": event_id,
                    "outcome": outcome,
                }
            }
        });
        write_line(&mut stdin, &call_req).await?;
        let resp = read_json_line(&mut reader).await?;

        drop(stdin);
        let _ = child.wait().await;

        if let Some(err) = resp.get("error") {
            return Err(anyhow::anyhow!("memory-mcp error: {err}"));
        }
        Ok::<_, anyhow::Error>(())
    };

    tokio_timeout(Duration::from_secs(timeout_secs), call)
        .await
        .map_err(|_| anyhow::anyhow!("memory-mcp call timed out"))?
}

async fn write_line<T: serde::Serialize>(
    stdin: &mut tokio::process::ChildStdin,
    value: &T,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    stdin.write_all(&line).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_json_line(
    reader: &mut tokio::io::BufReader<tokio::process::ChildStdout>,
) -> anyhow::Result<serde_json::Value> {
    use tokio::io::AsyncBufReadExt;
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            return Err(anyhow::anyhow!("memory-mcp stdout closed before response"));
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }
        return Ok(serde_json::from_str(trimmed)?);
    }
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

    let state: InjectedState = match serde_json::from_str(&state_content) {
        Ok(s) => s,
        Err(_) => {
            debug!("Invalid injected-memories state file — skipping");
            return HookOutput::allow();
        }
    };

    if state.memories.is_empty() {
        return HookOutput::allow();
    }

    // 2. Classify + record. record_outcomes_unified is fire-and-forget;
    //    a failing memory-mcp call must not block the Stop hook.
    let response = input.last_assistant_message.as_deref().unwrap_or("");
    let used = detect_used_memories(&state.memories, response);
    let correction = state.user_prompt.as_deref().and_then(detect_correction);
    record_outcomes_unified(&state.memories, &used, correction.is_some());

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
                name: "Firefly Pro CRM".to_string(),
                score: 0.85,
            },
            InjectedMemory {
                id: "id2".to_string(),
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
            name: "Firefly Pro CRM".to_string(),
            score: 0.85,
        }];

        let response = "The deployment process uses Railway for hosting.";
        let used = detect_used_memories(&memories, response);
        assert!(used.is_empty());
    }

    #[test]
    fn test_detect_used_memories_short_name_skipped() {
        // Names <= 3 chars should be skipped to avoid false positives
        let memories = vec![InjectedMemory {
            id: "id1".to_string(),
            name: "api".to_string(),
            score: 0.80,
        }];

        let response = "The API endpoint returns JSON";
        let used = detect_used_memories(&memories, response);
        assert!(used.is_empty());
    }

    #[test]
    fn test_detect_used_memories_case_insensitive() {
        let memories = vec![InjectedMemory {
            id: "id1".to_string(),
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
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_process_no_assistant_message() {
        let input = HookInput {
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
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
