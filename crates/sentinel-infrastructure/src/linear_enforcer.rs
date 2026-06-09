//! Linear real-time enforcement engine (Tier B of the "bad PM is bad software"
//! factory) — the always-on spine.
//!
//! Holds a live `graphql-transport-ws` subscription to Linear
//! (`wss://api.linear.app/graphql`) and reacts in real time to every issue
//! state-change. When a ticket transitions into a *started* state (In Progress /
//! Code Review / …) while it is NOT dev-ready (no estimate, no Type label, no
//! Area label, or still sitting in Triage), the enforcer moves it back and posts
//! a comment listing the missing fields. This catches EVERYONE — UI edits and
//! API edits alike — not just an in-session agent.
//!
//! Ported from the proven `LinearSubscriptionWsClient` in legatus-desktop
//! (LDT-7, Landed + tested). Self-contained: sentinel's own error type, no
//! legatus deps. The `graphql-transport-ws` handshake, per-topic subscribe,
//! reconnect/backoff ladder, and PAT-rejection handling are faithful to the
//! source.
//!
//! ## Scope invariant
//! This engine reacts ONLY to Linear issue events by construction — non-Linear
//! work never enters it. If you are not working a Linear ticket, the enforcer
//! does not see you.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{HeaderValue, SEC_WEBSOCKET_PROTOCOL};
use tokio_tungstenite::tungstenite::protocol::Message;
use uuid::Uuid;

/// Linear's GraphQL websocket endpoint. Override via `SENTINEL_LINEAR_WS_URL`.
const DEFAULT_WS_URL: &str = "wss://api.linear.app/graphql";
/// Reconnect backoff ladder (seconds), faithful to the legatus source.
const RECONNECT_BACKOFF_SEQ: &[u64] = &[1, 2, 4, 8, 16, 30];
/// Consecutive PAT (auth) rejections before the loop gives up.
const PAT_REJECTION_PERMANENT_THRESHOLD: u32 = 5;

/// The `issueHistoryCreated` subscription — fires on every issue change. This is
/// the enforcement trigger.
const ISSUE_HISTORY_CREATED_SUB: &str = r"
subscription IssueHistoryCreated {
  issueHistoryCreated { id issue { id } }
}
";

/// An issue-change event surfaced from the live subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueChanged {
    /// Linear issue UUID.
    pub linear_issue_id: String,
}

/// Connection state, used to flip the catch-up poll cadence (Live/Reconnecting →
/// 5-min backstop; `UnavailablePermanent` → give up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionState {
    Live,
    Reconnecting,
    UnavailablePermanent,
}

/// Errors from the enforcement engine.
#[derive(Debug, thiserror::Error)]
pub enum EnforcerError {
    #[error("subscription connect failed: {0}")]
    ConnectFailed(String),
    #[error("subscription auth (PAT) rejected")]
    PatRejected,
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("linear api: {0}")]
    Api(String),
}

// ----- graphql-transport-ws frames --------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    ConnectionInit { payload: Value },
    Subscribe { id: String, payload: SubscribePayload },
    Pong,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg {
    ConnectionAck,
    Next {
        #[allow(dead_code)]
        id: String,
        payload: NextPayload,
    },
    Error { id: String, payload: Value },
    Complete {
        #[allow(dead_code)]
        id: String,
    },
    Ping,
    Pong,
}

#[derive(Debug, Serialize, Deserialize)]
struct SubscribePayload {
    query: String,
    variables: Value,
}

#[derive(Debug, Deserialize)]
struct NextPayload {
    data: Value,
}

// ----- the dev-ready bar (shared with Tier A / Tier C) ------------------

/// A ticket's enforcement-relevant fields, fetched via the Linear REST GraphQL.
#[derive(Debug, Clone)]
pub struct TicketReadiness {
    pub identifier: String,
    pub estimate: Option<f64>,
    pub state_type: String,
    pub state_name: String,
    pub has_type_label: bool,
    pub has_area_label: bool,
}

impl TicketReadiness {
    /// The states where work is considered "started" — a ticket entering one of
    /// these while not dev-ready is the enforcement trigger.
    fn is_started(&self) -> bool {
        // Linear state types: triage, backlog, unstarted, started, completed,
        // canceled. "started" covers In Progress / Code Review / QA Testing.
        self.state_type == "started"
    }

    /// The missing dev-ready dimensions (empty == dev-ready).
    fn missing(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if self.estimate.is_none() {
            m.push("estimate");
        }
        if !self.has_type_label {
            m.push("Type label");
        }
        if !self.has_area_label {
            m.push("Area label");
        }
        if self.state_type == "triage" {
            m.push("still in Triage");
        }
        m
    }

    /// The missing dev-ready dimensions as owned `String`s (for serializing
    /// into the escalation graph's audit state).
    #[must_use]
    pub fn missing_owned(&self) -> Vec<String> {
        self.missing().iter().map(|s| (*s).to_string()).collect()
    }

