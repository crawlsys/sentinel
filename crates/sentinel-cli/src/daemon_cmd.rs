//! `sentinel daemon` — Starts MCP server + hook listener + dashboard API
//!
//! **Security layers:**
//! - Binds to 127.0.0.1 only (no network exposure)
//! - CORS restricted to localhost origins only (Attack #130)
//! - Per-instance bearer token auth (Attack #daemon-auth)

use std::sync::Arc;

use anyhow::Result;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use tokio::sync::RwLock;
use tracing::info;

use sentinel_domain::state::SessionState;

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
    // Allow health checks without auth
    if req.uri().path() == "/api/health" {
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

pub async fn run(port: u16) -> Result<()> {
    info!("Sentinel daemon starting on port {port}");

    // Generate per-instance bearer token for API auth
    let token = generate_bearer_token();
    let token_path = write_token_file(&token, port);
    info!("Bearer token written to {}", token_path.display());

    let state = Arc::new(RwLock::new(SessionState::new("daemon")));
    let app_state = crate::api::AppState { session: state };

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

    // Clean up token file on shutdown
    let _ = std::fs::remove_file(&token_path);

    Ok(())
}
