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
    default_inbox_path, default_outbox_path, make_pair, make_pair_with_persistence,
    ConnectConfig, EscalationKind, LegatusHandle, PersistentEscalationOutbox,
    PersistentInbox, RuntimeKind, BOOTSTRAP_SECRET_LEN,
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
    /// `--legatus-witness-verify` (one of `none`, `in-memory`).
    /// Controls how inbound CatastrophicAck witnesses are verified
    /// before recording an approval in the daemon's cache:
    ///
    /// - `none` (default): no verifier installed; the daemon trusts
    ///   the ack on receipt. Matches v0.1 daemon-local trust model.
    /// - `in-memory`: wraps an `InMemoryPraefectusClient`. Useful
    ///   for dev / tests / demo flows where there's no real
    ///   Praefectus running but you still want the verification
    ///   surface exercised end-to-end.
    ///
    /// `http` (production HTTP-backed Praefectus) is the next add:
    /// requires a Praefectus URL + bearer token, both of which are
    /// consul-side config the consul agent owns surfacing.
    pub witness_verify: WitnessVerifyMode,
    /// `--legatus-operator-id <UUID>` — operator identity scaffold
    /// for single-operator deployments. When set, the daemon logs
    /// the binding at startup so operators can confirm the daemon
    /// is talking to "their" Praefectus. v0.2 will propagate this
    /// through RegisterSession metadata so the consulate can route
    /// per-operator (multi-operator support); for now it's a
    /// declarative breadcrumb that records intent + surfaces in
    /// the startup banner.
    pub operator_id: Option<uuid::Uuid>,
}

/// Verifier wiring mode for inbound CatastrophicAck witnesses.
/// Surfaced through `--legatus-witness-verify`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WitnessVerifyMode {
    /// No verifier installed; daemon trusts on receipt. Default.
    #[default]
    None,
    /// Wrap an InMemoryPraefectusClient. Dev / test mode -- the
    /// in-memory client accepts every witness unless test toggles
    /// force a fail.
    InMemory,
    /// Wrap an HttpPraefectusClient pointing at the operator's
    /// reachable Praefectus. Production mode -- every inbound
    /// CatastrophicAck witness is verified cryptographically
    /// against a live Praefectus before the approval is recorded.
    Http {
        /// Base URL of the Praefectus HTTP endpoint
        /// (e.g. `http://127.0.0.1:9001`).
        base_url: String,
        /// Bearer token used to authenticate every request.
        bearer_token: String,
    },
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

/// Emit the operator-facing startup banner to stderr.
///
/// **Why:** `tracing_subscriber` defaults to `warn` filter, so every
/// `info!` in the daemon startup path is silenced unless the operator
/// remembers `RUST_LOG=info`. Operators following the consul↔sentinel
/// runbook need *some* signal that the daemon started cleanly and where
/// to find the bearer token — without this banner, Terminal 2 is
/// completely silent and the integration appears broken.
fn print_daemon_banner(
    port: u16,
    token_path: &std::path::Path,
    token: &str,
    legatus: &LegatusOptions,
) {
    eprintln!();
    eprintln!("============================================================");
    eprintln!("  Sentinel daemon ready");
    eprintln!("============================================================");
    eprintln!("  Dashboard:     http://127.0.0.1:{port}");
    eprintln!("  Token path:    {} (file format: {{port}}:{{token}})", token_path.display());
    eprintln!("  Auth header:   Authorization: Bearer {token}");
    if let Some(url) = &legatus.consulate_url {
        eprintln!("  Legatus mode:  ON");
        eprintln!("  Consulate URL: {url}");
        eprintln!("  Display name:  {}", legatus.suggested_name);
        if let Some(wd) = &legatus.working_dir {
            eprintln!("  Working dir:   {wd}");
        }
        eprintln!("  Heartbeat:     {}s", legatus.heartbeat_secs);
    } else {
        eprintln!("  Legatus mode:  OFF (no --legatus-consulate-url)");
    }
    eprintln!("============================================================");
    eprintln!();
}