    /// Should this ticket be reverted? Only when it has STARTED but is not ready.
    #[must_use]
    pub fn should_revert(&self) -> bool {
        self.is_started() && !self.missing().is_empty()
    }
}

// ----- the wss subscription client --------------------------------------

/// Outcome of one connect-and-handshake attempt.
enum ConnectOutcome {
    Ok(
        futures::stream::SplitSink<WsStream, Message>,
        futures::stream::SplitStream<WsStream>,
    ),
    PatRejected,
    Failed(EnforcerError),
}

type WsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

fn ws_url() -> String {
    std::env::var("SENTINEL_LINEAR_WS_URL").unwrap_or_else(|_| DEFAULT_WS_URL.to_string())
}

fn build_request(
    url: &str,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, EnforcerError> {
    let mut req = url
        .into_client_request()
        .map_err(|e| EnforcerError::ConnectFailed(e.to_string()))?;
    req.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static("graphql-transport-ws"),
    );
    Ok(req)
}

async fn connect_once(url: &str, token: &str) -> ConnectOutcome {
    let req = match build_request(url) {
        Ok(r) => r,
        Err(e) => return ConnectOutcome::Failed(e),
    };
    let (ws, _resp) = match tokio_tungstenite::connect_async(req).await {
        Ok(p) => p,
        Err(e) => return ConnectOutcome::Failed(EnforcerError::ConnectFailed(e.to_string())),
    };
    let (mut sink, mut reader) = ws.split();

    // connection_init { Authorization: <token> }
    let init = ClientMsg::ConnectionInit {
        payload: serde_json::json!({ "Authorization": token }),
    };
    match serde_json::to_string(&init) {
        Ok(s) => {
            if let Err(e) = sink.send(Message::Text(s)).await {
                return ConnectOutcome::Failed(EnforcerError::ConnectFailed(e.to_string()));
            }
        }
        Err(e) => return ConnectOutcome::Failed(EnforcerError::Serde(e)),
    }

    // expect connection_ack (or a 4401 close = PAT rejected)
    match reader.next().await {
        Some(Ok(Message::Text(t))) => match serde_json::from_str::<ServerMsg>(&t) {
            Ok(ServerMsg::ConnectionAck) => {}
            Ok(other) => {
                return ConnectOutcome::Failed(EnforcerError::ConnectFailed(format!(
                    "unexpected frame before ack: {other:?}"
                )));
            }
            Err(e) => return ConnectOutcome::Failed(EnforcerError::Serde(e)),
        },
        Some(Ok(Message::Close(Some(frame))))
            if matches!(
                frame.code,
                tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Library(4401)
            ) =>
        {
            return ConnectOutcome::PatRejected;
        }
        Some(Ok(other)) => {
            return ConnectOutcome::Failed(EnforcerError::ConnectFailed(format!(
                "unexpected pre-ack frame: {other:?}"
            )));
        }
        Some(Err(e)) => return ConnectOutcome::Failed(EnforcerError::ConnectFailed(e.to_string())),
        None => {
            return ConnectOutcome::Failed(EnforcerError::ConnectFailed(
                "stream closed before ack".into(),
            ));
        }
    }

    // subscribe to issueHistoryCreated
    let sub = ClientMsg::Subscribe {
        id: Uuid::now_v7().to_string(),
        payload: SubscribePayload {
            query: ISSUE_HISTORY_CREATED_SUB.to_string(),
            variables: serde_json::json!({}),
        },
    };
    match serde_json::to_string(&sub) {
        Ok(s) => {
            if let Err(e) = sink.send(Message::Text(s)).await {
                return ConnectOutcome::Failed(EnforcerError::ConnectFailed(e.to_string()));
            }
        }
        Err(e) => return ConnectOutcome::Failed(EnforcerError::Serde(e)),
    }

    ConnectOutcome::Ok(sink, reader)
}

/// Parse a `Next` frame into an `IssueChanged`, if it is an issueHistoryCreated.
fn parse_issue_changed(payload: &NextPayload) -> Option<IssueChanged> {
    let node = payload.data.as_object()?.get("issueHistoryCreated")?;
    let id = node.get("issue")?.get("id")?.as_str()?.to_string();
    Some(IssueChanged { linear_issue_id: id })
}

async fn sleep_backoff(idx: &mut usize) {
    let secs = RECONNECT_BACKOFF_SEQ
        .get(*idx)
        .copied()
        .unwrap_or_else(|| *RECONNECT_BACKOFF_SEQ.last().unwrap_or(&30));
    if *idx + 1 < RECONNECT_BACKOFF_SEQ.len() {
        *idx += 1;
    }
    tokio::time::sleep(Duration::from_secs(secs)).await;
}

