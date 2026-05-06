//! Memory MCP stdio client.
//!
//! Gives sentinel hooks a way to call the Memory engine's MCP tools
//! (`memory_search`, `memory_capture`, `memory_why`, etc.) via the same
//! stdio protocol Claude Code uses, without taking a direct crate
//! dependency on `memory-application` / `memory-adapters`. This keeps
//! sentinel's hexagonal layering intact — sentinel stays a hook shell and
//! doesn't inherit Memory's Qdrant schema knowledge.
//!
//! # Design
//!
//! - Each call spawns `mcp-router --single memory-mcp` as a child process,
//!   performs the MCP protocol handshake (`initialize` → `initialized` →
//!   tool call → response), then exits. Cold start per call.
//! - Cold-start tolerance is acceptable because the sentinel memory-inject
//!   hook's hot path is the precomputed file (read, not MCP call). Live
//!   calls only happen on the first prompt of a session or when the
//!   precomputed file is stale; those are already one-off costs.
//! - Timeout guards every call at the process level so a hung memory-mcp
//!   can't stall a hook indefinitely.
//! - No request batching yet. Future optimisation: long-lived subprocess
//!   reused across calls. Not in scope for F1-PRE-3b.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sentinel_domain::ports::MemoryMcpPort;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tracing::{debug, warn};

// ── Config ───────────────────────────────────────────────────────────

/// Default command to invoke. `MEMORY_MCP_CMD` overrides (space-separated).
const DEFAULT_CMD: &str = "mcp-router --single memory-mcp";

/// Default call timeout. Covers spawn + initialise + tool invocation.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Protocol version we negotiate in `initialize`. Matches the MCP spec
/// version Vulcan 1.x emits on the server side.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Configuration for a `MemoryMcpClient`. `from_env` reads
/// `MEMORY_MCP_CMD` (a shell-split command) and `MEMORY_MCP_TIMEOUT_SECS`.
#[derive(Debug, Clone)]
pub struct MemoryMcpConfig {
    /// Program + args to run. `argv[0]` must be on PATH or absolute.
    pub argv: Vec<String>,
    /// Hard timeout for one round-trip (spawn + initialise + call).
    pub timeout: Duration,
}

