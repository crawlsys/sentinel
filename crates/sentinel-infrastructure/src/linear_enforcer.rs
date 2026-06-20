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
    ConnectionInit {
        payload: Value,
    },
    Subscribe {
        id: String,
        payload: SubscribePayload,
    },
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
    Error {
        id: String,
        payload: Value,
    },
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
// Each bool is an independent, named readiness/discipline dimension read
// straight off the Linear payload — a flat record is the clearest shape; a
// bitfield/sub-struct would obscure them for no benefit.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct TicketReadiness {
    pub identifier: String,
    pub estimate: Option<f64>,
    pub state_type: String,
    pub state_name: String,
    pub has_type_label: bool,
    pub has_area_label: bool,
    /// Actor attribution (for the agent-vs-human differential). `true` when the
    /// ticket was created by a bot/integration (`botActor` present or
    /// `integrationSourceType` set) — i.e. an agent's own ticket, safe to heal
    /// silently. `false` for human-created tickets, where we escalate via
    /// comment instead of silently rewriting the operator's ticket.
    pub created_by_agent: bool,
    /// Linear priority: 0=none, 1=urgent, 2=high, 3=medium, 4=low.
    pub priority: Option<i64>,
    /// RFC3339 timestamp the SLA clock started (None ⇒ no SLA running).
    pub sla_started_at: Option<String>,
    /// RFC3339 timestamp the SLA breaches at (None ⇒ no SLA running).
    pub sla_breaches_at: Option<String>,
    /// ≥3 testable acceptance criteria — heuristically, the description carries
    /// at least 3 checklist/numbered lines. A dev-ready dimension (heal can
    /// draft AC), so it joins `missing()`.
    pub has_acceptance_criteria: bool,
    /// A human/agent is assigned. Extended-discipline check (comment-only):
    /// a started ticket with no assignee is flagged, not auto-filled.
    pub assignee_present: bool,
    /// Ticket is in a cycle. Extended-discipline (comment-only).
    pub in_cycle: bool,
    /// Ticket is attached to a project. Extended-discipline (comment-only).
    pub in_project: bool,
    /// Has a LIVE blocker: an inbound `blocks` relation whose blocking issue is
    /// not completed/canceled. A started ticket that's actively blocked is a
    /// workflow smell (work can't really proceed) — extended-discipline flag.
    pub actively_blocked: bool,
    /// Linear's NATIVE SLA high-risk threshold (`slaHighRiskAt`, RFC3339) — when
    /// present, `evaluate_sla` uses it instead of the 80%-elapsed heuristic so
    /// the warning fires exactly when Linear's own SLA config says it should.
    pub sla_high_risk_at: Option<String>,
    /// Issue due date (`dueDate`, "YYYY-MM-DD"). Distinct from SLA — an open
    /// ticket past its due date is flagged (extended-discipline).
    pub due_date: Option<String>,
    /// Attached to a project milestone (`projectMilestone`). Extended-discipline:
    /// a ticket in a project but with no milestone is a planning gap.
    pub has_milestone: bool,
}

/// Outcome of the SLA health check (Tier-3 SLA discipline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlaVerdict {
    /// No SLA concern.
    Ok,
    /// SLA clock running and ≥80% elapsed but not yet breached.
    HighRisk,
    /// SLA deadline has passed.
    Breached,
    /// Urgent (priority 1) ticket with no SLA clock running at all.
    UrgentNoSla,
}

/// Pure SLA evaluation — `now` is injected so this stays testable and free of
/// `Utc::now()` in domain-ish logic. Prefers Linear's NATIVE `slaHighRiskAt`
/// threshold when present (fires `HighRisk` once `now >= slaHighRiskAt`);
/// otherwise applies Sentinel's ≥80%-elapsed high-risk policy.
#[must_use]
pub fn evaluate_sla(
    priority: Option<i64>,
    sla_started_at: Option<&str>,
    sla_breaches_at: Option<&str>,
    sla_high_risk_at: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> SlaVerdict {
    use chrono::DateTime;
    let parse = |s: &str| {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&chrono::Utc))
    };
    match sla_breaches_at {
        Some(breach_s) => {
            let Some(breach) = parse(breach_s) else {
                return SlaVerdict::Ok;
            };
            if now >= breach {
                return SlaVerdict::Breached;
            }
            // Prefer Linear's native high-risk threshold; else Sentinel's 80% policy.
            if let Some(hr) = sla_high_risk_at.and_then(parse) {
                if now >= hr {
                    return SlaVerdict::HighRisk;
                }
            } else if let Some(start) = sla_started_at.and_then(parse) {
                let total = (breach - start).num_seconds();
                if total > 0 {
                    let elapsed = (now - start).num_seconds().max(0);
                    if elapsed * 100 >= total * 80 {
                        return SlaVerdict::HighRisk;
                    }
                }
            }
            SlaVerdict::Ok
        }
        // No SLA clock at all — only a problem if the ticket is Urgent.
        None if priority == Some(1) => SlaVerdict::UrgentNoSla,
        None => SlaVerdict::Ok,
    }
}

/// Is this issue past its `dueDate`? `due_date` is a plain `YYYY-MM-DD`; overdue
/// when that date is strictly before `today`. Pure + testable.
#[must_use]
pub fn is_overdue(due_date: Option<&str>, today: chrono::NaiveDate) -> bool {
    due_date
        .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .is_some_and(|d| d < today)
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
        if !self.has_acceptance_criteria {
            m.push("acceptance criteria");
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

    /// Extended workflow-discipline gaps for a STARTED ticket — softer than the
    /// dev-ready set (these are flagged via comment, never auto-filled or
    /// reverted): no assignee, not in a cycle, no project. Empty unless started.
    #[must_use]
    pub fn extended_discipline(&self) -> Vec<&'static str> {
        let mut d = Vec::new();
        if !self.is_started() {
            return d;
        }
        if !self.assignee_present {
            d.push("no assignee");
        }
        if !self.in_cycle {
            d.push("not in a cycle");
        }
        if !self.in_project {
            d.push("no project");
        }
        if self.in_project && !self.has_milestone {
            d.push("no project milestone");
        }
        if self.actively_blocked {
            d.push("started but actively blocked");
        }
        d
    }
}

/// Does this issue have a LIVE blocker? Scans `inverseRelations` for a `blocks`
/// relation whose blocking issue's state is not completed/canceled. Pure +
/// testable; tolerant of missing/partial JSON.
#[must_use]
pub fn has_live_blocker(issue: &Value) -> bool {
    let Some(nodes) = issue
        .get("inverseRelations")
        .and_then(|r| r.get("nodes"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    nodes.iter().any(|n| {
        n.get("type").and_then(Value::as_str) == Some("blocks")
            && !matches!(
                n.get("issue")
                    .and_then(|i| i.get("state"))
                    .and_then(|s| s.get("type"))
                    .and_then(Value::as_str),
                Some("completed" | "canceled")
            )
    })
}

/// Heuristic: does the description carry ≥3 acceptance-criteria lines? Counts
/// checklist (`- [ ]` / `- [x]`) and numbered (`1.`, `2)`) list items. Pure +
/// testable.
#[must_use]
pub fn has_acceptance_criteria(description: &str) -> bool {
    let count = description
        .lines()
        .map(str::trim_start)
        .filter(|l| {
            l.starts_with("- [")
                || l.starts_with("* [")
                || l.chars().next().is_some_and(|c| c.is_ascii_digit())
                    && (l.contains(". ") || l.contains(") "))
        })
        .count();
    count >= 3
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

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

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
    Some(IssueChanged {
        linear_issue_id: id,
    })
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
                        ServerMsg::Complete { .. } | ServerMsg::Pong | ServerMsg::ConnectionAck => {
                        }
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
// this is expected Linear behavior, so Sentinel runs REST polling as a required
// realtime feed. It writes into the same `IssueChanged` channel, so the
// graph-backed `run_enforcer` pipeline stays single-path after ingestion.

/// Poll cadence for the REST realtime feed.
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
            if tx
                .send(IssueChanged {
                    linear_issue_id: id.to_string(),
                })
                .await
                .is_err()
            {
                return None; // receiver gone — stop polling
            }
        }
    }
    Some(next_cursor(since_rfc3339, nodes))
}

