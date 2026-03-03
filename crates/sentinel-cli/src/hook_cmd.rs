//! `sentinel hook` — Process hook events (thin client or standalone)

use anyhow::Result;
use tracing::debug;

use sentinel_infrastructure::stdin;
use sentinel_infrastructure::stdout;

pub async fn run(event: &str, matcher: Option<&str>, standalone: bool) -> Result<()> {
    debug!(event, ?matcher, standalone, "Processing hook event");

    // Read input from stdin
    let input = stdin::read_hook_input()?;

    // For now, standalone mode — all hooks run inline
    // TODO: In daemon mode, forward to daemon via IPC
    let output = sentinel_domain::events::HookOutput::allow();

    // Write output to stdout
    stdout::write_hook_output(&output)?;

    Ok(())
}