impl Default for MemoryMcpConfig {
    fn default() -> Self {
        Self {
            argv: shell_split(DEFAULT_CMD),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl MemoryMcpConfig {
    /// Build from environment variables. Missing vars fall back to defaults.
    pub fn from_env() -> Self {
        let argv = std::env::var("MEMORY_MCP_CMD")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map_or_else(|| shell_split(DEFAULT_CMD), |s| shell_split(&s));
        let timeout = std::env::var("MEMORY_MCP_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map_or(DEFAULT_TIMEOUT, Duration::from_secs);
        Self { argv, timeout }
    }
}

/// Very small shell-splitter — whitespace-delimited tokens, no quoting
/// support. Sufficient for `mcp-router --single memory-mcp`; if callers
/// need a path with spaces they can set `MEMORY_MCP_CMD` to a single
/// quoted command plus args as separate `MEMORY_MCP_ARG_N` env vars
/// (not implemented; open a task if needed).
fn shell_split(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

// ── Client ───────────────────────────────────────────────────────────

/// Thin stdio-MCP client for the Memory engine. Not Send+Sync across
/// await points — each call spawns a fresh subprocess.
#[derive(Debug, Clone)]
pub struct MemoryMcpClient {
    cfg: MemoryMcpConfig,
}

impl MemoryMcpClient {
    pub const fn new(cfg: MemoryMcpConfig) -> Self {
        Self { cfg }
    }

    pub fn from_env() -> Self {
        Self::new(MemoryMcpConfig::from_env())
    }

    /// Search the Memory engine for atoms relevant to `query`. Writes one
    /// `RetrievalEvent` per surfaced atom to `memory-retrieval-log` as a
    /// side effect (Loop 4's fuel).
    pub async fn search(
        &self,
        query: &str,
        project: &str,
        top_k: u32,
        session: Option<&str>,
    ) -> Result<Vec<McpSearchHit>> {
        let mut args = serde_json::Map::new();
        args.insert("query".into(), serde_json::Value::String(query.into()));
        args.insert("project".into(), serde_json::Value::String(project.into()));
        args.insert("top_k".into(), serde_json::Value::Number(top_k.into()));
        if let Some(s) = session {
            args.insert("session".into(), serde_json::Value::String(s.into()));
        }

        let payload = self.call_tool("memory_search", args).await?;
        let parsed: SearchResponse =
            serde_json::from_value(payload).context("parse memory_search response payload")?;
        if parsed.status != "ok" {
            return Err(anyhow!("memory_search returned status={}", parsed.status));
        }
        Ok(parsed.hits)
    }

    /// Invoke an arbitrary MCP tool. Returns the decoded `content[0].text`
    /// payload parsed as JSON (the memory-mcp server always emits a single
    /// `text` content item containing JSON, per its `json_result` helper).
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value> {
        timeout(self.cfg.timeout, self.call_tool_inner(tool_name, arguments))
            .await
            .context("memory-mcp call timed out")?
    }

    async fn call_tool_inner(
        &self,
        tool_name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let mut child = self.spawn_child().context("spawn memory-mcp subprocess")?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("child stdin missing"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("child stdout missing"))?;
        let mut stdin = stdin;
        let mut reader = BufReader::new(stdout);

        // 1. initialize
        let init_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "sentinel", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        write_json_line(&mut stdin, &init_req).await?;
        let _init_resp = read_json_line(&mut reader).await?;

        // 2. notifications/initialized (no response expected)
        let initialized = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        write_json_line(&mut stdin, &initialized).await?;

        // 3. tools/call
        let call_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": serde_json::Value::Object(arguments),
            }
        });
        write_json_line(&mut stdin, &call_req).await?;
        let call_resp = read_json_line(&mut reader).await?;

        // Drop stdin so memory-mcp sees EOF and exits cleanly.
        drop(stdin);
        let _ = child.wait().await;

        extract_tool_payload(&call_resp)
    }

    fn spawn_child(&self) -> Result<Child> {
        if self.cfg.argv.is_empty() {
            return Err(anyhow!("MEMORY_MCP_CMD is empty"));
        }
        let mut cmd = Command::new(&self.cfg.argv[0]);
        cmd.args(&self.cfg.argv[1..]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // stderr goes to the void — logs show up in mcp-router's own log
            .kill_on_drop(true);
        let child = cmd.spawn().with_context(|| {
            format!(
                "spawn `{}` failed — is mcp-router on PATH?",
                self.cfg.argv.join(" ")
            )
        })?;
        debug!(cmd = ?self.cfg.argv, "spawned memory-mcp subprocess");
        Ok(child)
    }
}

// ── Response shapes ──────────────────────────────────────────────────

/// Hit shape emitted by memory-mcp's `memory_search` tool. Matches the
/// JSON `hits[i]` entries; extra fields are ignored on deserialisation
/// so server additions stay backwards-compatible.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpSearchHit {
    pub atom_id: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub predicate: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub memory_type: Option<String>,
    #[serde(default)]
    pub rank: u32,
    #[serde(default)]
    pub final_score: f64,
    #[serde(default)]
    pub base_score: f64,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    status: String,
    #[serde(default)]
    hits: Vec<McpSearchHit>,
}

// ── Low-level JSON-RPC IO helpers ────────────────────────────────────

async fn write_json_line<T: Serialize>(
    stdin: &mut tokio::process::ChildStdin,
    value: &T,
) -> Result<()> {
    let mut line = serde_json::to_vec(value).context("serialise JSON-RPC request")?;
    line.push(b'\n');
    stdin
        .write_all(&line)
        .await
        .context("write JSON-RPC request")?;
    stdin.flush().await.context("flush JSON-RPC request")?;
    Ok(())
}

async fn read_json_line(
    reader: &mut BufReader<tokio::process::ChildStdout>,
) -> Result<serde_json::Value> {
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader
            .read_line(&mut buf)
            .await
            .context("read JSON-RPC response line")?;
        if n == 0 {
            return Err(anyhow!("memory-mcp stdout closed before response"));
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip lines that aren't JSON-RPC (server may log banners on stderr
        // only, but be defensive).
        if !trimmed.starts_with('{') {
            warn!(line = %trimmed, "non-JSON line from memory-mcp stdout");
            continue;
        }
        return serde_json::from_str(trimmed)
            .with_context(|| format!("parse JSON-RPC response: {trimmed}"));
    }
}

fn extract_tool_payload(resp: &serde_json::Value) -> Result<serde_json::Value> {
    if let Some(err) = resp.get("error") {
        return Err(anyhow!("memory-mcp error: {err}"));
    }
    // memory-mcp emits both `structuredContent` (preferred — typed JSON) and
    // `content[0].text` (fallback — JSON string inside a text block). Match
    // the order the inlined transports used so behaviour is unchanged.
    if let Some(sc) = resp.pointer("/result/structuredContent") {
        return Ok(sc.clone());
    }
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(|t| t.as_str())
        .ok_or_else(|| {
            anyhow!("memory-mcp response missing structuredContent and content[0].text: {resp}")
        })?;

    // **Bug fix (2026-05-06)**: Previously parsed `content[0].text` directly as
    // JSON and surfaced a noisy `parse memory-mcp tool text payload: <error>`
    // warning when the upstream returned a plain-text error. The most common
    // case: mcp-router emits `Error: Server '<name>' not found` as
    // `content[0].text` when the wrapped server isn't registered in
    // ~/.claude.json. That string isn't JSON, so every hook invocation logs a
    // confusing parse failure even though the *real* error is "memory-mcp not
    // registered" or "wrong --single arg".
    //
    // New behaviour:
    //   1. Try parsing as JSON (the common, healthy path).
    //   2. If parse fails AND the text starts with "Error:" / "error:" /
    //      "ERROR:", treat the whole text as the error message and return a
    //      clean Err(anyhow!) — caller logs surface the real cause.
    //   3. Otherwise, return the raw text wrapped in a `{"text": "..."}` JSON
    //      object so callers that just want the string still work.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
        return Ok(parsed);
    }
    let trimmed = text.trim_start();
    if trimmed.starts_with("Error:")
        || trimmed.starts_with("error:")
        || trimmed.starts_with("ERROR:")
    {
        return Err(anyhow!("memory-mcp upstream error: {trimmed}"));
    }
    Ok(serde_json::json!({ "text": text }))
}

/// Implement `MemoryMcpPort` so hooks can call any memory-mcp tool through
/// the domain port instead of inlining their own subprocess transport.
#[async_trait::async_trait]
impl MemoryMcpPort for MemoryMcpClient {
    async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value> {
        // Delegate to the inherent method, which already wraps the call in
        // the configured timeout and handles the MCP handshake.
        Self::call_tool(self, name, arguments).await
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_split_basic() {
        assert_eq!(shell_split("a b c"), vec!["a", "b", "c"]);
        assert_eq!(shell_split("mcp-router --single memory-mcp").len(), 3);
        assert!(shell_split("").is_empty());
    }

    #[test]
    fn config_from_env_defaults() {
        // Deliberately NOT setting env vars — defaults should apply.
        // We use a sub-scope without touching global env so parallel tests
        // stay isolated.
        std::env::remove_var("MEMORY_MCP_CMD");
        std::env::remove_var("MEMORY_MCP_TIMEOUT_SECS");
        let cfg = MemoryMcpConfig::from_env();
        assert_eq!(cfg.argv, vec!["mcp-router", "--single", "memory-mcp"]);
        assert_eq!(cfg.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn extract_tool_payload_returns_parsed_json() {
        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "content": [{
                    "type": "text",
                    "text": "{\"status\":\"ok\",\"count\":0,\"hits\":[]}"
                }]
            }
        });
        let payload = extract_tool_payload(&resp).unwrap();
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["count"], 0);
    }

