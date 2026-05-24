//! `sentinel legatus connect` dispatch — thin shim over
//! [`sentinel_legatus::run_connect`].

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use sentinel_legatus::{
    run_connect as legatus_run_connect, ConnectConfig, RuntimeKind, BOOTSTRAP_SECRET_LEN,
};
use tokio::sync::Notify;
use tracing::info;

#[allow(clippy::too_many_arguments)]
pub async fn run_connect(
    consulate_url: &str,
    bootstrap_secret_hex: &str,
    suggested_name: &str,
    working_dir: Option<&str>,
    branch: Option<String>,
    task_description: Option<String>,
    heartbeat_secs: u64,
) -> anyhow::Result<()> {
    let bootstrap_secret = parse_bootstrap_secret(bootstrap_secret_hex)?;
    let working_dir = match working_dir {
        Some(d) => d.to_owned(),
        None => std::env::current_dir()
            .context("failed to read current working directory")?
            .to_string_lossy()
            .into_owned(),
    };

    let config = ConnectConfig {
        consulate_url: consulate_url.to_owned(),
        bootstrap_secret,
        suggested_name: suggested_name.to_owned(),
        working_dir,
        branch,
        task_description,
        runtime: RuntimeKind::ClaudeCode,
        heartbeat_interval: Duration::from_secs(heartbeat_secs.max(1)),
        // Standalone `sentinel legatus connect` doesn't bind to an
        // operator — sessions register as ROOT.
        operator_id: None,
    };

    let cancel = Arc::new(Notify::new());
    let cancel_handle = Arc::clone(&cancel);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        info!("Ctrl-C received; shutting down legatus");
        cancel_handle.notify_one();
    });

    eprintln!(
        "  legatus connecting to {consulate_url}\n  Ctrl-C to send SessionCompleted and exit.",
    );
    legatus_run_connect(config, cancel)
        .await
        .context("legatus connect failed")
}

fn parse_bootstrap_secret(hex: &str) -> anyhow::Result<[u8; BOOTSTRAP_SECRET_LEN]> {
    let bytes =
        hex::decode(hex.trim()).with_context(|| "--bootstrap-secret must be hex-encoded bytes")?;
    let arr: [u8; BOOTSTRAP_SECRET_LEN] = bytes.as_slice().try_into().map_err(|_| {
        anyhow::anyhow!(
            "--bootstrap-secret must decode to exactly {BOOTSTRAP_SECRET_LEN} bytes; got {}",
            bytes.len(),
        )
    })?;
    Ok(arr)
}