/// Install the rustls `ring` `CryptoProvider` as the process default, once.
///
/// rustls 0.23 (pulled in by `tokio-tungstenite`'s `rustls-tls-webpki-roots`)
/// compiles in BOTH the `aws-lc-rs` and `ring` providers, so it refuses to
/// auto-select and panics at the first TLS handshake. Installing one explicitly
/// fixes that. Idempotent: `install_default` errors if a provider is already
/// set, which we ignore — any provider being present is the success condition.
fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Run the live subscription forever, forwarding `IssueChanged` events on `tx`.
/// Reconnects with backoff; gives up after `PAT_REJECTION_PERMANENT_THRESHOLD`
/// consecutive auth rejections.
pub async fn run_subscription(token: String, tx: mpsc::Sender<IssueChanged>) {
    ensure_crypto_provider();
    let url = ws_url();
    let mut backoff = 0usize;
    let mut pat_rejections = 0u32;

    loop {
        let (mut sink, mut reader) = match connect_once(&url, &token).await {
            ConnectOutcome::Ok(s, r) => {
                backoff = 0;
                pat_rejections = 0;
                tracing::debug!("linear enforcer: subscription live");
                (s, r)
            }
            ConnectOutcome::PatRejected => {
                pat_rejections += 1;
                tracing::warn!(attempts = pat_rejections, "linear enforcer: PAT rejected");
                if pat_rejections >= PAT_REJECTION_PERMANENT_THRESHOLD {
                    tracing::error!("linear enforcer: PAT permanently rejected; stopping");
                    return;
                }
                sleep_backoff(&mut backoff).await;
                continue;
            }
            ConnectOutcome::Failed(e) => {
                tracing::warn!(error = %e, "linear enforcer: connect failed; retrying");
                sleep_backoff(&mut backoff).await;
                continue;
            }
        };

        // Drain frames until the connection drops.
        while let Some(frame) = reader.next().await {
            match frame {
                Ok(Message::Text(t)) => {
                    let Ok(parsed) = serde_json::from_str::<ServerMsg>(&t) else {
                        tracing::debug!(raw = %t, "linear enforcer: unparsed server frame");
                        continue;
                    };
                    match parsed {
                        ServerMsg::Next { payload, .. } => {
                            if let Some(ev) = parse_issue_changed(&payload) {
                                if tx.send(ev).await.is_err() {
                                    return; // receiver gone
                                }
                            }
                        }
                        ServerMsg::Ping => {
                            if let Ok(p) = serde_json::to_string(&ClientMsg::Pong) {
                                let _ = sink.send(Message::Text(p)).await;
                            }
                        }
                        ServerMsg::Error { id, payload } => {
                            tracing::warn!(?id, ?payload, "linear enforcer: error frame");
                        }
                        ServerMsg::Complete { .. } | ServerMsg::Pong | ServerMsg::ConnectionAck => {}
                    }
                }
                Ok(Message::Ping(p)) => {
                    let _ = sink.send(Message::Pong(p)).await;
                }
                Ok(Message::Close(c)) => {
                    tracing::debug!(close = ?c, "linear enforcer: server CLOSE frame");
                    break;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "linear enforcer: ws read error");
                    break;
                }
                other => {
                    tracing::debug!(?other, "linear enforcer: other ws frame");
                }
            }
        }

        if tx.is_closed() {
            return;
        }
        tracing::debug!("linear enforcer: connection dropped; reconnecting");
        sleep_backoff(&mut backoff).await;
    }
}

// ----- poll source (the reliable realtime feed) ------------------------
//
// Linear's wss `subscribe` op refuses a personal API key with a 4002 close
// even though the same key authenticates REST + the `connection_init` ack —
// this is expected Linear behavior, and legatus-desktop (the reference impl)
// handles it by falling back to REST polling (`LinearPollActor`: "bump to 30s
// when the WS auth is rejected"). Our spine omitted that fallback; this is it.
// It feeds the SAME `IssueChanged` channel, so the whole `run_enforcer`
// pipeline (debounce, team filter, shadow/live, escalation graph) is unchanged.

/// Poll cadence for the realtime feed (matches legatus' WS-rejected fallback).
const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Default poll cursor lower bound when `SENTINEL_LINEAR_POLL_SINCE` is unset.
/// A fixed recent date keeps the first poll bounded (started-state issues only)
/// without this module reading the clock; the cursor then advances live.
const DEFAULT_POLL_SINCE: &str = "2026-06-01T00:00:00Z";

/// Build the GraphQL body for one poll: issues in a *started* state updated at
/// or after `since_rfc3339`, returning just `id` + `identifier` + `updatedAt`.
/// Pure (no I/O) so the filter shape is unit-testable.
#[must_use]
fn poll_query_body(since_rfc3339: &str) -> String {
    let filter = serde_json::json!({
        "updatedAt": { "gte": since_rfc3339 },
        "state": { "type": { "in": ["started"] } }
    });
    serde_json::json!({
        "query": "query($f:IssueFilter){ issues(filter:$f, first:50){ nodes { id identifier updatedAt } } }",
        "variables": { "f": filter }
    })
    .to_string()
}