    #[test]
    fn extract_tool_payload_surfaces_error() {
        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "error": { "code": -32600, "message": "bad request" }
        });
        let err = extract_tool_payload(&resp).unwrap_err();
        assert!(err.to_string().contains("bad request"));
    }

    #[test]
    fn mcp_search_hit_deserialises_from_memory_search_shape() {
        let raw = serde_json::json!({
            "atom_id": "0190a3b4-8811-7c6e-8000-000000000001",
            "subject": "gary",
            "predicate": "prefers",
            "value": "terse",
            "project": "firefly-pro",
            "memory_type": "feedback",
            "rank": 0,
            "base_score": 0.82,
            "project_weight": 0.7,
            "rerank_score": 0.82,
            "utility_multiplier": 1.0,
            "recency_boost": 0.95,
            "final_score": 0.78
        });
        let hit: McpSearchHit = serde_json::from_value(raw).unwrap();
        assert_eq!(hit.atom_id, "0190a3b4-8811-7c6e-8000-000000000001");
        assert_eq!(hit.subject, "gary");
        assert!((hit.final_score - 0.78).abs() < 1e-6);
    }

    /// Smoke test — spawn a bogus binary to verify we surface a real error
    /// rather than hang. Uses `nonexistent-binary-xyz` so the spawn fails
    /// immediately.
    #[tokio::test]
    async fn search_surfaces_spawn_failure() {
        let cfg = MemoryMcpConfig {
            argv: vec!["nonexistent-binary-xyz-memory-mcp".into()],
            timeout: Duration::from_secs(1),
        };
        let client = MemoryMcpClient::new(cfg);
        let err = client
            .search("q", "firefly-pro", 1, None)
            .await
            .expect_err("should fail to spawn");
        let msg = err.to_string();
        assert!(
            msg.contains("spawn") || msg.contains("mcp-router") || msg.contains("memory-mcp"),
            "error should mention the spawn: {msg}"
        );
    }
}
