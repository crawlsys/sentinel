//! Stdout Writer
//!
//! Writes hook output JSON to stdout for Claude Code to consume.

use anyhow::Result;

use sentinel_domain::events::HookOutput;

/// Write hook output to stdout
pub fn write_hook_output(output: &HookOutput) -> Result<()> {
    let json = serde_json::to_string(output)?;
    println!("{json}");
    Ok(())
}