pub async fn run(port: u16, legatus: LegatusOptions) -> Result<()> {
    info!("Sentinel daemon starting on port {port}");

    // Generate per-instance bearer token for API auth
    let token = generate_bearer_token();
    let token_path = write_token_file(&token, port);
    info!("Bearer token written to {}", token_path.display());

    // Operator-facing startup banner. Goes to stderr so it is visible
    // regardless of RUST_LOG filter (default is `warn`, which swallows
    // every info! the daemon emits). Operators following a runbook need
    // to see *some* sign of life from Terminal 2 — without this, the
    // daemon log appears silent and the round-trip looks broken.
    print_daemon_banner(port, &token_path, &token, &legatus);

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
    let app = if let Some(legatus_state) = &legatus_handle {
        api_app.merge(legatus_routes(legatus_state.clone()))
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

/// Bundle returned by [`start_legatus_if_configured`] -- the handle
/// (for outbound escalations) plus the approval cache that the
/// inbound CatastrophicAck handler writes to and the new HTTP
/// route reads from.
#[derive(Clone)]
pub struct LegatusRouteState {
    pub handle: LegatusHandle,
    pub approval_cache: Arc<sentinel_legatus::CatastrophicApprovalCache>,
}

async fn start_legatus_if_configured(
    options: LegatusOptions,
) -> Result<Option<LegatusRouteState>> {
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
        // Carry operator identity in the RegisterSession so the
        // consulate populates Session.owner and the voice-attested
        // gate's resolver can route per-session escalations to the
        // correct operator. `None` (the default) keeps the v0.1
        // single-operator-as-ROOT scaffold semantics intact.
        operator_id: options
            .operator_id
            .map(sentinel_legatus::OperatorId::from_uuid),
    };

    // Persistent inbox + outbox at ~/.claude/sentinel/state/.
    // - Inbox (legatus-inbox.jsonl): operator instructions
    //   incoming from consul — survives daemon crash so the
    //   hook still gets them on next prompt.
    // - Outbox (legatus-escalations.jsonl): escalations outgoing
    //   to consul — survives daemon crash so the
    //   `InstructionResult { Declined }` emitted on cancel
    //   (and other lifecycle events) actually reaches consul on
    //   the next start.
    //
    // If we can't resolve $HOME (degenerate, but possible in
    // chroots / some CI containers), fall back to an in-memory
    // pair so the daemon still runs — both directions just
    // don't survive a daemon crash in that mode.
    let (handle, runtime) = if let (Some(inbox_path), Some(outbox_path)) =
        (default_inbox_path(), default_outbox_path())
    {
        let inbox = PersistentInbox::new(inbox_path);
        let outbox = PersistentEscalationOutbox::new(outbox_path);
        let queued_in = inbox.len();
        let queued_out = outbox.len();
        if queued_in > 0 {
            info!(
                queued = queued_in,
                path = ?inbox.path(),
                "rehydrated persistent inbox at startup",
            );
        }
        if queued_out > 0 {
            info!(
                queued = queued_out,
                path = ?outbox.path(),
                "rehydrated persistent outbox at startup — replaying on connect",
            );
        }
        make_pair_with_persistence(inbox, outbox)
    } else {
        warn!("no resolvable home dir; legatus inbox + outbox are in-memory only");
        make_pair()
    };
    // Approval cache: shared between the inbound CatastrophicAck
    // handler (writes when an ack arrives) and the
    // /legatus/catastrophic-acks HTTP route (reads on hook retry).
    // v0.2: persistent JSONL snapshot under
    // ~/.claude/sentinel/state/legatus-catastrophic-approvals.json
    // so pending approvals survive daemon restart. Falls back to
    // in-memory-only if HOME is unresolvable (degenerate hosts).
    let approval_cache = Arc::new(
        sentinel_legatus::CatastrophicApprovalCache::default_persistent()
            .unwrap_or_else(sentinel_legatus::CatastrophicApprovalCache::new),
    );
    if let Some(path) = sentinel_legatus::default_approval_cache_path() {
        let count = approval_cache.len();
        if count > 0 {
            info!(
                queued = count,
                path = %path.display(),
                "rehydrated persistent approval cache at startup",
            );
        }
    }
    // `--legatus-operator-id` flows into `ConnectConfig.operator_id`
    // above; the consulate uses it on `RegisterSession` to populate
    // `Session.owner`, and the voice-attested gate's
    // `OperatorResolverPort` then routes per-session escalations to
    // the correct operator's voice gate. Logged at startup so the
    // operator can confirm the daemon thinks it's bound to them.
    if let Some(op_id) = options.operator_id {
        info!(operator_id = %op_id, "daemon bound to operator");
        eprintln!("[daemon] operator binding: {op_id}");
    } else {
        info!("no --legatus-operator-id set; sessions register as OperatorId::ROOT");
    }
    let runtime = runtime.with_approval_cache(approval_cache.clone());
    // Spent-nonce log: replay protection on inbound CatastrophicAcks.
    // Persistent JSONL snapshot so a witness stashed and replayed
    // after a daemon restart still hits the spent set. Always
    // installed in the daemon path (the cost is minimal and the
    // security property is non-optional).
    let spent_nonces = Arc::new(
        sentinel_legatus::SpentNonceLog::default_persistent()
            .unwrap_or_else(sentinel_legatus::SpentNonceLog::new),
    );
    let runtime = runtime.with_spent_nonce_log(spent_nonces);
    // Optional witness verifier per --legatus-witness-verify. None
    // mode preserves the v0.1 daemon-local trust model; InMemory
    // mode wraps an InMemoryPraefectusClient via the
    // sentinel-application adapter -- useful for dev / demo flows
    // that want to exercise the verification surface without a real
    // Praefectus.
    let runtime = match options.witness_verify {
        WitnessVerifyMode::None => {
            info!("witness verifier: NOT installed (daemon-local trust)");
            runtime
        }
        WitnessVerifyMode::InMemory => {
            use sentinel_application::praefectus_client::InMemoryPraefectusClient;
            use sentinel_application::witness_verifier_adapter::PraefectusClientWitnessVerifier;
            info!("witness verifier: InMemoryPraefectusClient (dev/demo mode)");
            let client = Arc::new(InMemoryPraefectusClient::new());
            let verifier = Arc::new(PraefectusClientWitnessVerifier::new(client));
            runtime.with_witness_verifier(verifier)
        }
        WitnessVerifyMode::Http {
            base_url,
            bearer_token,
        } => {
            use sentinel_application::praefectus_client::{
                HttpPraefectusClient, HttpPraefectusConfig,
            };
            use sentinel_application::witness_verifier_adapter::PraefectusClientWitnessVerifier;
            info!(
                base_url = %base_url,
                "witness verifier: HttpPraefectusClient (production mode)"
            );
            let cfg = HttpPraefectusConfig {
                base_url,
                bearer_token,
                timeout: std::time::Duration::from_secs(5),
            };
            let client = HttpPraefectusClient::new(cfg).map_err(|e| {
                anyhow::anyhow!("failed to build HttpPraefectusClient: {e}")
            })?;
            let verifier = Arc::new(PraefectusClientWitnessVerifier::new(Arc::new(client)));
            runtime.with_witness_verifier(verifier)
        }
    };
    let cancel = Arc::new(Notify::new());
    info!(url = %consulate_url, "daemon hosting legatus");
    tokio::spawn(async move {
        // Reconnect wrapper: transient transport / heartbeat failures
        // trigger exponential backoff (1s → 30s cap) with cancel-honor.
        // VersionMismatch surfaces as fatal — restarting won't help.
        if let Err(err) =
            sentinel_legatus::client::run_connect_hosted_with_reconnect(config, cancel, runtime)
                .await
        {
            warn!(?err, "hosted legatus reconnect loop exited with fatal error");
        }
    });
    Ok(Some(LegatusRouteState {
        handle,
        approval_cache,
    }))
}

fn legatus_routes(state: LegatusRouteState) -> Router {
    // Two layered routers because the escalate/inbox/pending routes
    // were originally stated on `LegatusHandle`; the new
    // catastrophic-acks route is stated on `Arc<ApprovalCache>`.
    // axum supports merging routers with different State types as
    // long as nothing depends across the seam.
    let handle_state = state.handle.clone();
    let cache_state = state.approval_cache.clone();
    let handle_routes: Router = Router::new()
        .route("/legatus/escalate", post(handle_legatus_escalate))
        .route("/legatus/inbox/next", get(handle_legatus_inbox_next))
        .route("/legatus/pending", get(handle_legatus_pending))
        .with_state(handle_state);
    let cache_routes: Router = Router::new()
        .route(
            "/legatus/catastrophic-acks/{session_id}/{action_class}",
            get(handle_consume_catastrophic_ack),
        )
        .with_state(cache_state);
    handle_routes.merge(cache_routes)
}

/// `GET /legatus/catastrophic-acks/:session_id/:action_class`
///
/// Single-use approval check for the `catastrophic_escalation`
/// hook. Returns 200 with a JSON body containing the captured
/// transcript when an approval is present (and consumes it). 404
/// when no approval is present. Bearer-auth + localhost-bind +
/// single-use semantics mean replay across retries is structurally
/// blocked.
async fn handle_consume_catastrophic_ack(
    State(cache): State<Arc<sentinel_legatus::CatastrophicApprovalCache>>,
    axum::extract::Path((session_id, action_class)): axum::extract::Path<(String, String)>,
) -> Response {
    let Ok(uuid) = uuid::Uuid::parse_str(&session_id) else {
        return (StatusCode::BAD_REQUEST, "session_id must be a UUID").into_response();
    };
    let sid = sentinel_legatus::SessionId::from_uuid(uuid);
    match cache.consume(sid, &action_class) {
        Some(approval) => {
            let body = serde_json::json!({
                "transcript": approval.transcript,
                "age_seconds": approval.age.as_secs(),
            });
            (StatusCode::OK, Json(body)).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
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

/// `GET /legatus/pending` — operator-visible "what's in flight"
/// snapshot. Returns counts for the daemon's persistent inbox
/// (operator instructions queued for the next Claude Code
/// prompt) and persistent outbox (outbound escalations queued
/// for WS send). Useful for `consul status`-style operator
/// tooling and demo dashboards.
///
/// Response shape:
/// ```json
/// {"inbox_pending": 0, "outbox_pending": 3}
/// ```
///
/// File I/O is wrapped in `tokio::task::spawn_blocking` so the
/// HTTP handler never stalls on the advisory lock. Returns
/// `0`-counts for whichever direction lacks a persistent store
/// (e.g. standalone-CLI legatus with no daemon-hosted disk
/// state).
async fn handle_legatus_pending(State(handle): State<LegatusHandle>) -> Response {
    let inbox_pending = match handle.persistent_inbox().cloned() {
        Some(inbox) => tokio::task::spawn_blocking(move || inbox.len())
            .await
            .unwrap_or(0),
        None => 0,
    };
    let outbox_pending = match handle.persistent_outbox().cloned() {
        Some(outbox) => tokio::task::spawn_blocking(move || outbox.len())
            .await
            .unwrap_or(0),
        None => 0,
    };
    let body = serde_json::json!({
        "inbox_pending": inbox_pending,
        "outbox_pending": outbox_pending,
    });
    (StatusCode::OK, Json(body)).into_response()
}