/// Pick the next cursor: the lexicographically-greatest RFC3339 `updatedAt`
/// across `nodes`, or `prev` if none is newer. (RFC3339 UTC strings sort
/// chronologically, so a string `max` is a correct timestamp `max`.) Pure +
/// unit-testable; keeps the cursor-advance logic out of the I/O loop.
#[must_use]
fn next_cursor(prev: &str, nodes: &[Value]) -> String {
    let mut newest = prev.to_string();
    for n in nodes {
        if let Some(ts) = n.get("updatedAt").and_then(Value::as_str) {
            if ts > newest.as_str() {
                newest = ts.to_string();
            }
        }
    }
    newest
}

/// One poll round: fetch started issues updated since `since`, emit an
/// `IssueChanged` for each, and return the next cursor. Errors are logged and
/// swallowed (the loop keeps polling); returns `None` only when the receiver
/// is gone so the caller can stop.
async fn poll_once(
    client: &reqwest::Client,
    token: &str,
    since_rfc3339: &str,
    tx: &mpsc::Sender<IssueChanged>,
) -> Option<String> {
    let body = poll_query_body(since_rfc3339);
    let resp = client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .ok()?
        .json::<Value>()
        .await
        .ok()?;
    let nodes = resp.get("data")?.get("issues")?.get("nodes")?.as_array()?;
    if !nodes.is_empty() {
        tracing::debug!(count = nodes.len(), since = %since_rfc3339, "linear enforcer: poll round — started issues to check");
    }
    for n in nodes {
        if let Some(id) = n.get("id").and_then(Value::as_str) {
            if tx.send(IssueChanged { linear_issue_id: id.to_string() }).await.is_err() {
                return None; // receiver gone — stop polling
            }
        }
    }
    Some(next_cursor(since_rfc3339, nodes))
}

/// Run the REST poll source forever, forwarding `IssueChanged` events on `tx`.
/// This is the reliable feed that does NOT depend on the wss subscription —
/// the legatus fallback pattern. `cursor` seeds the first `updatedAt` lower
/// bound (the daemon passes a recent timestamp so a fresh start still catches
/// very recent transitions). Stops when the receiver is dropped.
pub async fn run_poll_source(token: String, tx: mpsc::Sender<IssueChanged>, mut cursor: String) {
    let client = reqwest::Client::new();
    tracing::info!(interval_secs = POLL_INTERVAL.as_secs(), "linear enforcer: poll source started");
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    loop {
        ticker.tick().await;
        if tx.is_closed() {
            tracing::warn!("linear enforcer: poll source stopped (receiver gone)");
            return;
        }
        match poll_once(&client, &token, &cursor, &tx).await {
            Some(next) => cursor = next,
            None if tx.is_closed() => return,
            None => tracing::debug!("linear enforcer: poll round failed; retrying next tick"),
        }
    }
}

// ----- Linear REST GraphQL: fetch readiness + enforce ------------------

const LINEAR_GRAPHQL_URL: &str = "https://api.linear.app/graphql";

/// Fetch a ticket's readiness fields by its Linear issue UUID.
///
/// # Errors
/// Returns [`EnforcerError::Api`] on network failure or an unexpected response.
pub async fn fetch_readiness(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
) -> Result<TicketReadiness, EnforcerError> {
    let query = format!(
        r#"{{"query":"query{{issue(id:\"{issue_id}\"){{identifier estimate state{{name type}} labels{{nodes{{name parent{{name}}}}}}}}}}"}}"#
    );
    let resp = client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(query)
        .send()
        .await
        .map_err(|e| EnforcerError::Api(e.to_string()))?;
    let body: Value = resp
        .json()
        .await
        .map_err(|e| EnforcerError::Api(e.to_string()))?;
    let issue = body
        .get("data")
        .and_then(|d| d.get("issue"))
        .ok_or_else(|| EnforcerError::Api(format!("no issue in response: {body}")))?;
    if issue.is_null() {
        return Err(EnforcerError::Api(format!("issue {issue_id} not found")));
    }
    parse_readiness(issue).ok_or_else(|| EnforcerError::Api("malformed issue payload".into()))
}

fn parse_readiness(issue: &Value) -> Option<TicketReadiness> {
    let identifier = issue.get("identifier")?.as_str()?.to_string();
    let estimate = issue.get("estimate").and_then(serde_json::Value::as_f64);
    let state = issue.get("state")?;
    let state_type = state.get("type")?.as_str()?.to_string();
    let state_name = state.get("name")?.as_str()?.to_string();
    let labels = issue.get("labels")?.get("nodes")?.as_array()?;
    let has_parent = |group: &str| {
        labels.iter().any(|l| {
            l.get("parent")
                .and_then(|p| p.get("name"))
                .and_then(serde_json::Value::as_str)
                == Some(group)
        })
    };
    Some(TicketReadiness {
        identifier,
        estimate,
        state_type,
        state_name,
        has_type_label: has_parent("Type"),
        has_area_label: has_parent("Area"),
    })
}

