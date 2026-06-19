//! Browser Test CLI — record and check browser test state
//!
//! Used by Claude Code to mark a browser test as passed when the test was run
//! via CDP, Puppeteer, Playwright, or any method other than an MCP server
//! that already has its own `PostToolUse` recorder.
//!
//! The `pre_push_browser_test` hook checks for a state file written by this
//! command (or by the `PostToolUse` handler for
//! `mcp__browserbase__release_session` or `mcp__cdp__close_instance`).
//!
//! Usage:
//!   sentinel browser-test record --session <id>   # Mark test as passed
//!   sentinel browser-test check  --session <id>   # Check if test is valid

use sentinel_application::hooks::pre_push_browser_test::{
    has_recent_browser_test_pub, record_browser_test_passed,
};
use sentinel_infrastructure::filesystem::RealFileSystem;

/// Record a passing browser test for the given session.
/// If no session ID is provided, reads `CLAUDE_SESSION_ID` from env.
pub fn record(session: Option<String>) -> anyhow::Result<()> {
    let session_id = resolve_session(session)?;
    record_browser_test_passed(&RealFileSystem, &session_id);
    println!("Browser test recorded for session {session_id}");
    Ok(())
}

/// Check if a valid browser test exists for the given session.
pub fn check(session: Option<String>) -> anyhow::Result<()> {
    let session_id = resolve_session(session)?;
    if has_recent_browser_test_pub(&RealFileSystem, &session_id) {
        println!("PASS — valid browser test found for session {session_id}");
        Ok(())
    } else {
        println!("FAIL — no valid browser test for session {session_id}");
        std::process::exit(1);
    }
}

/// Resolve session ID from arg or `CLAUDE_SESSION_ID` env var.
fn resolve_session(session: Option<String>) -> anyhow::Result<String> {
    match session {
        Some(s) => Ok(s),
        None => std::env::var("CLAUDE_SESSION_ID")
            .map_err(|_| anyhow::anyhow!("No --session provided and CLAUDE_SESSION_ID not set")),
    }
}
