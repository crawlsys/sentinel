//! `sentinel mcp` — MCP server over stdio
//!
//! Claude Code connects to this as an MCP server.
//! Reads JSON-RPC requests from stdin, writes responses to stdout.

use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;
use tracing::{debug, error, info};

use sentinel_application::judge_service::FallbackJudge;
use sentinel_application::mcp_handler::{McpHandler, McpToolCall};
use sentinel_application::proof_engine::ProofEngine;
use sentinel_domain::state::SessionState;
use sentinel_infrastructure::mcp_transport::{JsonRpcRequest, JsonRpcResponse};

/// MCP tool definitions — what we advertise to Claude Code
fn tool_definitions() -> serde_json::Value {
    serde_json::json!({
        "tools": [
            {
                "name": "sentinel__get_proof_chain",
                "description": "Get the cryptographic proof chain for a skill execution. Returns all phase proofs with tessera hashes, evidence, and judge verdicts.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name (e.g., 'linear')"
                        }
                    },
                    "required": ["skill"]
                }
            },
            {
                "name": "sentinel__get_workflow_status",
                "description": "Get the current workflow state for a skill. Shows which phases are completed, current phase, and what's next.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name (e.g., 'linear')"
                        }
                    },
                    "required": ["skill"]
                }
            },
            {
                "name": "sentinel__verify_chain",
                "description": "Re-verify the integrity of a skill's proof chain. Checks all hashes are consistent and no tampering has occurred.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name to verify"
                        }
                    },
                    "required": ["skill"]
                }
            },
            {
                "name": "sentinel__submit_phase_complete",
                "description": "Notify sentinel that a skill phase has been completed. Sentinel will evaluate the evidence and add a proof to the chain if sufficient.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name"
                        },
                        "phase_id": {
                            "type": "string",
                            "description": "Phase ID (e.g., 'claim', 'fetch', 'review')"
                        },
                        "summary": {
                            "type": "string",
                            "description": "Brief summary of what was done in this phase"
                        }
                    },
                    "required": ["skill", "phase_id", "summary"]
                }
            },
            {
                "name": "sentinel__get_session_stats",
                "description": "Get execution statistics for the current session — hook invocations, blocked calls, per-hook timing.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }
        ]
    })
}

/// Server info for MCP initialize response
fn server_info() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "sentinel",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

pub async fn run() -> Result<()> {
    let state = Arc::new(RwLock::new(SessionState::new("mcp-session")));
    let judge: Arc<dyn sentinel_application::judge_service::JudgeService> =
        match sentinel_infrastructure::anthropic::AnthropicClient::from_env() {
            Ok(client) => Arc::new(client),
            Err(_) => {
                info!("No ANTHROPIC_API_KEY — using fallback judge");
                Arc::new(FallbackJudge)
            }
        };
    let proof_engine = Arc::new(ProofEngine::new(state.clone(), judge));
    let handler = McpHandler::new(state.clone(), proof_engine);

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    info!("Sentinel MCP server started (stdio)");

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            debug!("stdin closed, shutting down MCP server");
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to parse JSON-RPC request: {e}");
                continue;
            }
        };

        let response = handle_request(&request, &handler, &state).await;

        let json = serde_json::to_string(&response)?;
        stdout.write_all(json.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}

async fn handle_request(
    request: &JsonRpcRequest,
    handler: &McpHandler,
    state: &Arc<RwLock<SessionState>>,
) -> JsonRpcResponse {
    match request.method.as_str() {
        // MCP lifecycle
        "initialize" => JsonRpcResponse::success(request.id.clone(), server_info()),

        "initialized" => {
            // Notification — no response needed, but we send one anyway for safety
            JsonRpcResponse::success(request.id.clone(), serde_json::json!({}))
        }

        // Tool listing
        "tools/list" => JsonRpcResponse::success(request.id.clone(), tool_definitions()),

        // Tool execution
        "tools/call" => {
            let tool_name = request
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = request
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_default();

            // Handle submit_phase_complete specially (needs state mutation)
            if tool_name == "sentinel__submit_phase_complete" {
                return handle_submit_phase(request, &arguments, state).await;
            }

            // Handle get_session_stats specially
            if tool_name == "sentinel__get_session_stats" {
                let s = state.read().await;
                let stats = serde_json::json!({
                    "session_id": s.session_id,
                    "active_skill": s.active_skill,
                    "total_invocations": s.hook_stats.total_invocations,
                    "total_blocked": s.hook_stats.total_blocked,
                    "per_hook": s.hook_stats.per_hook,
                    "workflows": s.workflows.keys().collect::<Vec<_>>(),
                    "proof_chains": s.proof_chains.keys().collect::<Vec<_>>(),
                });
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(true, stats),
                );
            }

            let call = McpToolCall {
                name: tool_name.to_string(),
                arguments,
            };

            let result = handler.handle(call).await;

            if result.success {
                JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, result.content))
            } else {
                JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(false, serde_json::json!({"error": result.error})),
                )
            }
        }

        // Ping
        "ping" => JsonRpcResponse::success(request.id.clone(), serde_json::json!({})),

        // Unknown method
        method => JsonRpcResponse::error(
            request.id.clone(),
            -32601,
            format!("Method not found: {method}"),
        ),
    }
}

/// Handle sentinel__submit_phase_complete
async fn handle_submit_phase(
    request: &JsonRpcRequest,
    args: &serde_json::Value,
    state: &Arc<RwLock<SessionState>>,
) -> JsonRpcResponse {
    let skill = match args.get("skill").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, serde_json::json!({"error": "Missing 'skill'"})),
            )
        }
    };
    let phase_id = match args.get("phase_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, serde_json::json!({"error": "Missing 'phase_id'"})),
            )
        }
    };
    let summary = args
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Update state — mark phase complete, advance workflow
    let mut s = state.write().await;
    s.set_active_skill(&skill);

    if let Some(wf) = s.workflows.get_mut(&skill) {
        wf.advance(&phase_id);
    }

    let completed = s
        .workflows
        .get(&skill)
        .map(|w| w.completed_phases.clone())
        .unwrap_or_default();

    JsonRpcResponse::success(
        request.id.clone(),
        mcp_tool_result(
            true,
            serde_json::json!({
                "phase_id": phase_id,
                "skill": skill,
                "summary": summary,
                "completed_phases": completed,
                "message": format!("Phase '{}' marked complete", phase_id)
            }),
        ),
    )
}

/// Format MCP tool result in the standard content array format
fn mcp_tool_result(success: bool, data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&data).unwrap_or_default()
        }],
        "isError": !success
    })
}