/// The Backlog state id for an issue's team — the enforcer reverts an unready
/// started ticket here. Resolved per team (cached by the caller in practice).
///
/// # Errors
/// Returns [`EnforcerError::Api`] if the team's Backlog state can't be resolved.
async fn backlog_state_id(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
) -> Result<String, EnforcerError> {
    let query = format!(
        r#"{{"query":"query{{issue(id:\"{issue_id}\"){{team{{states{{nodes{{id name type}}}}}}}}}}"}}"#
    );
    let body: Value = client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(query)
        .send()
        .await
        .map_err(|e| EnforcerError::Api(e.to_string()))?
        .json()
        .await
        .map_err(|e| EnforcerError::Api(e.to_string()))?;
    let nodes = body
        .get("data")
        .and_then(|d| d.get("issue"))
        .and_then(|i| i.get("team"))
        .and_then(|t| t.get("states"))
        .and_then(|s| s.get("nodes"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| EnforcerError::Api("no states in team".into()))?;
    nodes
        .iter()
        .find(|n| n.get("type").and_then(serde_json::Value::as_str) == Some("backlog"))
        .and_then(|n| n.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| EnforcerError::Api("no backlog state for team".into()))
}

/// Enforce on one ticket: if it has STARTED while not dev-ready, revert it to
/// Backlog and post a comment listing the missing fields. No-op otherwise.
///
/// Returns `true` if an enforcement action (revert) was taken.
///
/// # Errors
/// Returns [`EnforcerError`] on any Linear API failure.
pub async fn enforce_ticket(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
) -> Result<bool, EnforcerError> {
    let r = fetch_readiness(client, token, issue_id).await?;
    if !r.should_revert() {
        return Ok(false);
    }
    let missing = r.missing().join(", ");
    tracing::warn!(ticket = %r.identifier, %missing, "linear enforcer: reverting un-ready started ticket");

    let backlog = backlog_state_id(client, token, issue_id).await?;
    // Revert state.
    let mutation = format!(
        r#"{{"query":"mutation{{issueUpdate(id:\"{issue_id}\",input:{{stateId:\"{backlog}\"}}){{success}}}}"}}"#
    );
    client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(mutation)
        .send()
        .await
        .map_err(|e| EnforcerError::Api(e.to_string()))?;

    // Comment listing the gap + the fix path.
    let comment_body = format!(
        "## 🚧 Reverted — ticket not dev-ready (bad PM is bad software)\\n\\n\
         This ticket entered a started state ({state}) while missing: **{missing}**. \
         The real-time enforcer moved it back to Backlog.\\n\\n\
         To start work: add the missing fields (estimate + Type & Area labels, out of Triage), \
         then move it forward. This applies to every ticket — UI or API.",
        state = r.state_name,
        missing = missing,
    );
    let comment_mutation = serde_json::json!({
        "query": "mutation($id:String!,$b:String!){commentCreate(input:{issueId:$id,body:$b}){success}}",
        "variables": { "id": issue_id, "b": comment_body }
    });
    client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .json(&comment_mutation)
        .send()
        .await
        .map_err(|e| EnforcerError::Api(e.to_string()))?;

    Ok(true)
}

// ----- the driver: wire the live subscription to the enforcer -----------

/// How the enforcer acts on an un-ready started ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnforcerMode {
    /// Observe and log what *would* be reverted, but never mutate Linear.
    /// The conservative default — ship this, watch the logs, then arm `Live`.
    #[default]
    Shadow,
    /// Actually revert un-ready started tickets and post the comment.
    Live,
}

/// Per-issue cooldown: a burst of `issueHistoryCreated` events on one ticket
/// (Linear fires several per edit) collapses to a single enforcement check.
const DEBOUNCE: Duration = Duration::from_secs(20);

/// Runtime configuration for the enforcer driver.
#[derive(Debug, Clone)]
pub struct EnforcerConfig {
    /// Linear PAT (drives both the wss subscription and the REST calls).
    pub token: String,
    /// Shadow (log-only) or Live (mutate). Defaults to Shadow.
    pub mode: EnforcerMode,
    /// If set, only tickets whose identifier prefix matches one of these teams
    /// are enforced (e.g. `["FPCRM", "FPROUTE"]`). `None` = all teams.
    pub team_filter: Option<Vec<String>>,
}

impl EnforcerConfig {
    /// Does `identifier` (e.g. `FPCRM-123`) pass the team filter?
    fn team_allowed(&self, identifier: &str) -> bool {
        match &self.team_filter {
            None => true,
            Some(teams) => {
                let prefix = identifier.split('-').next().unwrap_or(identifier);
                teams.iter().any(|t| t.eq_ignore_ascii_case(prefix))
            }
        }
    }

