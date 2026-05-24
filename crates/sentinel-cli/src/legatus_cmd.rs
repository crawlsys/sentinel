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

/// `sentinel legatus init` — generate a fresh 32-byte bootstrap
/// secret using the OS CSPRNG and emit it as hex. When `output` is
/// `Some(path)`, write the hex to that file with mode 0600
/// (refusing to overwrite an existing file unless `force`).
/// Otherwise print to stdout.
///
/// 0600 + refuse-overwrite is intentional: the secret authenticates
/// every legatus connecting to the consulate; silent rotation can
/// silently lock the daemon out of the consul.
pub fn run_init(output: Option<String>, force: bool) -> anyhow::Result<()> {
    let mut bytes = [0u8; BOOTSTRAP_SECRET_LEN];
    getrandom::getrandom(&mut bytes)
        .context("getrandom failed; OS CSPRNG unavailable?")?;
    let hex_str = hex::encode(bytes);

    let Some(path_str) = output else {
        // Stdout path. Trailing newline so shell `cat` output looks
        // right; operators can capture with `$(sentinel legatus init)`.
        println!("{hex_str}");
        eprintln!(
            "Bootstrap secret ({BOOTSTRAP_SECRET_LEN} bytes / {} hex chars) emitted to stdout.",
            BOOTSTRAP_SECRET_LEN * 2
        );
        eprintln!(
            "Pass to both `consulate --bootstrap-secret <SECRET>` and \
             `sentinel daemon --legatus-bootstrap-secret <SECRET>` (or via \
             CONSULATE_BOOTSTRAP_SECRET in env)."
        );
        return Ok(());
    };

    let path = std::path::PathBuf::from(&path_str);
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists; refusing to overwrite without --force \
             (rotating the bootstrap secret silently can lock running \
             legati out of the consul)",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent directory {}", parent.display()))?;
        }
    }
    // Write + chmod 0600. On non-Unix this falls back to the default
    // permissions; document the limitation in the help text.
    std::fs::write(&path, &hex_str)
        .with_context(|| format!("writing bootstrap secret to {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&path)?.permissions();
        perm.set_mode(0o600);
        std::fs::set_permissions(&path, perm)
            .with_context(|| format!("setting 0600 on {}", path.display()))?;
    }
    eprintln!("Wrote bootstrap secret to {} (mode 0600)", path.display());
    Ok(())
}

/// `sentinel legatus status` — query the running daemon's
/// `/legatus/health` (and `/legatus/pending`) and pretty-print the
/// connection state + pending outbox depth. Reads daemon port +
/// bearer token from `~/.claude/sentinel/daemon-token`.
pub async fn run_status(json: bool) -> anyhow::Result<()> {
    let (port, token) = read_daemon_token()
        .context("could not read daemon token; is `sentinel daemon` running?")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("building http client")?;

    let health_url = format!("http://127.0.0.1:{port}/legatus/health");
    let pending_url = format!("http://127.0.0.1:{port}/legatus/pending");
    let auth = format!("Bearer {token}");

    let health_resp = client
        .get(&health_url)
        .header("Authorization", &auth)
        .send()
        .await
        .with_context(|| format!("GET {health_url}"))?;
    if !health_resp.status().is_success() {
        anyhow::bail!(
            "daemon returned {} for /legatus/health (token mismatch? \
             daemon started without --legatus-consulate-url?)",
            health_resp.status()
        );
    }
    let health: serde_json::Value = health_resp
        .json()
        .await
        .context("parsing /legatus/health body")?;

    // /legatus/pending is best-effort — older daemons may not have
    // it, and a daemon running without legatus mode returns 404.
    // Surface depth when available; ignore otherwise.
    let pending_count: Option<usize> = match client
        .get(&pending_url)
        .header("Authorization", &auth)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("count").and_then(serde_json::Value::as_u64))
            .map(|n| n as usize),
        _ => None,
    };

    if json {
        let combined = serde_json::json!({
            "health": health,
            "pending_outbox_count": pending_count,
        });
        println!("{}", serde_json::to_string_pretty(&combined)?);
        return Ok(());
    }

    let state = health
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    println!("daemon:   http://127.0.0.1:{port}");
    println!("status:   {state}");
    match pending_count {
        Some(n) => println!("outbox:   {n} pending escalation(s)"),
        None => println!("outbox:   (unavailable — daemon may be running without legatus mode)"),
    }
    Ok(())
}

fn read_daemon_token() -> Option<(u16, String)> {
    let path = dirs::home_dir()?
        .join(".claude")
        .join("sentinel")
        .join("daemon-token");
    let content = std::fs::read_to_string(&path).ok()?;
    let trimmed = content.trim();
    let (port_str, token) = trimmed.split_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    Some((port, token.to_owned()))
}
