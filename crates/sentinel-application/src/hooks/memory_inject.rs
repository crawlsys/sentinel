//! Memory Inject Hook — surface relevant atoms to the model on every turn.
//!
//! Fires on UserPromptSubmit. Calls memory-mcp's `memory_search` tool, renders
//! the hits into a compact Markdown block, and injects it as additionalContext.
//! Also writes `last-injected-memories.json` so the `memory_feedback` hook
//! (Stop) can classify each atom's outcome (used / contradicted / ignored)
//! and feed it back into the Memory engine's RelevanceUpdater.
//!
//! No direct Qdrant traffic — every hit routes through the Memory engine's
//! Retriever via the MCP subprocess. That pipeline already does hybrid
//! search + project-bleed + rerank + utility/freshness, so sentinel's
//! client-side scoring helpers (decay_lambda, temporal_score, shingle
//! dedup, precompute cache) are all gone.

use chrono::Utc;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use super::{run_async, FileSystemPort};

// ---------------------------------------------------------------------------
// Injected state — written so memory_feedback can classify outcomes on Stop
// ---------------------------------------------------------------------------

/// Single injected atom, surfaced into the state file.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct InjectedMemoryEntry {
    id: String,
    name: String,
    score: f64,
}

/// Shape of `~/.claude/sentinel/state/last-injected-memories.json` read by
/// memory_feedback on Stop.
#[derive(serde::Serialize, serde::Deserialize)]
struct InjectedState {
    memories: Vec<InjectedMemoryEntry>,
    timestamp: String,
    user_prompt: Option<String>,
}

fn state_dir(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    fs.home_dir().map(|h| {
        h.join(".claude")
            .join("sentinel")
            .join("state")
    })
}

fn write_injected_state(
    fs: &dyn FileSystemPort,
    hits: &[UnifiedHit],
    user_prompt: Option<&str>,
) {
    let Some(dir) = state_dir(fs) else {
        return;
    };
    let _ = fs.create_dir_all(&dir);

    let state = InjectedState {
        memories: hits
            .iter()
            .map(|h| InjectedMemoryEntry {
                id: h.atom_id.clone(),
                name: format!("{}/{}={}", h.subject, h.predicate, h.value),
                score: h.final_score,
            })
            .collect(),
        timestamp: Utc::now().to_rfc3339(),
        user_prompt: user_prompt.map(String::from),
    };

    let path = dir.join("last-injected-memories.json");
    if let Ok(json) = serde_json::to_string_pretty(&state) {
        let _ = fs.write(&path, json.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Unified memory_search hit shape (deserialised from memory-mcp payload)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Debug, Clone)]
struct UnifiedHit {
    atom_id: String,
    subject: String,
    predicate: String,
    value: String,
    project: String,
    #[serde(default)]
    final_score: f64,
}

// ---------------------------------------------------------------------------
// Project derivation
// ---------------------------------------------------------------------------

/// Derive a project name from cwd that satisfies memory-mcp's
/// `validate_project` regex `[A-Za-z0-9_-]{1,128}`. Falls back to
/// `"global"` when nothing usable can be extracted.
fn project_from_cwd(cwd: &str) -> String {
    let normalized = cwd.replace('\\', "/");
    let basename = normalized
        .trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("");

    let mapped: String = basename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Collapse duplicate dashes and trim leading/trailing
    let mut cleaned = String::with_capacity(mapped.len());
    let mut last_dash = false;
    for c in mapped.chars() {
        if c == '-' {
            if !last_dash {
                cleaned.push(c);
            }
            last_dash = true;
        } else {
            cleaned.push(c);
            last_dash = false;
        }
    }
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        return "global".to_string();
    }
    if cleaned.len() > 128 {
        cleaned.chars().take(128).collect()
    } else {
        cleaned
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn compact_summary(content: &str, max_chars: usize) -> String {
    let collapsed: String = content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }

    // Truncate on a char boundary, prefer a trailing word boundary within
    // the last ~15% of the window.
    let truncate_at = collapsed
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(collapsed.len());

    let window = &collapsed[..truncate_at];
    let cutoff = window
        .rfind(' ')
        .filter(|&i| i + 1 >= (max_chars * 85 / 100))
        .unwrap_or(window.len());

    format!("{}…", &window[..cutoff])
}

fn render_context(hits: &[UnifiedHit]) -> String {
    let mut out = format!("[Memory] {} relevant atom(s):\n", hits.len());
    for h in hits {
        let short = compact_summary(&h.value, 150);
        out.push_str(&format!(
            "\n- [{:.2}] **{}/{}={}** ({}):\n  {}\n",
            h.final_score, h.subject, h.predicate, h.value, h.project, short
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Core search path
// ---------------------------------------------------------------------------

/// Call memory-mcp's `memory_search` tool and return hits + rendered context.
/// Returns `None` when memory-mcp returns zero hits or the subprocess fails.
fn search_memory_engine(prompt: &str, cwd: &str) -> Option<(Vec<UnifiedHit>, String)> {
    let project = project_from_cwd(cwd);

    let payload: serde_json::Value = match run_async(async {
        match call_memory_mcp_search(prompt, &project).await {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(
                    project = %project,
                    error = %e,
                    "memory-mcp search failed — skipping injection this turn"
                );
                None
            }
        }
    }) {
        Some(p) => p,
        None => return None,
    };

    let hits: Vec<UnifiedHit> = payload
        .get("hits")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    if hits.is_empty() {
        return None;
    }

    let rendered = render_context(&hits);
    Some((hits, rendered))
}

/// Spawn `mcp-router --single memory-mcp`, run the initialise handshake,
/// call `memory_search`, and return the parsed tool payload.
async fn call_memory_mcp_search(
    query: &str,
    project: &str,
) -> anyhow::Result<serde_json::Value> {
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

    let query = query.to_string();
    let project = project.to_string();

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

        let init = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "sentinel-memory-inject", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        write_line(&mut stdin, &init).await?;
        let _ = read_json_line(&mut reader).await?;

        let initialized = serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized", "params": {}
        });
        write_line(&mut stdin, &initialized).await?;

        let search_req = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {
                "name": "memory_search",
                "arguments": {
                    "query": query,
                    "project": project,
                    "top_k": 8,
                }
            }
        });
        write_line(&mut stdin, &search_req).await?;
        let resp = read_json_line(&mut reader).await?;

        drop(stdin);
        let _ = child.wait().await;

        if let Some(err) = resp.get("error") {
            return Err(anyhow::anyhow!("memory-mcp error: {err}"));
        }

        // memory-mcp returns {"content": [{"type":"text","text":"<json>"}], "structuredContent": {...}}
        // Prefer structuredContent; fall back to parsing the text block.
        if let Some(sc) = resp.pointer("/result/structuredContent") {
            return Ok(sc.clone());
        }
        if let Some(text) = resp.pointer("/result/content/0/text").and_then(|v| v.as_str()) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
                return Ok(parsed);
            }
        }
        Err(anyhow::anyhow!("memory-mcp returned no parsable payload"))
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
// Hook entry points
// ---------------------------------------------------------------------------