    /// Build a config from the process environment, or `None` (enforcer
    /// disabled) when no `SENTINEL_LINEAR_TOKEN` is set. The daemon calls this at
    /// startup and only spawns [`run_enforcer`] when it returns `Some`.
    ///
    /// - `SENTINEL_LINEAR_TOKEN` — the PAT. Absent/empty ⇒ `None` (disabled).
    /// - `SENTINEL_LINEAR_ENFORCE` — `live` arms mutation; anything else
    ///   (including unset) stays in the safe `Shadow` default.
    /// - `SENTINEL_LINEAR_TEAMS` — comma-separated team prefixes (e.g.
    ///   `FPCRM,FPROUTE`). Empty/unset ⇒ all teams.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from_raw(
            std::env::var("SENTINEL_LINEAR_TOKEN").ok(),
            std::env::var("SENTINEL_LINEAR_ENFORCE").ok(),
            std::env::var("SENTINEL_LINEAR_TEAMS").ok(),
        )
    }

    /// Pure config-builder shared by [`from_env`](Self::from_env) — takes the
    /// three raw values so it is testable without mutating process env (the
    /// workspace forbids `unsafe`, which `std::env::set_var` now requires).
    #[must_use]
    fn from_raw(
        token: Option<String>,
        enforce: Option<String>,
        teams: Option<String>,
    ) -> Option<Self> {
        let token = token?;
        if token.trim().is_empty() {
            return None;
        }
        let mode = match enforce {
            Some(v) if v.eq_ignore_ascii_case("live") => EnforcerMode::Live,
            _ => EnforcerMode::Shadow,
        };
        let team_filter = teams.and_then(|v| {
            let teams: Vec<String> = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            (!teams.is_empty()).then_some(teams)
        });
        Some(Self { token, mode, team_filter })
    }
}

/// Has `issue_id` been seen within the debounce window? Records the time if not.
/// Returns `true` when the event should be SKIPPED (still cooling down).
fn debounced(
    seen: &mut std::collections::HashMap<String, std::time::Instant>,
    issue_id: &str,
) -> bool {
    let now = std::time::Instant::now();
    if let Some(prev) = seen.get(issue_id) {
        if now.duration_since(*prev) < DEBOUNCE {
            return true;
        }
    }
    seen.insert(issue_id.to_string(), now);
    false
}