/// Run the REST poll source forever, forwarding `IssueChanged` events on `tx`.
/// This is the reliable feed that does NOT depend on the wss subscription —
/// `cursor` seeds the first `updatedAt` lower
/// bound (the daemon passes a recent timestamp so a fresh start still catches
/// very recent transitions). Stops when the receiver is dropped.
pub async fn run_poll_source(token: String, tx: mpsc::Sender<IssueChanged>, mut cursor: String) {
    let client = reqwest::Client::new();
    tracing::info!(
        interval_secs = POLL_INTERVAL.as_secs(),
        "linear enforcer: poll source started"
    );
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
        r#"{{"query":"query{{issue(id:\"{issue_id}\"){{identifier estimate priority slaStartedAt slaBreachesAt slaHighRiskAt dueDate description state{{name type}} labels{{nodes{{name parent{{name}}}}}} botActor{{id}} integrationSourceType assignee{{id}} cycle{{id}} project{{id}} projectMilestone{{id}} inverseRelations{{nodes{{type issue{{state{{type}}}}}}}}}}}}"}}"#
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
    // Agent-vs-human attribution: a bot actor OR a non-null integration source
    // means an agent/integration created this ticket — safe to heal silently.
    // Absent both ⇒ human-authored ⇒ escalate via comment instead.
    let created_by_agent = !issue.get("botActor").unwrap_or(&Value::Null).is_null()
        || !issue
            .get("integrationSourceType")
            .unwrap_or(&Value::Null)
            .is_null();
    let priority = issue.get("priority").and_then(serde_json::Value::as_i64);
    let sla_started_at = issue
        .get("slaStartedAt")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let sla_breaches_at = issue
        .get("slaBreachesAt")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let sla_high_risk_at = issue
        .get("slaHighRiskAt")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let due_date = issue
        .get("dueDate")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let description = issue
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let present = |k: &str| !issue.get(k).unwrap_or(&Value::Null).is_null();
    Some(TicketReadiness {
        identifier,
        estimate,
        state_type,
        state_name,
        has_type_label: has_parent("Type"),
        has_area_label: has_parent("Area"),
        created_by_agent,
        priority,
        sla_started_at,
        sla_breaches_at,
        has_acceptance_criteria: has_acceptance_criteria(description),
        assignee_present: present("assignee"),
        in_cycle: present("cycle"),
        in_project: present("project"),
        actively_blocked: has_live_blocker(issue),
        sla_high_risk_at,
        due_date,
        has_milestone: present("projectMilestone"),
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

/// Enforce on one ticket through the durable LangGraph escalation authority.
/// If the graph decides `Revert`, move it to Backlog and post a comment listing
/// the missing fields. No-op otherwise.
///
/// Returns `true` if an enforcement action (revert) was taken.
///
/// # Errors
/// Returns [`EnforcerError`] on Linear API failure or when the graph authority
/// cannot be constructed. This public helper intentionally has no direct
/// readiness-only mutation path.
pub async fn enforce_ticket(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
) -> Result<bool, EnforcerError> {
    let readiness = fetch_readiness(client, token, issue_id).await?;
    let llm = crate::openrouter_llm::OpenRouterLlm::from_env()
        .map_err(|e| EnforcerError::Api(format!("graph authority unavailable: {e}")))?;
    let graph = crate::enforcement_graph::build_escalation_graph()
        .await
        .map_err(|e| EnforcerError::Api(format!("escalation graph unavailable: {e}")))?;
    let comment_graph = crate::linear_comment_graph::build_linear_comment_graph()
        .await
        .map_err(|e| EnforcerError::Api(format!("comment graph unavailable: {e}")))?;
    let run = crate::enforcement_graph::evaluate_ticket_report(&llm, &graph, &readiness)
        .await
        .map_err(|e| EnforcerError::Api(format!("escalation graph decision failed: {e}")))?;
    if run.state.decision != crate::enforcement_graph::Decision::Revert {
        return Ok(false);
    }
    let enforcement_authorization = run
        .revert_authorization()
        .map_err(|e| EnforcerError::Api(format!("enforcement graph authorization failed: {e}")))?
        .ok_or_else(|| EnforcerError::Api("enforcement graph did not authorize revert".into()))?;
    let revert_authorization =
        LinearRevertAuthorization::from_enforcement(&enforcement_authorization);

    let audit_note = graph_checkpoint_note(&enforcement_authorization.checkpoint_ref(), &[]);
    enforce_ticket_with_graph_audit(
        client,
        token,
        issue_id,
        &audit_note,
        &comment_graph,
        &revert_authorization,
    )
    .await
}

struct LinearRevertAuthorization {
    identifier: String,
    checkpoint_refs: Vec<String>,
}

impl LinearRevertAuthorization {
    fn from_enforcement(
        enforcement: &crate::enforcement_graph::EnforcementRevertAuthorization,
    ) -> Self {
        Self {
            identifier: enforcement.identifier().to_string(),
            checkpoint_refs: vec![enforcement.checkpoint_ref()],
        }
    }

    fn from_enforcement_and_remediation(
        enforcement: &crate::enforcement_graph::EnforcementRevertAuthorization,
        remediation: &crate::remediation::RemediationRevertAuthorization,
    ) -> Result<Self, EnforcerError> {
        if enforcement.identifier() != remediation.identifier() {
            return Err(EnforcerError::Api(format!(
                "revert authorization identifier mismatch: enforcement={} remediation={}",
                enforcement.identifier(),
                remediation.identifier()
            )));
        }
        Ok(Self {
            identifier: enforcement.identifier().to_string(),
            checkpoint_refs: vec![enforcement.checkpoint_ref(), remediation.checkpoint_ref()],
        })
    }

    fn verify(
        &self,
        readiness_identifier: &str,
        graph_audit_note: &str,
    ) -> Result<(), EnforcerError> {
        if self.identifier != readiness_identifier {
            return Err(EnforcerError::Api(format!(
                "revert authorization identifier mismatch: authorization={} readiness={readiness_identifier}",
                self.identifier
            )));
        }
        for checkpoint in &self.checkpoint_refs {
            if !graph_audit_note.contains(checkpoint) {
                return Err(EnforcerError::Api(format!(
                    "revert authorization checkpoint missing from audit note: {checkpoint}"
                )));
            }
        }
        Ok(())
    }

    fn checkpoint_refs(&self) -> Vec<String> {
        self.checkpoint_refs.clone()
    }
}

async fn enforce_ticket_with_graph_audit(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
    graph_audit_note: &str,
    comment_graph: &crate::linear_comment_graph::LinearCommentGraph,
    authorization: &LinearRevertAuthorization,
) -> Result<bool, EnforcerError> {
    let r = fetch_readiness(client, token, issue_id).await?;
    authorization.verify(&r.identifier, graph_audit_note)?;
    if !r.should_revert() {
        return Ok(false);
    }
    let missing = r.missing().join(", ");
    tracing::warn!(ticket = %r.identifier, %missing, "linear enforcer: reverting un-ready started ticket");

    let backlog = backlog_state_id(client, token, issue_id).await?;
    // Revert state.
    let mutation = serde_json::json!({
        "query": "mutation($id:String!,$state:String!){issueUpdate(id:$id,input:{stateId:$state}){success}}",
        "variables": { "id": issue_id, "state": backlog }
    })
    .to_string();
    post_linear_success(client, token, mutation, "issueUpdate").await?;

    // Comment listing the gap + the fix path.
    let mut comment_body = format!(
        "## 🚧 Reverted — ticket not dev-ready (bad PM is bad software)\\n\\n\
         This ticket entered a started state ({state}) while missing: **{missing}**. \
         The real-time enforcer moved it back to Backlog.\\n\\n\
         To start work: add the missing fields (estimate + Type & Area labels, out of Triage), \
         then move it forward. This applies to every ticket — UI or API.",
        state = r.state_name,
        missing = missing,
    );
    comment_body.push_str(graph_audit_note);
    post_live_graph_comment(
        client,
        token,
        comment_graph,
        issue_id,
        &r.identifier,
        "revert_audit",
        comment_body,
        authorization.checkpoint_refs(),
    )
    .await?;

    Ok(true)
}

async fn post_linear_success(
    client: &reqwest::Client,
    token: &str,
    body: String,
    op: &str,
) -> Result<(), EnforcerError> {
    let response = client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| EnforcerError::Api(format!("{op} request failed: {e}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| EnforcerError::Api(format!("read {op} response: {e}")))?;
    if !status.is_success() {
        return Err(EnforcerError::Api(format!(
            "{op} returned HTTP {status}: {body}"
        )));
    }
    validate_linear_success(op, &body)
}

fn validate_linear_success(op: &str, body: &str) -> Result<(), EnforcerError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| EnforcerError::Api(format!("parse {op} response: {e}: {body}")))?;
    if let Some(errors) = value.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            return Err(EnforcerError::Api(format!(
                "{op} returned GraphQL errors: {errors:?}"
            )));
        }
    }
    let ok = value
        .get("data")
        .and_then(|data| data.get(op))
        .and_then(|operation| operation.get("success"))
        .and_then(Value::as_bool)
        == Some(true);
    if !ok {
        return Err(EnforcerError::Api(format!(
            "{op} omitted success=true: {value}"
        )));
    }
    Ok(())
}

fn graph_checkpoint_note(
    enforcement_checkpoint_ref: &str,
    remediation_checkpoints: &[(&str, &str)],
) -> String {
    let mut note = format!("\n\nLangGraph checkpoints: enforcement `{enforcement_checkpoint_ref}`");
    for (label, checkpoint_ref) in remediation_checkpoints {
        note.push_str(", ");
        note.push_str(label);
        note.push_str(" `");
        note.push_str(checkpoint_ref);
        note.push('`');
    }
    note
}

fn terminal_decision_checkpoint_ref<S: Serialize>(
    graph_name: &str,
    thread_id: &str,
    terminal_state: &S,
    checkpoints: &[crate::decision_graph_introspection::DecisionGraphCheckpointSnapshot<S>],
    write_history: &[crate::decision_graph_introspection::DecisionGraphWriteHistoryEntry],
) -> Result<String, EnforcerError> {
    crate::decision_graph_introspection::terminal_decision_checkpoint_result(
        graph_name,
        thread_id,
        terminal_state,
        checkpoints,
        write_history,
    )
    .map(crate::decision_graph_introspection::DecisionGraphCheckpointSnapshot::checkpoint_ref)
    .map_err(|err| {
        EnforcerError::Api(format!(
            "{graph_name} LangGraph authorization failed: {err}"
        ))
    })
}

fn linear_enforcer_graph_audit_path() -> std::path::PathBuf {
    crate::paths::sentinel_root()
        .join("metrics")
        .join("linear-enforcer-graph-runs.jsonl")
}

fn append_linear_enforcer_graph_audit<S, R>(
    graph_name: &str,
    stage: &str,
    identifier: &str,
    thread_id: &str,
    terminal_state: &S,
    checkpoints: &[crate::decision_graph_introspection::DecisionGraphCheckpointSnapshot<S>],
    write_history: &[crate::decision_graph_introspection::DecisionGraphWriteHistoryEntry],
    run: &R,
) -> Result<(), EnforcerError>
where
    S: Serialize,
    R: Serialize,
{
    append_linear_enforcer_graph_audit_to_path(
        &linear_enforcer_graph_audit_path(),
        graph_name,
        stage,
        identifier,
        thread_id,
        terminal_state,
        checkpoints,
        write_history,
        run,
    )
}

fn append_linear_enforcer_graph_audit_to_path<S, R>(
    path: &std::path::Path,
    graph_name: &str,
    stage: &str,
    identifier: &str,
    thread_id: &str,
    terminal_state: &S,
    checkpoints: &[crate::decision_graph_introspection::DecisionGraphCheckpointSnapshot<S>],
    write_history: &[crate::decision_graph_introspection::DecisionGraphWriteHistoryEntry],
    run: &R,
) -> Result<(), EnforcerError>
where
    S: Serialize,
    R: Serialize,
{
    let checkpoint_ref = terminal_decision_checkpoint_ref(
        graph_name,
        thread_id,
        terminal_state,
        checkpoints,
        write_history,
    )?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": graph_name,
        "stage": stage,
        "identifier": identifier,
        "thread_id": thread_id,
        "terminal_checkpoint_ref": checkpoint_ref,
        "run": run,
    });
    let mut line = serde_json::to_vec(&row)?;
    line.push(b'\n');
    let fs = crate::filesystem::RealFileSystem;
    sentinel_domain::ports::FileSystemPort::append(&fs, path, &line).map_err(|e| {
        EnforcerError::Api(format!(
            "append linear enforcer graph audit {}: {e}",
            path.display()
        ))
    })
}

