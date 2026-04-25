//! Steel Test CLI — record and check browser test state
//!
//! Used by Claude Code to mark a browser test as passed when the test was run
//! via CDP, Puppeteer, or any method other than the Steel MCP server.
//!
//! The pre_push_steel_test hook checks for a state file written by this command
//! (or by the PostToolUse handler for mcp__steel__release_session).
//!
//! Usage:
//!   sentinel steel-test record --session <id>   # Mark test as passed
//!   sentinel steel-test check  --session <id>   # Check if test is valid

use sentinel_application::hooks::pre_push_steel_test::{
    has_recent_steel_test_pub, record_steel_test_passed,
};
use sentinel_infrastructure::filesystem::RealFileSystem;

/// Record a passing browser test for the given session.
/// If no session ID is provided, reads CLAUDE_SESSION_ID from env.
pub async fn record(session: Option<String>) -> anyhow::Result<()> {
    let session_id = resolve_session(session)?;
    record_steel_test_passed(&RealFileSystem, &session_id);
    println!("Steel test recorded for session {session_id}");
    Ok(())
}

/// Check if a valid browser test exists for the given session.
pub async fn check(session: Option<String>) -> anyhow::Result<()> {
    let session_id = resolve_session(session)?;
    if has_recent_steel_test_pub(&RealFileSystem, &session_id) {
        println!("PASS — valid Steel test found for session {session_id}");
        Ok(())
    } else {
        println!("FAIL — no valid Steel test for session {session_id}");
        std::process::exit(1);
    }
}

/// Resolve session ID from arg or CLAUDE_SESSION_ID env var.
fn resolve_session(session: Option<String>) -> anyhow::Result<String> {
    match session {
        Some(s) => Ok(s),
        None => std::env::var("CLAUDE_SESSION_ID")
            .map_err(|_| anyhow::anyhow!("No --session provided and CLAUDE_SESSION_ID not set")),
    }
}
