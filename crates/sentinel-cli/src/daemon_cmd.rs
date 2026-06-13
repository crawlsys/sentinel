//! `sentinel daemon` — Starts MCP server + hook listener + dashboard API
//!
//! **Security layers:**
//! - Binds to 127.0.0.1 only (no network exposure)
//! - CORS restricted to localhost origins only (Attack #130)
//! - Per-instance bearer token auth (Attack #daemon-auth)

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use sentinel_domain::state::SessionState;
use tokio::sync::RwLock;
use tracing::info;

/// Generate a random bearer token for this daemon instance.
fn generate_bearer_token() -> String {
    let mut bytes = [0u8; 32];
    if getrandom::getrandom(&mut bytes).is_err() {
        // Fallback — use PID + time (weak but better than nothing)
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(std::process::id().to_le_bytes());
        hasher.update(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_le_bytes(),
        );
        bytes.copy_from_slice(&hasher.finalize());
    }
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Write the bearer token to a file so authorized clients can read it.
/// Write the bearer token to a file atomically.
/// **Attack #153 fix**: Write to a temp file with restricted permissions FIRST,
/// then rename into place. This eliminates the TOCTOU window where the token
/// file exists with default ACLs before icacls/chmod runs.
fn write_token_file(token: &str, port: u16) -> std::path::PathBuf {
    let dir = dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory")
        .join(".claude")
        .join("sentinel");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("daemon-token");
    let tmp_path = dir.join(".daemon-token.tmp");
    let content = format!("{port}:{token}");

    // Write to temp file first
    let _ = std::fs::write(&tmp_path, &content);

    // Restrict permissions on the temp file BEFORE renaming
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let tmp_str = tmp_path.to_string_lossy();
        let username = std::env::var("USERNAME").unwrap_or_default();
        if !username.is_empty() {
            let _ = std::process::Command::new("icacls")
                .args([tmp_str.as_ref(), "/inheritance:r"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            let _ = std::process::Command::new("icacls")
                .args([tmp_str.as_ref(), "/grant:r", &format!("{username}:F")])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
    }

    // Atomic rename — file appears at final path already hardened
    let _ = std::fs::rename(&tmp_path, &path);

    path
}

/// Axum middleware that validates the bearer token on every request.
/// The /api/health endpoint is exempt (used for liveness checks).
async fn bearer_auth(req: Request, next: Next) -> Result<Response, axum::http::StatusCode> {
    // Allow health checks without auth (used for liveness checks).
    let path = req.uri().path();
    if path == "/api/health" {
        return Ok(next.run(req).await);
    }

    // Extract expected token from extension
    let expected = req.extensions().get::<BearerToken>().map(|t| t.0.clone());

    let Some(expected) = expected else {
        return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    };

    // Check Authorization header
    let auth_header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Some(token) = auth_header.strip_prefix("Bearer ") {
        if token == expected {
            return Ok(next.run(req).await);
        }
    }

    Err(axum::http::StatusCode::UNAUTHORIZED)
}

/// Wrapper type for the bearer token stored in request extensions.
#[derive(Clone)]
struct BearerToken(String);

/// Emit the operator-facing startup banner to stderr.
///
/// **Why:** `tracing_subscriber` defaults to `warn` filter, so every
/// `info!` in the daemon startup path is silenced unless the operator
/// remembers `RUST_LOG=info`. Operators following the sentinel
/// runbook need *some* signal that the daemon started cleanly and where
/// to find the bearer token — without this banner, Terminal 2 is
/// completely silent and the integration appears broken.
fn print_daemon_banner(port: u16, token_path: &std::path::Path, token: &str) {
    eprintln!();
    eprintln!("============================================================");
    eprintln!("  Sentinel daemon ready");
    eprintln!("============================================================");
    eprintln!("  Dashboard:     http://127.0.0.1:{port}");
    eprintln!("  Token path:    {} (file format: {{port}}:{{token}})", token_path.display());
    eprintln!("  Auth header:   Authorization: Bearer {token}");
    eprintln!("============================================================");
    eprintln!();
}

pub async fn run(port: u16) -> Result<()> {
    info!("Sentinel daemon starting on port {port}");

    // Generate per-instance bearer token for API auth
    let token = generate_bearer_token();
    let token_path = write_token_file(&token, port);
    info!("Bearer token written to {}", token_path.display());

    // PID file — written next to the token file so `sentinel stop`
    // can find this daemon and send it SIGTERM. Mode 0644 (world-
    // readable, owner-writable). Cleaned up on graceful shutdown
    // below.
    let pid_path = write_pid_file()?;
    info!("PID file written to {}", pid_path.display());

    // Operator-facing startup banner. Goes to stderr so it is visible
    // regardless of RUST_LOG filter (default is `warn`, which swallows
    // every info! the daemon emits). Operators following a runbook need
    // to see *some* sign of life from Terminal 2 — without this, the
    // daemon log appears silent and the round-trip looks broken.
    print_daemon_banner(port, &token_path, &token);

    let state = Arc::new(RwLock::new(SessionState::new("daemon")));
    let app_state = crate::api::AppState { session: state };

    // Tier B Linear enforcement spine. When SENTINEL_LINEAR_TOKEN is set, hold a
    // live `graphql-transport-ws` subscription to Linear and react to every issue
    // state-change in real time. Defaults to Shadow mode (log-only, zero
    // mutations) unless SENTINEL_LINEAR_ENFORCE=live. Fire-and-forget: a detached
    // task that logs and exits on permanent PAT rejection without taking the
    // daemon down.
    if let Some(enforcer_cfg) = sentinel_infrastructure::linear_enforcer::EnforcerConfig::from_env()
    {
        info!(
            mode = ?enforcer_cfg.mode,
            "Linear enforcer enabled — starting real-time subscription"
        );
        tokio::spawn(async move {
            sentinel_infrastructure::linear_enforcer::run_enforcer(enforcer_cfg).await;
        });
    } else {
        info!("Linear enforcer disabled (SENTINEL_LINEAR_TOKEN not set)");
    }

    // **Attack #130 fix**: Restrict CORS to localhost origins only.
    // The previous `Any` CORS policy allowed JavaScript from any origin to access
    // the dashboard API. Even though we bind to 127.0.0.1, a malicious website
    // could make fetch() calls to localhost:{port} and read proof chains, workflow
    // state, and hook stats via the browser's CORS preflight.
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin([
            format!("http://localhost:{port}")
                .parse::<axum::http::HeaderValue>()
                .unwrap(),
            format!("http://127.0.0.1:{port}")
                .parse::<axum::http::HeaderValue>()
                .unwrap(),
        ])
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any);

    // Inject bearer token into request extensions for the auth middleware
    let token_clone = token.clone();
    let inject_token = axum::middleware::from_fn(move |mut req: Request, next: Next| {
        let t = token_clone.clone();
        async move {
            req.extensions_mut().insert(BearerToken(t));
            next.run(req).await
        }
    });

    let app = crate::api::router(app_state)
        .layer(axum::middleware::from_fn(bearer_auth))
        .layer(inject_token)
        .layer(cors);

    // CRITICAL: Always bind to localhost only. Never 0.0.0.0.
    let bind_addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    info!("Dashboard API listening on http://localhost:{port}");
    info!("Use 'Authorization: Bearer {token}' to authenticate API requests");

    axum::serve(listener, app).await?;

    // Clean up token file + PID file on shutdown
    let _ = std::fs::remove_file(&token_path);
    let _ = std::fs::remove_file(&pid_path);

    Ok(())
}

/// Write the daemon's PID to `~/.claude/sentinel/daemon-pid` so
/// `sentinel stop` can find it. Refuses to overwrite an existing
/// PID file when the named PID is still alive — that signals a
/// daemon is already running and starting a second would just race
/// it on the same token file. When the PID file is stale (process
/// gone), it's silently replaced.
fn write_pid_file() -> Result<std::path::PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join(".claude")
        .join("sentinel");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("daemon-pid");

    if let Ok(existing) = std::fs::read_to_string(&path) {
        if let Ok(prev_pid) = existing.trim().parse::<i32>() {
            if is_pid_alive(prev_pid) {
                anyhow::bail!(
                    "a sentinel daemon (pid {prev_pid}) is already running per {}; \
                     stop it first with `sentinel stop` or delete the PID file if \
                     you're sure it's stale",
                    path.display()
                );
            }
            // Stale; fall through and overwrite.
            info!(stale_pid = prev_pid, "overwriting stale daemon-pid file");
        }
    }

    let pid = std::process::id();
    let content = format!("{pid}\n");
    std::fs::write(&path, content)?;
    Ok(path)
}

/// True when the named PID is alive. Shells out to `/bin/kill -0
/// <pid>` rather than calling `libc::kill` directly so the
/// workspace's `unsafe_code = "forbid"` lint stays clean. The
/// signal-0 idiom checks process existence without delivering a
/// signal; exit code 0 means alive, anything else means gone.
/// On non-Unix, returns `true` conservatively (better to refuse
/// to start than silently double up).
#[cfg(unix)]
fn is_pid_alive(pid: i32) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
#[cfg(not(unix))]
const fn is_pid_alive(_pid: i32) -> bool {
    true
}

/// `sentinel stop` — read `~/.claude/sentinel/daemon-pid`, send
/// SIGTERM, wait up to `wait_secs` for clean exit, then remove the
/// PID file if the daemon hadn't already done so.
///
/// Returns Ok(()) when the daemon was successfully signalled
/// (regardless of whether it exited within the wait window).
/// Returns Err when there's no PID file, when the PID file's
/// process doesn't exist, or when SIGTERM itself fails.
#[cfg(unix)]
pub fn run_stop(wait_secs: u64) -> Result<()> {
    let path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join(".claude")
        .join("sentinel")
        .join("daemon-pid");
    let pid_str = std::fs::read_to_string(&path)
        .with_context(|| format!("reading PID file at {}", path.display()))?;
    let pid: i32 = pid_str
        .trim()
        .parse()
        .with_context(|| format!("parsing PID from {}", path.display()))?;

    if !is_pid_alive(pid) {
        eprintln!("PID {pid} from {} is not alive; cleaning up stale file", path.display());
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }

    // Shell out to /bin/kill -TERM to keep the workspace's
    // `unsafe_code = "forbid"` lint clean. SIGTERM is a polite
    // shutdown signal; the daemon's axum::serve loop handles it via
    // tokio's default SIGTERM handler that drops the runtime.
    let status = std::process::Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .context("invoking /bin/kill")?;
    if !status.success() {
        anyhow::bail!("/bin/kill -TERM {pid} returned {status}");
    }
    eprintln!("Sent SIGTERM to PID {pid}");

    // Poll for exit. Daemon removes the PID file in its graceful
    // shutdown path, so file-gone is a reliable success signal.
    // Also check liveness directly in case the daemon was killed
    // out from under us by something else.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait_secs);
    while std::time::Instant::now() < deadline {
        if !path.exists() || !is_pid_alive(pid) {
            eprintln!("Daemon exited cleanly");
            // Belt-and-suspenders: tokio's default SIGTERM handler
            // aborts the runtime before the daemon's post-serve
            // cleanup code can run, so neither the PID file nor
            // the daemon-token file get removed by the daemon
            // itself. Clean up both here so the next
            // `sentinel daemon` start isn't confused by stale
            // state.
            let _ = std::fs::remove_file(&path);
            if let Some(token_path) = path
                .parent()
                .map(|p| p.join("daemon-token"))
            {
                let _ = std::fs::remove_file(&token_path);
            }
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    eprintln!(
        "Daemon did not exit within {wait_secs}s; SIGTERM delivered. \
         Re-run `sentinel stop` or send SIGKILL manually if it's stuck."
    );
    Ok(())
}
#[cfg(not(unix))]
pub fn run_stop(_wait_secs: u64) -> Result<()> {
    anyhow::bail!("sentinel stop is only implemented on Unix today")
}