fn post_apply_missing_from_readiness(
    readiness: Result<TicketReadiness, EnforcerError>,
) -> Result<Vec<String>, EnforcerError> {
    Ok(readiness?.missing_owned())
}

async fn post_live_graph_comment(
    client: &reqwest::Client,
    token: &str,
    comment_graph: &crate::linear_comment_graph::LinearCommentGraph,
    issue_id: &str,
    identifier: &str,
    category: &str,
    body: String,
    checkpoint_refs: Vec<String>,
) -> Result<(), EnforcerError> {
    let state = crate::linear_comment_graph::LinearCommentState::new(
        identifier,
        issue_id,
        category,
        body,
        checkpoint_refs,
    );
    let result = crate::linear_comment_graph::post_graph_authorized_comment(
        client,
        token,
        comment_graph,
        state,
    )
    .await
    .map_err(|e| EnforcerError::Api(format!("comment graph decision failed: {e}")))?;
    ensure_comment_graph_post_authorized(category, result.run.state.decision)?;
    append_linear_comment_graph_audit(category, &result)?;
    tracing::debug!(
        ticket = %identifier,
        category,
        comment_graph_thread_id = %result.run.thread_id,
        posted = result.posted,
        "linear enforcer: graph-backed comment path completed"
    );
    Ok(())
}

fn linear_comment_graph_audit_path() -> std::path::PathBuf {
    crate::paths::sentinel_root()
        .join("metrics")
        .join("linear-comment-graph-runs.jsonl")
}

fn append_linear_comment_graph_audit(
    category: &str,
    result: &crate::linear_comment_graph::LinearCommentApplyResult,
) -> Result<(), EnforcerError> {
    append_linear_comment_graph_audit_to_path(&linear_comment_graph_audit_path(), category, result)
}

fn append_linear_comment_graph_audit_to_path(
    path: &std::path::Path,
    category: &str,
    result: &crate::linear_comment_graph::LinearCommentApplyResult,
) -> Result<(), EnforcerError> {
    let comment_checkpoint_ref = terminal_decision_checkpoint_ref(
        "linear_comment",
        &result.run.thread_id,
        &result.run.state,
        &result.run.checkpoints,
        &result.run.write_history,
    )?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "linear_comment",
        "identifier": result.run.state.identifier.clone(),
        "issue_id": result.run.state.issue_id.clone(),
        "category": category,
        "posted": result.posted,
        "decision": result.run.state.decision,
        "comment_checkpoint_ref": comment_checkpoint_ref,
        "required_checkpoint_refs": result.run.state.checkpoint_refs.clone(),
        "thread_id": result.run.thread_id.clone(),
        "run": &result.run,
    });
    let mut line = serde_json::to_vec(&row)?;
    line.push(b'\n');
    let fs = crate::filesystem::RealFileSystem;
    sentinel_domain::ports::FileSystemPort::append(&fs, path, &line).map_err(|e| {
        EnforcerError::Api(format!(
            "append linear comment graph audit {}: {e}",
            path.display()
        ))
    })
}

fn ensure_comment_graph_post_authorized(
    category: &str,
    decision: crate::linear_comment_graph::LinearCommentDecision,
) -> Result<(), EnforcerError> {
    if decision == crate::linear_comment_graph::LinearCommentDecision::Post {
        Ok(())
    } else {
        Err(EnforcerError::Api(format!(
            "comment graph rejected live comment category `{category}` with decision {decision:?}"
        )))
    }
}

// ----- the driver: wire the live subscription to the enforcer -----------

/// Per-issue cooldown: a burst of `issueHistoryCreated` events on one ticket
/// (Linear fires several per edit) collapses to a single enforcement check.
const DEBOUNCE: Duration = Duration::from_secs(20);

/// Runtime configuration for the enforcer driver.
#[derive(Debug, Clone)]
pub struct EnforcerConfig {
    /// Linear PAT (drives both the wss subscription and the REST calls).
    pub token: String,
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

    /// Build a config from the process environment, or `None` when the
    /// graph-backed live enforcer is not explicitly armed. The daemon calls this
    /// at startup and only spawns [`run_enforcer`] when it returns `Some`.
    ///
    /// - `SENTINEL_LINEAR_TOKEN` — the PAT. Absent/empty ⇒ `None` (disabled).
    /// - `SENTINEL_LINEAR_ENFORCE` — must be exactly `live` (case-insensitive).
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
        if !enforce
            .as_deref()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("live"))
        {
            return None;
        }
        let team_filter = teams.and_then(|v| {
            let teams: Vec<String> = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            (!teams.is_empty()).then_some(teams)
        });
        Some(Self { token, team_filter })
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

type EnforcementGraph = langgraph_core::application::services::CompilationResult<
    crate::enforcement_graph::EscalationState,
>;
type RemediationGraph =
    langgraph_core::application::services::CompilationResult<crate::remediation::RemediationState>;
type LiveEnforcerRuntime = (
    crate::openrouter_llm::OpenRouterLlm,
    EnforcementGraph,
    RemediationGraph,
    crate::linear_comment_graph::LinearCommentGraph,
);

async fn build_live_enforcer_runtime() -> Result<LiveEnforcerRuntime, String> {
    let llm = crate::openrouter_llm::OpenRouterLlm::from_env()
        .map_err(|e| format!("OpenRouter LLM unavailable: {e}"))?;
    let enforcement_graph = crate::enforcement_graph::build_escalation_graph()
        .await
        .map_err(|e| format!("escalation graph unavailable: {e}"))?;
    let remediation_graph = crate::remediation::build_remediation_graph()
        .await
        .map_err(|e| format!("remediation graph unavailable: {e}"))?;
    let comment_graph = crate::linear_comment_graph::build_linear_comment_graph()
        .await
        .map_err(|e| format!("comment graph unavailable: {e}"))?;
    Ok((llm, enforcement_graph, remediation_graph, comment_graph))
}

