//! `sentinel daemon` — Starts MCP server + hook listener + dashboard API
//!
//! **Security layers:**
//! - Binds to 127.0.0.1 only (no network exposure)
//! - CORS restricted to localhost origins only (Attack #130)
//! - Per-instance bearer token auth (Attack #daemon-auth)

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use sentinel_domain::state::SessionState;
use sentinel_legatus::{
    make_pair, run_connect_hosted, ConnectConfig, EscalationKind, LegatusHandle,
    BOOTSTRAP_SECRET_LEN, RuntimeKind,
};
use tokio::sync::{Notify, RwLock};
use tracing::{info, warn};

/// Optional legatus configuration the daemon hosts alongside the
/// dashboard API. Constructed from the `--legatus-*` CLI flags;
/// when [`Self::consulate_url`] is `None`, the daemon runs with no
/// legatus (pre-commit-B behavior).
#[derive(Clone, Debug)]
pub struct LegatusOptions {
    /// `--legatus-consulate-url`.
    pub consulate_url: Option<String>,
    /// `--legatus-bootstrap-secret` / `CONSULATE_BOOTSTRAP_SECRET`.
    pub bootstrap_secret_hex: Option<String>,
    /// `--legatus-suggested-name`.
    pub suggested_name: String,
    /// `--legatus-working-dir` (default: daemon's cwd).
    pub working_dir: Option<String>,
    /// `--legatus-heartbeat-secs`.
    pub heartbeat_secs: u64,
}

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
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
    // Allow health checks and CCAM dashboard HTML without auth
    // (dashboard JS reads token from URL param and stores in localStorage)
    let path = req.uri().path();
    if path == "/api/health" || path == "/api/ccam" {
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

pub async fn run(port: u16, legatus: LegatusOptions) -> Result<()> {
    info!("Sentinel daemon starting on port {port}");

    // Generate per-instance bearer token for API auth
    let token = generate_bearer_token();
    let token_path = write_token_file(&token, port);
    info!("Bearer token written to {}", token_path.display());

    let state = Arc::new(RwLock::new(SessionState::new("daemon")));
    let app_state = crate::api::AppState { session: state };

    // Optionally host a legatus connection alongside the dashboard
    // API. When configured, expose POST /legatus/escalate +
    // GET /legatus/inbox/next for hook clients.
    let legatus_handle = start_legatus_if_configured(legatus).await?;

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

    let api_app = crate::api::router(app_state);
    let app = if let Some(handle) = &legatus_handle {
        api_app.merge(legatus_routes(handle.clone()))
    } else {
        api_app
    }
    .layer(axum::middleware::from_fn(bearer_auth))
    .layer(inject_token)
    .layer(cors);

    // CRITICAL: Always bind to localhost only. Never 0.0.0.0.
    let bind_addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    info!("Dashboard API listening on http://localhost:{port}");
    info!("Use 'Authorization: Bearer {token}' to authenticate API requests");

    axum::serve(listener, app).await?;

    // Clean up token file on shutdown
    let _ = std::fs::remove_file(&token_path);

    Ok(())
}

async fn start_legatus_if_configured(
    options: LegatusOptions,
) -> Result<Option<LegatusHandle>> {
    let Some(consulate_url) = options.consulate_url else {
        return Ok(None);
    };
    let secret_hex = options.bootstrap_secret_hex.ok_or_else(|| {
        anyhow::anyhow!(
            "--legatus-consulate-url requires --legatus-bootstrap-secret (or CONSULATE_BOOTSTRAP_SECRET)",
        )
    })?;
    let secret_bytes = hex::decode(secret_hex.trim())
        .with_context(|| "--legatus-bootstrap-secret must be hex-encoded bytes")?;
    let bootstrap_secret: [u8; BOOTSTRAP_SECRET_LEN] =
        secret_bytes.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!(
                "--legatus-bootstrap-secret must decode to exactly {BOOTSTRAP_SECRET_LEN} bytes; got {}",
                secret_bytes.len(),
            )
        })?;
    let working_dir = match options.working_dir {
        Some(d) => d,
        None => std::env::current_dir()
            .context("failed to read current working directory")?
            .to_string_lossy()
            .into_owned(),
    };
    let config = ConnectConfig {
        consulate_url: consulate_url.clone(),
        bootstrap_secret,
        suggested_name: options.suggested_name,
        working_dir,
        branch: None,
        task_description: None,
        runtime: RuntimeKind::ClaudeCode,
        heartbeat_interval: Duration::from_secs(options.heartbeat_secs.max(1)),
    };

    let (handle, runtime) = make_pair();
    let cancel = Arc::new(Notify::new());
    info!(url = %consulate_url, "daemon hosting legatus");
    tokio::spawn(async move {
        if let Err(err) = run_connect_hosted(config, cancel, runtime).await {
            warn!(?err, "hosted legatus exited with error");
        }
    });
    Ok(Some(handle))
}

fn legatus_routes(handle: LegatusHandle) -> Router {
    Router::new()
        .route("/legatus/escalate", post(handle_legatus_escalate))
        .route("/legatus/inbox/next", get(handle_legatus_inbox_next))
        .with_state(handle)
}

async fn handle_legatus_escalate(
    State(handle): State<LegatusHandle>,
    Json(event): Json<EscalationKind>,
) -> Result<StatusCode, (StatusCode, String)> {
    handle
        .escalate(event)
        .map_err(|err| (StatusCode::SERVICE_UNAVAILABLE, err.to_string()))?;
    Ok(StatusCode::ACCEPTED)
}

async fn handle_legatus_inbox_next(State(handle): State<LegatusHandle>) -> Response {
    match handle.try_pop_inbox().await {
        Some(instr) => match serde_json::to_value(&instr) {
            Ok(v) => (StatusCode::OK, Json(v)).into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("instruction serialize: {err}"),
            )
                .into_response(),
        },
        None => StatusCode::NO_CONTENT.into_response(),
    }
}