/// Run the full enforcement spine: hold the live subscription and enforce each
/// issue-change event according to `cfg.mode`. Runs until the subscription gives
/// up permanently (PAT rejected) or the receiver is dropped.
///
/// In `Shadow` mode it fetches readiness and logs what it *would* revert, with
/// zero mutations. In `Live` mode an un-ready started ticket is routed through
/// the `LangGraph` escalation graph ([`crate::enforcement_graph`]): an adversarial
/// `Codex` judge must CONFIRM before the strict revert + comment fires. If the
/// `OPENROUTER_API_KEY` isn't set, Live falls back to the direct
/// [`enforce_ticket`] path (judge unavailable ⇒ readiness check alone).
pub async fn run_enforcer(cfg: EnforcerConfig) {
    let (tx, mut rx) = mpsc::channel::<IssueChanged>(256);
    // Two feeds into the one channel: the wss subscription (optimization — may
    // 4002 on a PAT) and the REST poll source (the reliable backstop, the
    // legatus-desktop pattern). The poll cursor seeds from `SENTINEL_LINEAR_POLL_SINCE`
    // if set (RFC3339), else from a recent fixed lower bound so a fresh daemon
    // still catches current transitions without replaying all history.
    tokio::spawn(run_subscription(cfg.token.clone(), tx.clone()));
    let poll_cursor = std::env::var("SENTINEL_LINEAR_POLL_SINCE")
        .unwrap_or_else(|_| DEFAULT_POLL_SINCE.to_string());
    tokio::spawn(run_poll_source(cfg.token.clone(), tx, poll_cursor));

    let client = reqwest::Client::new();
    let mut seen: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();

    // Build the escalation graph + LLM judge once, for Live mode. Either being
    // unavailable degrades Live to the direct readiness-only revert path.
    let (llm, graph) = if matches!(cfg.mode, EnforcerMode::Live) {
        let llm = crate::openrouter_llm::OpenRouterLlm::from_env().ok();
        let graph = match crate::enforcement_graph::build_escalation_graph().await {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!(error = %e, "linear enforcer: escalation graph unavailable; Live uses direct revert");
                None
            }
        };
        (llm, graph)
    } else {
        (None, None)
    };

    tracing::info!(mode = ?cfg.mode, judge = llm.is_some(), graph = graph.is_some(), "linear enforcer: driver started");

    while let Some(ev) = rx.recv().await {
        let id = &ev.linear_issue_id;
        if debounced(&mut seen, id) {
            continue;
        }

        match cfg.mode {
            EnforcerMode::Shadow => match fetch_readiness(&client, &cfg.token, id).await {
                Ok(r) => {
                    if cfg.team_allowed(&r.identifier) && r.should_revert() {
                        tracing::warn!(
                            ticket = %r.identifier,
                            missing = %r.missing().join(", "),
                            state = %r.state_name,
                            "linear enforcer [SHADOW]: WOULD revert un-ready started ticket"
                        );
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, issue = %id, "linear enforcer [SHADOW]: readiness fetch failed");
                }
            },
            EnforcerMode::Live => {
                // Fetch once to learn the identifier + readiness, and gate on team.
                let readiness = match fetch_readiness(&client, &cfg.token, id).await {
                    Ok(r) if !cfg.team_allowed(&r.identifier) => continue,
                    Ok(r) => r,
                    Err(e) => {
                        tracing::debug!(error = %e, issue = %id, "linear enforcer [LIVE]: pre-check fetch failed");
                        continue;
                    }
                };

                // Preferred path: route through the LangGraph escalation graph
                // so an adversarial judge confirms before a strict revert.
                if let (Some(llm), Some(graph)) = (llm.as_ref(), graph.as_ref()) {
                    match crate::enforcement_graph::evaluate_ticket(llm, graph, &readiness).await {
                        Ok(state)
                            if state.decision == crate::enforcement_graph::Decision::Revert =>
                        {
                            match enforce_ticket(&client, &cfg.token, id).await {
                                Ok(_) => tracing::info!(ticket = %readiness.identifier, "linear enforcer [LIVE]: reverted (judge-confirmed)"),
                                Err(e) => tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: revert failed"),
                            }
                        }
                        Ok(_) => { /* Clear — judge refuted or ticket ready; no action. */ }
                        Err(e) => {
                            tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: escalation graph failed");
                        }
                    }
                } else if readiness.should_revert() {
                    // Fallback (no judge/graph): direct readiness-only revert.
                    match enforce_ticket(&client, &cfg.token, id).await {
                        Ok(true) => tracing::info!(ticket = %readiness.identifier, "linear enforcer [LIVE]: reverted (readiness-only)"),
                        Ok(false) => {}
                        Err(e) => tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: enforcement failed"),
                    }
                }
            }
        }
    }

    tracing::warn!("linear enforcer: driver stopped (subscription ended)");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(state_type: &str) -> TicketReadiness {
        TicketReadiness {
            identifier: "FPCRM-1".into(),
            estimate: Some(3.0),
            state_type: state_type.into(),
            state_name: "x".into(),
            has_type_label: true,
            has_area_label: true,
        }
    }

    #[test]
    fn dev_ready_started_is_not_reverted() {
        assert!(!ready("started").should_revert());
    }

    #[test]
    fn unready_started_is_reverted() {
        let mut t = ready("started");
        t.estimate = None;
        assert!(t.should_revert(), "started + no estimate must revert");
        assert!(t.missing().contains(&"estimate"));
    }

    #[test]
    fn unready_but_backlog_is_not_reverted() {
        // A not-ready ticket sitting in Backlog is fine — it hasn't STARTED.
        let mut t = ready("backlog");
        t.estimate = None;
        t.has_type_label = false;
        assert!(!t.should_revert(), "backlog work isn't enforced until it starts");
    }

    #[test]
    fn started_missing_labels_reverted() {
        let mut t = ready("started");
        t.has_type_label = false;
        t.has_area_label = false;
        assert!(t.should_revert());
        let m = t.missing();
        assert!(m.contains(&"Type label") && m.contains(&"Area label"));
    }

    #[test]
    fn triage_is_flagged() {
        let t = ready("triage");
        // triage is not "started", so should_revert is false, but missing() flags it.
        assert!(t.missing().contains(&"still in Triage"));
    }

    #[test]
    fn parse_issue_changed_extracts_id() {
        let payload = NextPayload {
            data: serde_json::json!({
                "issueHistoryCreated": { "id": "h1", "issue": { "id": "issue-uuid-123" } }
            }),
        };
        let ev = parse_issue_changed(&payload).expect("must parse");
        assert_eq!(ev.linear_issue_id, "issue-uuid-123");
    }

    #[test]
    fn parse_ignores_unrelated_payload() {
        let payload = NextPayload {
            data: serde_json::json!({ "commentCreated": { "id": "c1" } }),
        };
        assert!(parse_issue_changed(&payload).is_none());
    }

    #[test]
    fn build_request_sets_subprotocol() {
        let req = build_request("wss://api.linear.app/graphql").unwrap();
        let proto = req.headers().get(SEC_WEBSOCKET_PROTOCOL).unwrap();
        assert_eq!(proto, "graphql-transport-ws");
    }

    // ----- driver -------------------------------------------------------

    fn cfg(team_filter: Option<Vec<String>>) -> EnforcerConfig {
        EnforcerConfig {
            token: "tok".into(),
            mode: EnforcerMode::default(),
            team_filter,
        }
    }

    #[test]
    fn mode_defaults_to_shadow() {
        // The conservative default — live mutation must be opt-in.
        assert_eq!(EnforcerMode::default(), EnforcerMode::Shadow);
        assert_eq!(cfg(None).mode, EnforcerMode::Shadow);
    }

    #[test]
    fn team_filter_none_allows_all() {
        assert!(cfg(None).team_allowed("FPCRM-123"));
        assert!(cfg(None).team_allowed("ANYTHING-1"));
    }

    #[test]
    fn team_filter_matches_prefix_case_insensitive() {
        let c = cfg(Some(vec!["FPCRM".into(), "FPROUTE".into()]));
        assert!(c.team_allowed("FPCRM-7"));
        assert!(c.team_allowed("fproute-42"), "match is case-insensitive");
        assert!(!c.team_allowed("ATLUS-9"), "off-list team is rejected");
        assert!(!c.team_allowed("FPCRMX-1"), "prefix must be exact, not a substring");
    }

    #[test]
    fn from_raw_disabled_without_token() {
        // No token (or blank) ⇒ enforcer disabled (None), regardless of others.
        assert!(EnforcerConfig::from_raw(None, Some("live".into()), None).is_none());
        assert!(EnforcerConfig::from_raw(Some("  ".into()), None, None).is_none());
    }

    #[test]
    fn from_raw_parses_token_mode_and_teams() {
        let c = EnforcerConfig::from_raw(
            Some("pat_xyz".into()),
            Some("LIVE".into()),
            Some("FPCRM, FPROUTE ,".into()),
        )
        .expect("token present ⇒ Some");
        assert_eq!(c.token, "pat_xyz");
        assert_eq!(c.mode, EnforcerMode::Live, "ENFORCE=LIVE (any case) arms live");
        assert_eq!(
            c.team_filter,
            Some(vec!["FPCRM".to_string(), "FPROUTE".to_string()]),
            "teams trimmed, empties dropped"
        );
    }

    #[test]
    fn from_raw_defaults_to_shadow_and_no_filter() {
        // ENFORCE unset ⇒ safe Shadow; TEAMS unset ⇒ all teams.
        let c = EnforcerConfig::from_raw(Some("pat".into()), None, None).unwrap();
        assert_eq!(c.mode, EnforcerMode::Shadow, "default is safe Shadow");
        assert_eq!(c.team_filter, None, "no TEAMS ⇒ all teams");
        // Any non-"live" value also stays Shadow.
        let c2 =
            EnforcerConfig::from_raw(Some("pat".into()), Some("on".into()), Some(" , ".into()))
                .unwrap();
        assert_eq!(c2.mode, EnforcerMode::Shadow, "ENFORCE=on is not live");
        assert_eq!(c2.team_filter, None, "all-empty TEAMS ⇒ no filter");
    }

    #[test]
    fn debounce_skips_within_window_then_allows() {
        let mut seen = std::collections::HashMap::new();
        // First sighting is allowed (records the time).
        assert!(!debounced(&mut seen, "issue-1"));
        // Immediate repeat is skipped (still cooling down).
        assert!(debounced(&mut seen, "issue-1"));
        // A different issue is independent — allowed.
        assert!(!debounced(&mut seen, "issue-2"));
        // Force the window to have elapsed for issue-1, then it's allowed again.
        seen.insert(
            "issue-1".to_string(),
            std::time::Instant::now() - (DEBOUNCE + Duration::from_secs(1)),
        );
        assert!(!debounced(&mut seen, "issue-1"), "allowed again after the window");
    }

    // ----- poll source --------------------------------------------------

    #[test]
    fn poll_query_filters_started_and_since() {
        let body = poll_query_body("2026-06-01T00:00:00Z");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let f = &v["variables"]["f"];
        assert_eq!(f["updatedAt"]["gte"], "2026-06-01T00:00:00Z");
        assert_eq!(f["state"]["type"]["in"][0], "started");
        assert!(v["query"].as_str().unwrap().contains("issues(filter:$f"));
    }

    #[test]
    fn next_cursor_picks_newest_updatedat() {
        let nodes = vec![
            serde_json::json!({ "id": "a", "updatedAt": "2026-06-09T10:00:00Z" }),
            serde_json::json!({ "id": "b", "updatedAt": "2026-06-09T12:00:00Z" }), // newest
            serde_json::json!({ "id": "c", "updatedAt": "2026-06-09T11:00:00Z" }),
        ];
        assert_eq!(next_cursor("2026-06-01T00:00:00Z", &nodes), "2026-06-09T12:00:00Z");
    }

    #[test]
    fn next_cursor_keeps_prev_when_nothing_newer() {
        // Empty round, or all older than the cursor → cursor unchanged.
        assert_eq!(next_cursor("2026-06-09T12:00:00Z", &[]), "2026-06-09T12:00:00Z");
        let older = vec![serde_json::json!({ "id": "a", "updatedAt": "2026-06-08T00:00:00Z" })];
        assert_eq!(next_cursor("2026-06-09T12:00:00Z", &older), "2026-06-09T12:00:00Z");
    }
}