/// Run the full graph-backed enforcement spine: hold the live subscription and
/// enforce each issue-change event through LangGraph checkpoint authorization.
/// Runs until the subscription gives up permanently (PAT rejected) or the
/// receiver is dropped.
///
/// An un-ready started agent-authored ticket is routed through the durable
/// LangGraph remediation graph: it can heal, escalate, or choose last-resort
/// revert only from checkpointed graph decisions. If any required graph runtime
/// cannot be initialized, the enforcer exits instead of running a degraded path.
pub async fn run_enforcer(cfg: EnforcerConfig) {
    let live_runtime = match build_live_enforcer_runtime().await {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(
                error = %e,
                "linear enforcer: required LangGraph runtime unavailable; driver not started"
            );
            return;
        }
    };
    let (llm, egraph, rgraph, cgraph) = &live_runtime;

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

    tracing::info!(
        escalation_runtime = true,
        remediation_runtime = true,
        comment_runtime = true,
        "linear enforcer: driver started"
    );

    while let Some(ev) = rx.recv().await {
        let id = &ev.linear_issue_id;
        if debounced(&mut seen, id) {
            continue;
        }

        // Fetch once to learn the identifier + readiness, and gate on team.
        let readiness = match fetch_readiness(&client, &cfg.token, id).await {
            Ok(r) if !cfg.team_allowed(&r.identifier) => continue,
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, issue = %id, "linear enforcer: pre-check fetch failed");
                continue;
            }
        };

        let enforcement_run =
            match crate::enforcement_graph::evaluate_ticket_report(llm, egraph, &readiness).await {
                Ok(run) => run,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        ticket = %readiness.identifier,
                        "linear enforcer [LIVE]: enforcement graph decision failed"
                    );
                    continue;
                }
            };
        let enforcement_thread_id = enforcement_run.thread_id.clone();
        let enforcement_checkpoint_ref = match terminal_decision_checkpoint_ref(
            "enforcement",
            &enforcement_run.thread_id,
            &enforcement_run.state,
            &enforcement_run.checkpoints,
            &enforcement_run.write_history,
        ) {
            Ok(checkpoint_ref) => checkpoint_ref,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    ticket = %readiness.identifier,
                    enforcement_graph_thread_id = %enforcement_thread_id,
                    "linear enforcer [LIVE]: enforcement graph decision-node checkpoint missing"
                );
                continue;
            }
        };
        if let Err(e) = append_linear_enforcer_graph_audit(
            "enforcement",
            "readiness",
            &readiness.identifier,
            &enforcement_run.thread_id,
            &enforcement_run.state,
            &enforcement_run.checkpoints,
            &enforcement_run.write_history,
            &enforcement_run,
        ) {
            tracing::warn!(
                error = %e,
                ticket = %readiness.identifier,
                enforcement_graph_thread_id = %enforcement_thread_id,
                "linear enforcer [LIVE]: enforcement graph audit append failed"
            );
            continue;
        }
        let enforcement_note = graph_checkpoint_note(&enforcement_checkpoint_ref, &[]);

        // SLA gate (Tier 3) — orthogonal to dev-readiness, comment-only,
        // runs on every team-allowed event: escalate breached/high-risk
        // SLAs and flag Urgent tickets that have no SLA clock at all.
        let now = chrono::Utc::now();
        match evaluate_sla(
            readiness.priority,
            readiness.sla_started_at.as_deref(),
            readiness.sla_breaches_at.as_deref(),
            readiness.sla_high_risk_at.as_deref(),
            now,
        ) {
            SlaVerdict::Breached => {
                let body = format!(
                            "## ⏰ SLA breached\n\nThis ticket's SLA deadline has passed. Escalate or re-scope now.{enforcement_note}"
                        );
                if let Err(e) = post_live_graph_comment(
                    &client,
                    &cfg.token,
                    cgraph,
                    id,
                    &readiness.identifier,
                    "sla_breached",
                    body,
                    vec![enforcement_checkpoint_ref.clone()],
                )
                .await
                {
                    tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: SLA breached comment failed");
                }
                tracing::info!(ticket = %readiness.identifier, graph_thread_id = %enforcement_thread_id, "linear enforcer [LIVE]: SLA breached — escalated");
            }
            SlaVerdict::HighRisk => {
                let body = format!(
                            "## ⏳ SLA at risk\n\nThis ticket's SLA is in its high-risk window and it isn't done. Prioritize it before it breaches.{enforcement_note}"
                        );
                if let Err(e) = post_live_graph_comment(
                    &client,
                    &cfg.token,
                    cgraph,
                    id,
                    &readiness.identifier,
                    "sla_high_risk",
                    body,
                    vec![enforcement_checkpoint_ref.clone()],
                )
                .await
                {
                    tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: SLA high-risk comment failed");
                }
                tracing::info!(ticket = %readiness.identifier, graph_thread_id = %enforcement_thread_id, "linear enforcer [LIVE]: SLA high-risk — escalated");
            }
            SlaVerdict::UrgentNoSla => {
                let body = format!(
                            "## 🚨 Urgent without an SLA\n\nThis ticket is **Urgent** but has no SLA clock running. Apply an SLA so urgent work is time-bound.{enforcement_note}"
                        );
                if let Err(e) = post_live_graph_comment(
                    &client,
                    &cfg.token,
                    cgraph,
                    id,
                    &readiness.identifier,
                    "urgent_without_sla",
                    body,
                    vec![enforcement_checkpoint_ref.clone()],
                )
                .await
                {
                    tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: urgent-without-SLA comment failed");
                }
                tracing::info!(ticket = %readiness.identifier, graph_thread_id = %enforcement_thread_id, "linear enforcer [LIVE]: urgent-without-SLA — flagged");
            }
            SlaVerdict::Ok => {}
        }

        // Due-date gate (comment-only): an open ticket past its dueDate
        // (distinct from SLA). Completed/canceled tickets are exempt.
        if !matches!(readiness.state_type.as_str(), "completed" | "canceled")
            && is_overdue(readiness.due_date.as_deref(), now.date_naive())
        {
            let body = format!(
                        "## 📅 Past due date\n\nThis ticket's due date ({}) has passed and it isn't done. Re-date it or push to close.{}",
                        readiness.due_date.as_deref().unwrap_or(""),
                        enforcement_note,
                    );
            if let Err(e) = post_live_graph_comment(
                &client,
                &cfg.token,
                cgraph,
                id,
                &readiness.identifier,
                "past_due_date",
                body,
                vec![enforcement_checkpoint_ref.clone()],
            )
            .await
            {
                tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: overdue comment failed");
            }
            tracing::info!(ticket = %readiness.identifier, due = ?readiness.due_date, graph_thread_id = %enforcement_thread_id, "linear enforcer [LIVE]: overdue — flagged");
        }

        // Extended workflow discipline (comment-only): a started ticket
        // with no assignee / not in a cycle / no project / no milestone /
        // actively blocked. Flagged, never auto-filled or reverted.
        let discipline = readiness.extended_discipline();
        if !discipline.is_empty() {
            let body = format!(
                        "## 📋 Workflow discipline\n\nThis started ticket has loose ends: **{}**. \
                         Tidy these so the board stays trustworthy (the enforcer flags but won't auto-fill them).{}",
                        discipline.join(", "),
                        enforcement_note,
                    );
            if let Err(e) = post_live_graph_comment(
                &client,
                &cfg.token,
                cgraph,
                id,
                &readiness.identifier,
                "workflow_discipline",
                body,
                vec![enforcement_checkpoint_ref.clone()],
            )
            .await
            {
                tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: workflow-discipline comment failed");
            }
            tracing::info!(ticket = %readiness.identifier, gaps = %discipline.join(", "), graph_thread_id = %enforcement_thread_id, "linear enforcer [LIVE]: extended-discipline flags — commented");
        }

        if enforcement_run.state.decision != crate::enforcement_graph::Decision::Revert {
            tracing::debug!(
                ticket = %readiness.identifier,
                decision = ?enforcement_run.state.decision,
                graph_thread_id = %enforcement_thread_id,
                "linear enforcer [LIVE]: readiness decision clear"
            );
            continue;
        }
        let enforcement_revert_authorization = match enforcement_run.revert_authorization() {
            Ok(Some(authorization)) => authorization,
            Ok(None) => {
                tracing::warn!(ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, "linear enforcer [LIVE]: enforcement graph returned Revert without authorization token");
                continue;
            }
            Err(err) => {
                tracing::warn!(error = %err, ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, "linear enforcer [LIVE]: enforcement graph authorization failed");
                continue;
            }
        };

        // Actor differential (L2): a HUMAN-authored ticket is the
        // operator's own — we ask via comment rather than silently
        // rewriting their fields. Only AGENT/integration-authored
        // tickets get the silent heal-first treatment below.
        if !readiness.created_by_agent {
            let body = format!(
                        "## 🩺 Not dev-ready — please complete before starting\n\nThis ticket entered **{}** while still missing: **{}**. \
                         Set these so it meets the Definition of Ready (the enforcer leaves human-authored tickets for you to fill rather than rewriting them).{}",
                        readiness.state_name,
                        readiness.missing_owned().join(", "),
                        enforcement_note,
                    );
            if let Err(e) = post_live_graph_comment(
                &client,
                &cfg.token,
                cgraph,
                id,
                &readiness.identifier,
                "human_authored_readiness",
                body,
                vec![enforcement_checkpoint_ref.clone()],
            )
            .await
            {
                tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: human-authored readiness comment failed");
            }
            tracing::info!(ticket = %readiness.identifier, missing = %readiness.missing_owned().join(", "), graph_thread_id = %enforcement_thread_id, "linear enforcer [LIVE]: human-authored — escalated via comment (no auto-heal)");
            continue;
        }

        // ASSISTANT, not bouncer: remediate-first. Heal as many gaps as
        // we can confidently infer; the ticket stays in progress. Only
        // escalate the gaps we can't fill, and revert as a last resort.
        {
            let missing = readiness.missing_owned();
            let catalog =
                match crate::remediation::fetch_label_catalog(&client, &cfg.token, id).await {
                    Ok(catalog) => catalog,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            ticket = %readiness.identifier,
                            enforcement_graph_thread_id = %enforcement_thread_id,
                            "linear enforcer [LIVE]: remediation catalog fetch failed"
                        );
                        continue;
                    }
                };
            let (title, desc) =
                match crate::remediation::fetch_ticket_text(&client, &cfg.token, id).await {
                    Ok(text) => text,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            ticket = %readiness.identifier,
                            enforcement_graph_thread_id = %enforcement_thread_id,
                            "linear enforcer [LIVE]: remediation ticket text fetch failed"
                        );
                        continue;
                    }
                };
            let plan = crate::remediation::propose_remediation(
                llm,
                &readiness.identifier,
                &title,
                &desc,
                &missing,
                &catalog,
            )
            .await;
            let planned_count = plan.applicable_change_count(&catalog);
            let pre_state = crate::remediation::RemediationState {
                identifier: readiness.identifier.clone(),
                stage: crate::remediation::RemediationStage::PreApply,
                missing_before: missing.clone(),
                planned_count,
                applied_count: 0,
                still_missing: plan.unfillable.clone(),
                proposal_failed: plan.proposal_failed,
                outcome: crate::remediation::RemediationOutcome::Clear,
            };
            let pre_run = match crate::remediation::run_remediation_decision_report(
                rgraph, pre_state,
            )
            .await
            {
                Ok(run) => run,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        ticket = %readiness.identifier,
                        enforcement_graph_thread_id = %enforcement_thread_id,
                        "linear enforcer [LIVE]: remediation pre-apply graph failed"
                    );
                    continue;
                }
            };
            let pre_graph_thread_id = pre_run.thread_id.clone();
            let pre_graph_checkpoint_ref = match terminal_decision_checkpoint_ref(
                "remediation",
                &pre_run.thread_id,
                &pre_run.state,
                &pre_run.checkpoints,
                &pre_run.write_history,
            ) {
                Ok(checkpoint_ref) => checkpoint_ref,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        ticket = %readiness.identifier,
                        enforcement_graph_thread_id = %enforcement_thread_id,
                        remediation_pre_graph_thread_id = %pre_graph_thread_id,
                        "linear enforcer [LIVE]: remediation pre-apply decision-node checkpoint missing"
                    );
                    continue;
                }
            };
            if let Err(e) = append_linear_enforcer_graph_audit(
                "remediation",
                "pre_apply",
                &readiness.identifier,
                &pre_run.thread_id,
                &pre_run.state,
                &pre_run.checkpoints,
                &pre_run.write_history,
                &pre_run,
            ) {
                tracing::warn!(
                    error = %e,
                    ticket = %readiness.identifier,
                    enforcement_graph_thread_id = %enforcement_thread_id,
                    remediation_pre_graph_thread_id = %pre_graph_thread_id,
                    "linear enforcer [LIVE]: remediation pre-apply graph audit append failed"
                );
                continue;
            }
            let apply_authorization = match pre_run.state.outcome {
                crate::remediation::RemediationOutcome::Apply => {
                    match pre_run.apply_authorization() {
                        Ok(Some(authorization)) => authorization,
                        Ok(None) => {
                            tracing::warn!(ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_pre_graph_thread_id = %pre_graph_thread_id, "linear enforcer [LIVE]: remediation graph returned Apply without authorization token");
                            continue;
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_pre_graph_thread_id = %pre_graph_thread_id, "linear enforcer [LIVE]: remediation apply authorization failed");
                            continue;
                        }
                    }
                }
                crate::remediation::RemediationOutcome::Escalate => {
                    let comment_refs = vec![
                        enforcement_checkpoint_ref.clone(),
                        pre_graph_checkpoint_ref.clone(),
                    ];
                    let graph_note = graph_checkpoint_note(
                        &enforcement_checkpoint_ref,
                        &[("remediation_pre", &pre_graph_checkpoint_ref)],
                    );
                    let body = format!(
                                "## 🩺 Auto-remediation needs your input\n\nThe enforcer could not safely apply a remediation plan. Still needs a human decision: **{}**. Please set these so the ticket is dev-ready.{}",
                                if plan.unfillable.is_empty() {
                                    missing.join(", ")
                                } else {
                                    plan.unfillable.join(", ")
                                },
                                graph_note,
                            );
                    if let Err(e) = post_live_graph_comment(
                        &client,
                        &cfg.token,
                        cgraph,
                        id,
                        &readiness.identifier,
                        "remediation_pre_escalate",
                        body,
                        comment_refs,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: remediation pre-apply escalation comment failed");
                    }
                    tracing::info!(ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_pre_graph_thread_id = %pre_graph_thread_id, "linear enforcer [LIVE]: remediation pre-apply escalated");
                    continue;
                }
                crate::remediation::RemediationOutcome::Revert => {
                    let remediation_revert_authorization = match pre_run.revert_authorization() {
                        Ok(Some(authorization)) => authorization,
                        Ok(None) => {
                            tracing::warn!(ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_pre_graph_thread_id = %pre_graph_thread_id, "linear enforcer [LIVE]: remediation pre-apply graph returned Revert without authorization token");
                            continue;
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_pre_graph_thread_id = %pre_graph_thread_id, "linear enforcer [LIVE]: remediation pre-apply revert authorization failed");
                            continue;
                        }
                    };
                    let revert_authorization =
                        match LinearRevertAuthorization::from_enforcement_and_remediation(
                            &enforcement_revert_authorization,
                            &remediation_revert_authorization,
                        ) {
                            Ok(authorization) => authorization,
                            Err(e) => {
                                tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: remediation pre-apply revert authorization failed");
                                continue;
                            }
                        };
                    let graph_note = graph_checkpoint_note(
                        &enforcement_checkpoint_ref,
                        &[("remediation_pre", &pre_graph_checkpoint_ref)],
                    );
                    match enforce_ticket_with_graph_audit(
                        &client,
                        &cfg.token,
                        id,
                        &graph_note,
                        cgraph,
                        &revert_authorization,
                    )
                    .await
                    {
                        Ok(_) => {
                            tracing::info!(ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_pre_graph_thread_id = %pre_graph_thread_id, "linear enforcer [LIVE]: reverted after pre-apply graph decision")
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_pre_graph_thread_id = %pre_graph_thread_id, "linear enforcer [LIVE]: pre-apply revert failed")
                        }
                    }
                    continue;
                }
                crate::remediation::RemediationOutcome::Clear => {
                    tracing::info!(ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_pre_graph_thread_id = %pre_graph_thread_id, "linear enforcer [LIVE]: remediation pre-apply graph cleared without mutation");
                    continue;
                }
            };

            let applied = match crate::remediation::apply_remediation(
                &client,
                &cfg.token,
                id,
                &plan,
                &catalog,
                &apply_authorization,
            )
            .await
            {
                Ok(applied) => applied,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        ticket = %readiness.identifier,
                        enforcement_graph_thread_id = %enforcement_thread_id,
                        remediation_pre_graph_thread_id = %pre_graph_thread_id,
                        remediation_pre_graph_checkpoint = %pre_graph_checkpoint_ref,
                        "linear enforcer [LIVE]: graph-authorized remediation apply failed"
                    );
                    continue;
                }
            };

            // Re-fetch to learn what's STILL missing after the heal.
            let still_missing = match post_apply_missing_from_readiness(
                fetch_readiness(&client, &cfg.token, id).await,
            ) {
                Ok(still_missing) => still_missing,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        ticket = %readiness.identifier,
                        enforcement_graph_thread_id = %enforcement_thread_id,
                        remediation_pre_graph_thread_id = %pre_graph_thread_id,
                        remediation_pre_graph_checkpoint = %pre_graph_checkpoint_ref,
                        "linear enforcer [LIVE]: remediation post-apply readiness fetch failed"
                    );
                    continue;
                }
            };
            let state = crate::remediation::RemediationState {
                identifier: readiness.identifier.clone(),
                stage: crate::remediation::RemediationStage::PostApply,
                missing_before: missing,
                planned_count,
                applied_count: applied.len(),
                still_missing: still_missing.clone(),
                proposal_failed: plan.proposal_failed,
                outcome: crate::remediation::RemediationOutcome::Clear,
            };
            match crate::remediation::run_remediation_decision_report(rgraph, state).await {
                Ok(run) => {
                    let graph_thread_id = run.thread_id.clone();
                    let graph_checkpoint_ref = match terminal_decision_checkpoint_ref(
                        "remediation",
                        &run.thread_id,
                        &run.state,
                        &run.checkpoints,
                        &run.write_history,
                    ) {
                        Ok(checkpoint_ref) => checkpoint_ref,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                ticket = %readiness.identifier,
                                enforcement_graph_thread_id = %enforcement_thread_id,
                                remediation_pre_graph_thread_id = %pre_graph_thread_id,
                                remediation_post_graph_thread_id = %graph_thread_id,
                                "linear enforcer [LIVE]: remediation post-apply decision-node checkpoint missing"
                            );
                            continue;
                        }
                    };
                    if let Err(e) = append_linear_enforcer_graph_audit(
                        "remediation",
                        "post_apply",
                        &readiness.identifier,
                        &run.thread_id,
                        &run.state,
                        &run.checkpoints,
                        &run.write_history,
                        &run,
                    ) {
                        tracing::warn!(
                            error = %e,
                            ticket = %readiness.identifier,
                            enforcement_graph_thread_id = %enforcement_thread_id,
                            remediation_pre_graph_thread_id = %pre_graph_thread_id,
                            remediation_post_graph_thread_id = %graph_thread_id,
                            "linear enforcer [LIVE]: remediation post-apply graph audit append failed"
                        );
                        continue;
                    }
                    let comment_refs = vec![
                        enforcement_checkpoint_ref.clone(),
                        pre_graph_checkpoint_ref.clone(),
                        graph_checkpoint_ref.clone(),
                    ];
                    let graph_note = graph_checkpoint_note(
                        &enforcement_checkpoint_ref,
                        &[
                            ("remediation_pre", &pre_graph_checkpoint_ref),
                            ("remediation_post", &graph_checkpoint_ref),
                        ],
                    );
                    match run.state.outcome {
                        crate::remediation::RemediationOutcome::Apply => {
                            tracing::warn!(ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_pre_graph_thread_id = %pre_graph_thread_id, remediation_post_graph_thread_id = %graph_thread_id, "linear enforcer [LIVE]: post-apply remediation graph returned Apply")
                        }
                        crate::remediation::RemediationOutcome::Clear => {
                            if !applied.is_empty() {
                                let body = format!(
                                            "## 🩺 Auto-remediated to dev-ready\n\nThe enforcer filled: {}. Ticket stays in progress. ({}){}",
                                            applied.join(", "),
                                            plan.rationale,
                                            graph_note,
                                        );
                                if let Err(e) = post_live_graph_comment(
                                    &client,
                                    &cfg.token,
                                    cgraph,
                                    id,
                                    &readiness.identifier,
                                    "remediation_post_clear",
                                    body,
                                    comment_refs.clone(),
                                )
                                .await
                                {
                                    tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: remediation clear comment failed");
                                }
                            }
                            tracing::info!(ticket = %readiness.identifier, healed = %applied.join(", "), enforcement_graph_thread_id = %enforcement_thread_id, remediation_graph_thread_id = %graph_thread_id, "linear enforcer [LIVE]: healed in place (Clear)");
                        }
                        crate::remediation::RemediationOutcome::Escalate => {
                            let body = format!(
                                        "## 🩺 Partially auto-remediated — needs your input\n\nFilled: {}. Still needs a human decision: **{}**. Please set these so the ticket is dev-ready.{}",
                                        if applied.is_empty() {
                                            "(nothing)".into()
                                        } else {
                                            applied.join(", ")
                                        },
                                        still_missing.join(", "),
                                        graph_note,
                                    );
                            if let Err(e) = post_live_graph_comment(
                                &client,
                                &cfg.token,
                                cgraph,
                                id,
                                &readiness.identifier,
                                "remediation_post_escalate",
                                body,
                                comment_refs.clone(),
                            )
                            .await
                            {
                                tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: remediation post-apply escalation comment failed");
                            }
                            tracing::info!(ticket = %readiness.identifier, gaps = %still_missing.join(", "), enforcement_graph_thread_id = %enforcement_thread_id, remediation_graph_thread_id = %graph_thread_id, "linear enforcer [LIVE]: escalated remaining gaps (stays in progress)");
                        }
                        crate::remediation::RemediationOutcome::Revert => {
                            // Last resort: nothing could be healed.
                            let remediation_revert_authorization = match run.revert_authorization()
                            {
                                Ok(Some(authorization)) => authorization,
                                Ok(None) => {
                                    tracing::warn!(ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_post_graph_thread_id = %graph_thread_id, "linear enforcer [LIVE]: remediation post-apply graph returned Revert without authorization token");
                                    continue;
                                }
                                Err(err) => {
                                    tracing::warn!(error = %err, ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_post_graph_thread_id = %graph_thread_id, "linear enforcer [LIVE]: remediation post-apply revert authorization failed");
                                    continue;
                                }
                            };
                            let revert_authorization =
                                match LinearRevertAuthorization::from_enforcement_and_remediation(
                                    &enforcement_revert_authorization,
                                    &remediation_revert_authorization,
                                ) {
                                    Ok(authorization) => authorization,
                                    Err(e) => {
                                        tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: remediation post-apply revert authorization failed");
                                        continue;
                                    }
                                };
                            match enforce_ticket_with_graph_audit(
                                &client,
                                &cfg.token,
                                id,
                                &graph_note,
                                cgraph,
                                &revert_authorization,
                            )
                            .await
                            {
                                Ok(_) => {
                                    tracing::info!(ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_graph_thread_id = %graph_thread_id, "linear enforcer [LIVE]: reverted (last resort — unhealable)")
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, ticket = %readiness.identifier, enforcement_graph_thread_id = %enforcement_thread_id, remediation_graph_thread_id = %graph_thread_id, "linear enforcer [LIVE]: revert failed")
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, ticket = %readiness.identifier, "linear enforcer [LIVE]: remediation graph failed")
                }
            }
        }
    }

    tracing::warn!("linear enforcer: driver stopped (subscription ended)");
}