/// Process UserPromptSubmit — query memory-mcp and inject matching atoms.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    // Skip empty / too-short prompts
    let prompt = match input.prompt.as_deref() {
        Some(p) if p.len() > 10 => p,
        _ => return HookOutput::allow(),
    };

    // Slash commands are handled by the skill router — nothing to inject.
    if prompt.trim().starts_with('/') {
        return HookOutput::allow();
    }

    let cwd = input.cwd.as_deref().unwrap_or(".");
    match search_memory_engine(prompt, cwd) {
        Some((hits, rendered)) => {
            write_injected_state(ctx.fs, &hits, Some(prompt));
            debug!(atoms = hits.len(), "Injecting atoms via memory-mcp");
            HookOutput::inject_context(HookEvent::UserPromptSubmit, &rendered)
        }
        None => {
            debug!("memory-mcp returned no hits — not injecting");
            HookOutput::allow()
        }
    }
}

/// Process Stop — no-op.
///
/// The legacy implementation precomputed Qdrant search results into a
/// sidecar JSON file so the next UserPromptSubmit could read them without
/// a live search. Under memory-mcp, the live search is fast enough (single
/// subprocess, cold start ~2.7s) that the precompute cache is both
/// redundant and a maintenance burden. Kept as a no-op stub so the hook
/// dispatcher wiring can stay unchanged; delete the dispatch branch in a
/// follow-up pass.
pub fn process_stop(_input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compact_summary_short_returns_unchanged() {
        let result = compact_summary("hello world", 150);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_compact_summary_truncates_long() {
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda";
        let result = compact_summary(long, 30);
        assert!(result.ends_with('…'));
        assert!(result.chars().count() <= 31);
    }

    #[test]
    fn test_compact_summary_collapses_whitespace() {
        let messy = "hello   world\n\t  foo";
        let result = compact_summary(messy, 150);
        assert_eq!(result, "hello world foo");
    }

    #[test]
    fn test_compact_summary_handles_multibyte() {
        // Must not panic on non-ASCII boundaries.
        let s = "héllo wörld ñoño";
        for max in [1, 3, 5, 7, 10, 100] {
            let _ = compact_summary(s, max);
        }
    }

    #[test]
    fn test_project_from_cwd_posix_and_windows() {
        assert_eq!(project_from_cwd("/Users/gary/code/firefly-pro"), "firefly-pro");
        assert_eq!(
            project_from_cwd(r"C:\Users\gary\code\firefly-pro"),
            "firefly-pro"
        );
    }

    #[test]
    fn test_project_from_cwd_strips_invalid_chars() {
        assert_eq!(project_from_cwd("/a/b/my.repo"), "my-repo");
        assert_eq!(project_from_cwd("/a/b/my repo"), "my-repo");
    }

    #[test]
    fn test_project_from_cwd_falls_back_to_global() {
        assert_eq!(project_from_cwd(""), "global");
        assert_eq!(project_from_cwd("/"), "global");
    }

    #[test]
    fn test_render_context_empty_hits() {
        let out = render_context(&[]);
        assert!(out.contains("0 relevant atom(s)"));
    }

    #[test]
    fn test_render_context_single_hit() {
        let hits = vec![UnifiedHit {
            atom_id: "abc".to_string(),
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            value: "rust".to_string(),
            project: "test".to_string(),
            final_score: 0.87,
        }];
        let out = render_context(&hits);
        assert!(out.contains("0.87"));
        assert!(out.contains("user/likes=rust"));
    }

    #[test]
    fn test_process_skips_short_prompt() {
        let input = HookInput {
            prompt: Some("short".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_process_skips_slash_command() {
        let input = HookInput {
            prompt: Some("/commit something long enough".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_process_stop_is_noop() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_injected_state_roundtrip() {
        let state = InjectedState {
            memories: vec![InjectedMemoryEntry {
                id: "atom-1".to_string(),
                name: "user/likes=rust".to_string(),
                score: 0.9,
            }],
            timestamp: "2026-04-25T00:00:00Z".to_string(),
            user_prompt: Some("why rust".to_string()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: InjectedState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.memories.len(), 1);
        assert_eq!(parsed.memories[0].id, "atom-1");
        assert_eq!(parsed.user_prompt.as_deref(), Some("why rust"));
    }
}