#[cfg(test)]
mod tests {
    use super::*;

    use langgraph_core::domain::value_objects::END;

    fn ready(state_type: &str) -> TicketReadiness {
        TicketReadiness {
            identifier: "FPCRM-1".into(),
            estimate: Some(3.0),
            state_type: state_type.into(),
            state_name: "x".into(),
            has_type_label: true,
            has_area_label: true,
            created_by_agent: true,
            priority: Some(3),
            sla_started_at: None,
            sla_breaches_at: None,
            has_acceptance_criteria: true,
            assignee_present: true,
            in_cycle: true,
            in_project: true,
            actively_blocked: false,
            sla_high_risk_at: None,
            due_date: None,
            has_milestone: true,
        }
    }

    #[test]
    fn terminal_decision_checkpoint_ref_uses_decision_node_checkpoint() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Revert"
        });
        let revert_checkpoint = decision_checkpoint("thread-1", "checkpoint-1", 1, "revert");
        let end_checkpoint = decision_checkpoint("thread-1", "checkpoint-2", 2, END);
        let checkpoints = vec![end_checkpoint, revert_checkpoint];
        let writes = vec![
            decision_write("checkpoint-1", 1, "revert", terminal.clone()),
            decision_write("checkpoint-2", 2, END, terminal.clone()),
        ];

        let checkpoint_ref = terminal_decision_checkpoint_ref(
            "enforcement",
            "thread-1",
            &terminal,
            &checkpoints,
            &writes,
        )
        .expect("decision-node checkpoint ref must resolve");

        assert_eq!(checkpoint_ref, "thread-1#checkpoint-1");
    }

    #[test]
    fn terminal_decision_checkpoint_ref_rejects_boundary_only_history() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Revert"
        });
        let checkpoints = vec![decision_checkpoint("thread-1", "checkpoint-2", 2, END)];
        let writes = vec![decision_write("checkpoint-2", 2, END, terminal.clone())];

        let err = terminal_decision_checkpoint_ref(
            "enforcement",
            "thread-1",
            &terminal,
            &checkpoints,
            &writes,
        )
        .expect_err("boundary-only write history must not authorize an audit ref");

        assert!(err
            .to_string()
            .contains("write history omitted terminal decision-node state write"));
    }

    #[test]
    fn graph_checkpoint_note_includes_enforcement_and_optional_remediation() {
        assert_eq!(
            graph_checkpoint_note("enforcement:FPCRM-1:abc#chk-1", &[]),
            "\n\nLangGraph checkpoints: enforcement `enforcement:FPCRM-1:abc#chk-1`"
        );
        assert_eq!(
            graph_checkpoint_note(
                "enforcement:FPCRM-1:abc#chk-1",
                &[("remediation_pre", "remediation:FPCRM-1:def#chk-2")]
            ),
            "\n\nLangGraph checkpoints: enforcement `enforcement:FPCRM-1:abc#chk-1`, remediation_pre `remediation:FPCRM-1:def#chk-2`"
        );
        assert_eq!(
            graph_checkpoint_note(
                "enforcement:FPCRM-1:abc#chk-1",
                &[
                    ("remediation_pre", "remediation:FPCRM-1:def#chk-2"),
                    ("remediation_post", "remediation:FPCRM-1:ghi#chk-3"),
                ],
            ),
            "\n\nLangGraph checkpoints: enforcement `enforcement:FPCRM-1:abc#chk-1`, remediation_pre `remediation:FPCRM-1:def#chk-2`, remediation_post `remediation:FPCRM-1:ghi#chk-3`"
        );
    }

    #[test]
    fn linear_enforcer_graph_audit_jsonl_preserves_terminal_decision_evidence() {
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let path = dir.path().join("linear-enforcer-graph-runs.jsonl");
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Revert"
        });
        let checkpoints = vec![
            decision_checkpoint("thread-1", "checkpoint-1", 1, "revert"),
            decision_checkpoint("thread-1", "checkpoint-2", 2, END),
        ];
        let writes = vec![
            decision_write("checkpoint-1", 1, "revert", terminal.clone()),
            decision_write("checkpoint-2", 2, END, terminal.clone()),
        ];
        let run = serde_json::json!({
            "state": terminal.clone(),
            "thread_id": "thread-1",
            "checkpoints": &checkpoints,
            "write_history": &writes,
            "topology": {"graph": "enforcement", "durable_checkpointer": true},
        });

        append_linear_enforcer_graph_audit_to_path(
            &path,
            "enforcement",
            "readiness",
            "FPCRM-1",
            "thread-1",
            &terminal,
            &checkpoints,
            &writes,
            &run,
        )
        .expect("append audit");

        let text = std::fs::read_to_string(&path).expect("audit jsonl");
        let row: serde_json::Value = serde_json::from_str(text.trim()).expect("audit row");
        assert_eq!(row["workflow_authority"], "langgraph");
        assert_eq!(row["graph"], "enforcement");
        assert_eq!(row["stage"], "readiness");
        assert_eq!(row["identifier"], "FPCRM-1");
        assert_eq!(row["terminal_checkpoint_ref"], "thread-1#checkpoint-1");
        assert_eq!(row["run"]["topology"]["graph"], "enforcement");
        assert_eq!(
            row["run"]["checkpoints"][0]["checkpoint_id"],
            "checkpoint-1"
        );
        assert_eq!(row["run"]["write_history"][0]["node_id"], "revert");
    }

    fn decision_checkpoint(
        thread_id: &str,
        checkpoint_id: &str,
        step_number: u64,
        node_id: &str,
    ) -> crate::decision_graph_introspection::DecisionGraphCheckpointSnapshot<serde_json::Value>
    {
        crate::decision_graph_introspection::DecisionGraphCheckpointSnapshot {
            checkpoint_id: checkpoint_id.to_string(),
            parent_checkpoint_id: None,
            thread_id: thread_id.to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            step_number,
            source_step: Some(step_number as i32),
            source_type: Some("stream_update".to_string()),
            source_node: Some(node_id.to_string()),
            tags: std::collections::BTreeMap::new(),
            writes: vec![
                crate::decision_graph_introspection::DecisionGraphCheckpointWriteInfo {
                    node_id: node_id.to_string(),
                    channel: "state".to_string(),
                    ts: "2026-01-01T00:00:00Z".to_string(),
                },
            ],
            state: serde_json::json!({
                "identifier": "FPCRM-1",
                "decision": "Revert"
            }),
        }
    }

    fn decision_write(
        checkpoint_id: &str,
        step_number: u64,
        node_id: &str,
        value_json: serde_json::Value,
    ) -> crate::decision_graph_introspection::DecisionGraphWriteHistoryEntry {
        crate::decision_graph_introspection::DecisionGraphWriteHistoryEntry {
            thread_id: "thread-1".to_string(),
            checkpoint_id: checkpoint_id.to_string(),
            step_number,
            channel: "state".to_string(),
            node_id: node_id.to_string(),
            ts: "2026-01-01T00:00:00Z".to_string(),
            value_len: 1,
            value_sha256: "0".repeat(64),
            value_json,
        }
    }

    fn linear_comment_topology() -> crate::decision_graph_introspection::DecisionGraphTopology {
        crate::decision_graph_introspection::DecisionGraphTopology {
            graph: "linear_comment".to_string(),
            durable_checkpointer: true,
            checkpointer_backend: "sqlite".to_string(),
            checkpointer_scope: "database_path:test.db".to_string(),
            checkpointer_tenant_scope: None,
            auto_checkpoint: true,
            max_iterations: 100,
            schemas: crate::decision_graph_introspection::DecisionGraphSchemas {
                state: None,
                input: None,
                output: None,
                context: None,
            },
            nodes: Vec::new(),
            edges: Vec::new(),
            subgraphs: Vec::new(),
        }
    }

    fn linear_comment_apply_result() -> crate::linear_comment_graph::LinearCommentApplyResult {
        let mut state = crate::linear_comment_graph::LinearCommentState::new(
            "FPCRM-1",
            "issue-uuid",
            "sla_breached",
            "## SLA breached\n\nLangGraph checkpoints: enforcement `e1`",
            vec!["e1".to_string()],
        );
        state.decision = crate::linear_comment_graph::LinearCommentDecision::Post;
        let checkpoint = crate::decision_graph_introspection::DecisionGraphCheckpointSnapshot {
            checkpoint_id: "comment-checkpoint".to_string(),
            parent_checkpoint_id: None,
            thread_id: "comment-thread".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            step_number: 1,
            source_step: Some(1),
            source_type: Some("stream_update".to_string()),
            source_node: Some("post_comment".to_string()),
            tags: std::collections::BTreeMap::new(),
            writes: vec![
                crate::decision_graph_introspection::DecisionGraphCheckpointWriteInfo {
                    node_id: "post_comment".to_string(),
                    channel: "state".to_string(),
                    ts: "2026-01-01T00:00:00Z".to_string(),
                },
            ],
            state: state.clone(),
        };
        let write = crate::decision_graph_introspection::DecisionGraphWriteHistoryEntry {
            thread_id: "comment-thread".to_string(),
            checkpoint_id: "comment-checkpoint".to_string(),
            step_number: 1,
            channel: "state".to_string(),
            node_id: "post_comment".to_string(),
            ts: "2026-01-01T00:00:00Z".to_string(),
            value_len: 1,
            value_sha256: "0".repeat(64),
            value_json: serde_json::to_value(&state).expect("state json"),
        };
        crate::linear_comment_graph::LinearCommentApplyResult {
            run: crate::linear_comment_graph::LinearCommentRun {
                state,
                thread_id: "comment-thread".to_string(),
                checkpoints: vec![checkpoint],
                write_history: vec![write],
                stream: Vec::new(),
                topology: linear_comment_topology(),
            },
            posted: true,
        }
    }

    #[test]
    fn live_comment_requires_comment_graph_post_decision() {
        ensure_comment_graph_post_authorized(
            "sla_breached",
            crate::linear_comment_graph::LinearCommentDecision::Post,
        )
        .expect("Post authorizes the live comment path");

        let err = ensure_comment_graph_post_authorized(
            "sla_breached",
            crate::linear_comment_graph::LinearCommentDecision::Skip,
        )
        .expect_err("Skip must fail the live comment path")
        .to_string();
        assert!(err.contains("comment graph rejected"));
        assert!(err.contains("sla_breached"));
    }

    #[test]
    fn linear_comment_graph_audit_jsonl_preserves_checkpoint_evidence() {
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let path = dir.path().join("linear-comment-graph-runs.jsonl");
        let result = linear_comment_apply_result();

        append_linear_comment_graph_audit_to_path(&path, "sla_breached", &result)
            .expect("append audit");

        let text = std::fs::read_to_string(&path).expect("audit jsonl");
        let row: serde_json::Value = serde_json::from_str(text.trim()).expect("audit row");
        assert_eq!(row["workflow_authority"], "langgraph");
        assert_eq!(row["graph"], "linear_comment");
        assert_eq!(row["identifier"], "FPCRM-1");
        assert_eq!(row["category"], "sla_breached");
        assert_eq!(row["posted"], true);
        assert_eq!(
            row["comment_checkpoint_ref"],
            "comment-thread#comment-checkpoint"
        );
        assert_eq!(row["required_checkpoint_refs"], serde_json::json!(["e1"]));
        assert_eq!(row["run"]["topology"]["graph"], "linear_comment");
        assert_eq!(
            row["run"]["checkpoints"][0]["checkpoint_id"],
            "comment-checkpoint"
        );
        assert_eq!(row["run"]["write_history"][0]["channel"], "state");
    }

    #[test]
    fn post_apply_readiness_must_be_fetched() {
        let mut readiness = ready("started");
        readiness.estimate = None;
        assert_eq!(
            post_apply_missing_from_readiness(Ok(readiness)).expect("fetched readiness"),
            vec!["estimate".to_string()]
        );

        let err = post_apply_missing_from_readiness(Err(EnforcerError::Api("boom".into())))
            .expect_err("post-apply cannot synthesize readiness evidence")
            .to_string();
        assert!(err.contains("boom"));
    }

    #[test]
    fn revert_mutation_validator_rejects_failed_graphql_envelopes() {
        validate_linear_success(
            "issueUpdate",
            r#"{"data":{"issueUpdate":{"success":true}}}"#,
        )
        .expect("success envelope");
        assert!(validate_linear_success(
            "issueUpdate",
            r#"{"errors":[{"message":"permission denied"}],"data":null}"#
        )
        .expect_err("GraphQL errors must fail")
        .to_string()
        .contains("GraphQL errors"));
        assert!(validate_linear_success(
            "issueUpdate",
            r#"{"data":{"issueUpdate":{"success":false}}}"#
        )
        .expect_err("success=false must fail")
        .to_string()
        .contains("success=true"));
        assert!(
            validate_linear_success("issueUpdate", r#"{"data":{"issueUpdate":{}}}"#)
                .expect_err("missing success must fail")
                .to_string()
                .contains("success=true")
        );
    }

    #[test]
    fn dev_ready_started_is_not_reverted() {
        assert!(!ready("started").should_revert());
    }

    #[test]
    fn parse_readiness_attributes_actor_for_the_differential() {
        let base = |actor: serde_json::Value| {
            let mut issue = serde_json::json!({
                "identifier": "FPCRM-9",
                "estimate": null,
                "state": { "name": "In Progress", "type": "started" },
                "labels": { "nodes": [] },
                "integrationSourceType": null,
            });
            issue
                .as_object_mut()
                .unwrap()
                .insert("botActor".into(), actor);
            parse_readiness(&issue).expect("parses")
        };
        // botActor present ⇒ agent-authored (silent heal path).
        assert!(base(serde_json::json!({ "id": "bot-1" })).created_by_agent);
        // botActor absent + no integration source ⇒ human-authored (ask path).
        assert!(!base(serde_json::Value::Null).created_by_agent);

        // integrationSourceType present (no botActor) also ⇒ agent/integration.
        let mut issue = serde_json::json!({
            "identifier": "FPCRM-9",
            "estimate": null,
            "state": { "name": "In Progress", "type": "started" },
            "labels": { "nodes": [] },
            "botActor": null,
            "integrationSourceType": "github",
        });
        assert!(parse_readiness(&issue).unwrap().created_by_agent);
        issue["integrationSourceType"] = serde_json::Value::Null;
        assert!(!parse_readiness(&issue).unwrap().created_by_agent);
    }

    #[test]
    fn evaluate_sla_covers_breach_risk_and_urgent_no_sla() {
        use chrono::{Duration, Utc};
        let now = Utc::now();
        let rfc = |dt: chrono::DateTime<Utc>| dt.to_rfc3339();

        // Breached: deadline in the past.
        let past = rfc(now - Duration::hours(1));
        assert_eq!(
            evaluate_sla(Some(2), None, Some(&past), None, now),
            SlaVerdict::Breached
        );

        // HighRisk via Sentinel's 80% policy: 90% of a 10h window elapsed.
        let start = rfc(now - Duration::hours(9));
        let breach = rfc(now + Duration::hours(1));
        assert_eq!(
            evaluate_sla(Some(2), Some(&start), Some(&breach), None, now),
            SlaVerdict::HighRisk
        );

        // Ok: only 10% elapsed (started 1h ago, breaches in 9h), no native threshold.
        let start2 = rfc(now - Duration::hours(1));
        let breach2 = rfc(now + Duration::hours(9));
        assert_eq!(
            evaluate_sla(Some(2), Some(&start2), Some(&breach2), None, now),
            SlaVerdict::Ok
        );

        // NATIVE high-risk takes precedence: high-risk-at already passed → HighRisk
        // even though only 10% of the window has elapsed by the 80% heuristic.
        let hr_past = rfc(now - Duration::minutes(5));
        assert_eq!(
            evaluate_sla(Some(2), Some(&start2), Some(&breach2), Some(&hr_past), now),
            SlaVerdict::HighRisk
        );
        // Native high-risk in the future → still Ok despite being well into the window.
        let hr_future = rfc(now + Duration::hours(8));
        assert_eq!(
            evaluate_sla(Some(2), Some(&start), Some(&breach), Some(&hr_future), now),
            SlaVerdict::Ok
        );

        // UrgentNoSla: priority 1, no SLA clock.
        assert_eq!(
            evaluate_sla(Some(1), None, None, None, now),
            SlaVerdict::UrgentNoSla
        );

        // Non-urgent with no SLA is fine.
        assert_eq!(evaluate_sla(Some(3), None, None, None, now), SlaVerdict::Ok);
        assert_eq!(evaluate_sla(None, None, None, None, now), SlaVerdict::Ok);
    }

    #[test]
    fn is_overdue_compares_due_date_to_today() {
        use chrono::NaiveDate;
        let today = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        assert!(is_overdue(Some("2026-06-09"), today), "yesterday ⇒ overdue");
        assert!(
            !is_overdue(Some("2026-06-10"), today),
            "today ⇒ not overdue"
        );
        assert!(
            !is_overdue(Some("2026-06-11"), today),
            "tomorrow ⇒ not overdue"
        );
        assert!(!is_overdue(None, today), "no due date ⇒ not overdue");
        assert!(
            !is_overdue(Some("garbage"), today),
            "unparseable ⇒ not overdue"
        );
    }

    #[test]
    fn extended_discipline_flags_missing_milestone_in_a_project() {
        let mut t = ready("started");
        t.has_milestone = false; // but in_project is true
        assert!(t.extended_discipline().contains(&"no project milestone"));
        // no project at all ⇒ flag the project, not the milestone
        t.in_project = false;
        let d = t.extended_discipline();
        assert!(d.contains(&"no project") && !d.contains(&"no project milestone"));
    }

    #[test]
    fn acceptance_criteria_needs_three_checklist_or_numbered_items() {
        assert!(has_acceptance_criteria(
            "## AC\n- [ ] one\n- [ ] two\n- [ ] three"
        ));
        assert!(has_acceptance_criteria("1. first\n2. second\n3) third"));
        assert!(!has_acceptance_criteria("- [ ] only one\n- [ ] two"));
        assert!(!has_acceptance_criteria("just prose, no list at all"));
        assert!(!has_acceptance_criteria(""));
    }

    #[test]
    fn extended_discipline_flags_started_loose_ends_only() {
        let mut t = ready("started");
        assert!(t.extended_discipline().is_empty(), "fully tidy ⇒ no flags");
        t.assignee_present = false;
        t.in_cycle = false;
        t.in_project = false;
        let d = t.extended_discipline();
        assert!(
            d.contains(&"no assignee")
                && d.contains(&"not in a cycle")
                && d.contains(&"no project")
        );
        // Not started ⇒ discipline checks don't fire (only enforce on active work).
        let mut backlog = ready("backlog");
        backlog.assignee_present = false;
        backlog.in_cycle = false;
        backlog.in_project = false;
        assert!(backlog.extended_discipline().is_empty());
    }

    #[test]
    fn has_live_blocker_detects_unresolved_inbound_blocks() {
        let blocked_by = |state: &str| {
            serde_json::json!({
                "inverseRelations": { "nodes": [
                    { "type": "blocks", "issue": { "state": { "type": state } } }
                ] }
            })
        };
        assert!(
            has_live_blocker(&blocked_by("started")),
            "blocked by active issue ⇒ live"
        );
        assert!(has_live_blocker(&blocked_by("backlog")));
        assert!(
            !has_live_blocker(&blocked_by("completed")),
            "blocker done ⇒ not live"
        );
        assert!(!has_live_blocker(&blocked_by("canceled")));
        // 'related' (not 'blocks') is not a blocker.
        let related = serde_json::json!({"inverseRelations":{"nodes":[{"type":"related","issue":{"state":{"type":"started"}}}]}});
        assert!(!has_live_blocker(&related));
        // No relations at all.
        assert!(!has_live_blocker(&serde_json::json!({})));
    }

    #[test]
    fn extended_discipline_flags_active_blocker_on_started() {
        let mut t = ready("started");
        t.actively_blocked = true;
        assert!(t
            .extended_discipline()
            .contains(&"started but actively blocked"));
    }

    #[test]
    fn missing_acceptance_criteria_triggers_revert_when_started() {
        let mut t = ready("started");
        t.has_acceptance_criteria = false;
        assert!(t.should_revert());
        assert!(t.missing().contains(&"acceptance criteria"));
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
        assert!(
            !t.should_revert(),
            "backlog work isn't enforced until it starts"
        );
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
            team_filter,
        }
    }

    #[test]
    fn from_raw_requires_explicit_live_arming() {
        assert!(EnforcerConfig::from_raw(Some("pat".into()), None, None).is_none());
        assert!(EnforcerConfig::from_raw(
            Some("pat".into()),
            Some("on".into()),
            Some(" , ".into())
        )
        .is_none());
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
        assert!(
            !c.team_allowed("FPCRMX-1"),
            "prefix must be exact, not a substring"
        );
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
        .expect("token and live arming present");
        assert_eq!(c.token, "pat_xyz");
        assert_eq!(
            c.team_filter,
            Some(vec!["FPCRM".to_string(), "FPROUTE".to_string()]),
            "teams trimmed, empties dropped"
        );
    }

    #[test]
    fn from_raw_live_with_empty_teams_means_all_teams() {
        let c =
            EnforcerConfig::from_raw(Some("pat".into()), Some("live".into()), Some(" , ".into()))
                .unwrap();
        assert_eq!(c.team_filter, None, "all-empty TEAMS means no filter");
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
        assert!(
            !debounced(&mut seen, "issue-1"),
            "allowed again after the window"
        );
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
        assert_eq!(
            next_cursor("2026-06-01T00:00:00Z", &nodes),
            "2026-06-09T12:00:00Z"
        );
    }

    #[test]
    fn next_cursor_keeps_prev_when_nothing_newer() {
        // Empty round, or all older than the cursor → cursor unchanged.
        assert_eq!(
            next_cursor("2026-06-09T12:00:00Z", &[]),
            "2026-06-09T12:00:00Z"
        );
        let older = vec![serde_json::json!({ "id": "a", "updatedAt": "2026-06-08T00:00:00Z" })];
        assert_eq!(
            next_cursor("2026-06-09T12:00:00Z", &older),
            "2026-06-09T12:00:00Z"
        );
    }
}
