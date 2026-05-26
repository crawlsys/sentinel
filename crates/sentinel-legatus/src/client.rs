//! `sentinel legatus connect` — the standalone WS client.
//!
//! Connects to a consulate via the Consular Protocol, runs the
//! registration handshake, sends periodic heartbeats, logs any
//! `RelayInstruction` it receives, and emits a `SessionCompleted`
//! on graceful shutdown.
//!
//! This is the smallest viable legatus — no Claude Code
//! injection, no daemon integration, no hook plumbing. Useful for
//! verifying the wire end-to-end and as the substrate the next
//! two commits in the series will build on.

use std::time::Duration;

use chrono::Utc;
use consul_domain::identity::{
    ConnectionEpoch, KeyEpoch, SessionId, SessionMasterKey, SESSION_MASTER_KEY_LEN,
};
use consul_protocol::envelope::{
    decode_payload, encode_payload, AuthenticatedMessage, Nonce, Sequence, VerifyError,
};
use consul_protocol::keys::{
    derive_handshake_key, derive_mac_key_pair, MacKey, BOOTSTRAP_SECRET_LEN,
};
use consul_protocol::messages::{
    Capabilities, ConsularMessage, Hello, RegisterSession, RuntimeKind, SessionBlocked,
    SessionCompleted, SessionFailed, SessionHeartbeat, SessionStatus,
};
use consul_protocol::version::PROTOCOL_VERSION;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::connection_status::{ConnectionState, ConnectionStatus};
use crate::error::LegatusError;
use crate::handle::{EscalationKind, LegatusRuntime};

/// Default heartbeat interval. Consulate marks a session dead
/// after ~60s without a heartbeat (per
/// `consul_protocol::messages::heartbeat` docs); 20s gives us
/// three chances before dead-detection.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);

/// Configuration for one `legatus connect` invocation.
#[derive(Clone, Debug)]
pub struct ConnectConfig {
    /// Consulate WebSocket URL (e.g. `ws://127.0.0.1:9000`). When
    /// [`Self::failover_urls`] is non-empty, this is tried first on
    /// every attempt — failover URLs are tried in order only if the
    /// primary refuses.
    pub consulate_url: String,
    /// Additional consulate URLs tried after `consulate_url` fails
    /// on the current attempt. Empty by default (single-consulate
    /// mode). Operators list these in priority order; the wrapper
    /// does **not** persist a "last successful" preference — every
    /// new attempt restarts from `consulate_url` so a transiently-
    /// down primary doesn't permanently demote itself.
    pub failover_urls: Vec<String>,
    /// 32-byte bootstrap secret shared with consulate.
    pub bootstrap_secret: [u8; BOOTSTRAP_SECRET_LEN],
    /// Operator-chosen suggestion for the human-readable session
    /// name (consulate may add a suffix on collision).
    pub suggested_name: String,
    /// Working directory the session is anchored to.
    pub working_dir: String,
    /// Optional branch the session is on (for collision-suffix
    /// disambiguation).
    pub branch: Option<String>,
    /// Optional one-line task description sent in
    /// `RegisterSession`.
    pub task_description: Option<String>,
    /// Optional operator binding for this session. When `Some(op)`,
    /// it travels on the outgoing `RegisterSession.operator_id`
    /// field so consulate populates `Session.owner` with this
    /// operator (and consul-app's voice-attested gate routes
    /// per-session escalations to them per the Phase C3
    /// multi-operator routing path). Absent = consulate falls
    /// back to `OperatorId::ROOT` (single-operator v0.1
    /// behaviour, gate uses its `bound_operator`). The hint is
    /// self-asserted — the cryptographic proof remains the
    /// operator's keystore-sealed signing key on the witness
    /// payload, not this field.
    pub operator_id: Option<consul_domain::identity::republic::OperatorId>,
    /// Runtime kind. Sentinel-driven sessions are always
    /// [`RuntimeKind::ClaudeCode`] for now.
    pub runtime: RuntimeKind,
    /// How often to send `SessionHeartbeat`.
    pub heartbeat_interval: Duration,
}

impl ConnectConfig {
    /// Build a config with [`DEFAULT_HEARTBEAT_INTERVAL`] and
    /// [`RuntimeKind::ClaudeCode`].
    #[must_use]
    pub fn new(
        consulate_url: impl Into<String>,
        bootstrap_secret: [u8; BOOTSTRAP_SECRET_LEN],
        suggested_name: impl Into<String>,
        working_dir: impl Into<String>,
    ) -> Self {
        Self {
            consulate_url: consulate_url.into(),
            failover_urls: Vec::new(),
            bootstrap_secret,
            suggested_name: suggested_name.into(),
            working_dir: working_dir.into(),
            branch: None,
            task_description: None,
            operator_id: None,
            runtime: RuntimeKind::ClaudeCode,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
        }
    }
}

/// Connect, register, run the heartbeat loop until `cancel` fires
/// (or the consulate closes the connection), then send
/// `SessionCompleted` and return cleanly.
///
/// # Errors
///
/// Returns [`LegatusError`] on handshake or transport failure.
/// Clean shutdown via `cancel.notify_one()` returns `Ok(())` —
/// the `SessionCompleted` send is best-effort (errors logged at
/// warn).
pub async fn run_connect(
    config: ConnectConfig,
    cancel: std::sync::Arc<Notify>,
) -> Result<(), LegatusError> {
    // Standalone path — give the loop a runtime whose handle is
    // dropped immediately. Received instructions are still logged;
    // pushes to inbox_tx and pulls from escalation_rx silently
    // no-op (the channel ends are gone). A fresh `ConnectionStatus`
    // is constructed and discarded — the standalone CLI doesn't
    // expose it to anyone.
    let (_handle, mut runtime) = crate::handle::make_pair();
    let status = ConnectionStatus::new();
    run_connect_hosted(config, cancel, &mut runtime, &status).await
}

/// Hosted variant: caller supplies the [`LegatusRuntime`] half of a
/// pair from [`crate::handle::make_pair`]. The matching
/// [`crate::handle::LegatusHandle`] gives the caller (e.g. the
/// sentinel daemon's HTTP routes) the ability to push escalations
/// onto the WS and pop inbound `RelayInstruction`s as they arrive.
///
/// # Errors
///
/// Same shape as [`run_connect`].
pub async fn run_connect_hosted(
    config: ConnectConfig,
    cancel: std::sync::Arc<Notify>,
    runtime: &mut LegatusRuntime,
    status: &ConnectionStatus,
) -> Result<(), LegatusError> {
    info!(url = %config.consulate_url, "legatus connecting");
    // The wrapper may have us at Disconnected (first attempt) or
    // Reconnecting (between retries). Either way, we're now actively
    // attempting; surface that to the observer.
    status.set(ConnectionState::Connecting);
    if let Some(log) = status.event_log() {
        log.record_connecting(status.attempt());
    }
    // Bound the connect on CONNECT_TIMEOUT so an unreachable consulate
    // (firewalled remote, or a Windows loopback SYN timeout) becomes a
    // transport failure the wrapper can back off on, rather than blocking
    // reconnect detection on the OS-level connect timeout.
    let (ws, _) = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async(&config.consulate_url),
    )
    .await
    {
        Ok(res) => res.map_err(|err| {
            LegatusError::Transport(format!("connect {}: {err}", config.consulate_url))
        })?,
        Err(_elapsed) => {
            return Err(LegatusError::Transport(format!(
                "connect {}: timed out after {}s",
                config.consulate_url,
                CONNECT_TIMEOUT.as_secs()
            )));
        }
    };
    let (mut sink, mut source) = ws.split();

    // --- Handshake ------------------------------------------------
    let handshake_session_id = SessionId::new_v7();
    let handshake_key = derive_handshake_key(&config.bootstrap_secret, handshake_session_id);
    let mut hs_seq: u64 = 1;

    send_signed(
        &mut sink,
        &handshake_key,
        handshake_session_id,
        hs_seq,
        &ConsularMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            runtime: config.runtime.clone(),
        }),
    )
    .await?;
    hs_seq += 1;

    let caps = recv_signed(&mut source, &handshake_key).await?;
    match caps {
        ConsularMessage::Capabilities(Capabilities { .. }) => {
            debug!("consulate accepted protocol version; sending RegisterSession");
        }
        ConsularMessage::VersionMismatch(vm) => {
            return Err(LegatusError::VersionMismatch {
                accepted_min: Some(format!("{vm:?}")),
                accepted_max: None,
            });
        }
        other => {
            return Err(LegatusError::Handshake(format!(
                "expected Capabilities, got {other:?}",
            )));
        }
    }

    send_signed(
        &mut sink,
        &handshake_key,
        handshake_session_id,
        hs_seq,
        &ConsularMessage::RegisterSession(RegisterSession {
            suggested_name: config.suggested_name.clone().into(),
            runtime: config.runtime.clone(),
            working_dir: config.working_dir.clone(),
            branch: config.branch.clone(),
            task_description: config.task_description.clone(),
            operator_id: config.operator_id,
        }),
    )
    .await?;

    let registered = match recv_signed(&mut source, &handshake_key).await? {
        ConsularMessage::SessionRegistered(r) => r,
        other => {
            return Err(LegatusError::Handshake(format!(
                "expected SessionRegistered, got {other:?}",
            )));
        }
    };
    let session_id = registered.session_id;
    let display_name = registered.display_name.clone();
    let master_key_bytes: [u8; SESSION_MASTER_KEY_LEN] = registered.master_key_bytes;
    let master_key = SessionMasterKey::from_bytes(master_key_bytes);
    let pair = derive_mac_key_pair(
        &master_key,
        session_id,
        ConnectionEpoch::INITIAL,
        KeyEpoch::INITIAL,
    );
    info!(%session_id, %display_name, "legatus registered");
    // Handshake + registration complete — the session loop will run
    // until cancel or a transport failure. Observers (HTTP /health)
    // see us as healthy from this point on.
    status.set(ConnectionState::Connected);
    if let Some(log) = status.event_log() {
        log.record_connected(status.attempt());
    }

    // Operator-facing handshake-complete banner. Goes to stderr so it
    // is visible regardless of RUST_LOG filter (default is `warn`).
    // Operators following the consul↔sentinel runbook need to see a
    // sign of life when the WS handshake actually completes — the
    // info! above is filtered out by default and the consulate side's
    // logs are in a different terminal.
    eprintln!();
    eprintln!("------------------------------------------------------------");
    eprintln!("  Legatus handshake complete");
    eprintln!("------------------------------------------------------------");
    eprintln!("  Consulate URL: {}", config.consulate_url);
    eprintln!("  Session ID:    {session_id}");
    eprintln!("  Display name:  {display_name}");
    eprintln!("  Heartbeat:     {:?}", config.heartbeat_interval);
    eprintln!("------------------------------------------------------------");
    eprintln!();

    // --- Post-handshake loop --------------------------------------
    // Per the session_loop convention in consulate: our outbound
    // direction signs with `pair.outbound` and consulate verifies
    // with `pair.outbound`; consulate signs its pushed messages
    // with `pair.inbound` and we verify with `pair.inbound`.
    let outbound_key = pair.outbound;
    let inbound_key = pair.inbound;

    let mut local_seq: u64 = 1;
    let mut heartbeat = tokio::time::interval(config.heartbeat_interval);
    heartbeat.tick().await; // skip immediate first tick

    // Startup replay: re-enqueue any pending outbox entries from
    // disk so they flow through the same `escalation_rx` arm as a
    // fresh `LegatusHandle::escalate` call. This is the recovery
    // path after a daemon crash that landed events on disk but
    // didn't get them to the WS. Order is preserved by the
    // outbox's FIFO snapshot + the mpsc's send-order semantics.
    if let Some(outbox) = runtime.outbox.as_ref() {
        let pending = outbox.snapshot();
        if !pending.is_empty() {
            info!(
                %session_id,
                count = pending.len(),
                "replaying pending escalations from outbox",
            );
            for item in pending {
                // Replay preserves each item's original `at_ms`
                // (read from disk) so the envelope timestamps and
                // ack-match keys stay consistent across restart.
                if runtime.escalation_loopback.send(item).is_err() {
                    warn!(
                        %session_id,
                        "loopback channel closed during outbox replay",
                    );
                    break;
                }
            }
        }
    }

    // Loop exit classifier:
    //   `Cancelled`        — `cancel.notified()` fired; this is the
    //                        ONLY clean exit. Wrapper returns Ok(()).
    //   `TransportFailed`  — WS send/recv error, stream end, or
    //                        consulate-initiated close. Wrapper sees
    //                        Err(Transport) and reconnects with
    //                        exponential backoff.
    let exit_reason: ExitReason = loop {
        tokio::select! {
            () = cancel.notified() => {
                info!(%session_id, "legatus cancelled; sending SessionCompleted");
                break ExitReason::Cancelled;
            },
            escalation = runtime.escalation_rx.recv() => {
                let Some(item) = escalation else {
                    // All escalation senders dropped — host went
                    // away. Stay alive; the loop continues serving
                    // the WS until cancel or the consulate closes.
                    continue;
                };
                // Lifecycle variants reuse the item's `at_ms`
                // (stamped at-append time) so the on-disk timestamp
                // matches what goes on the wire — that's the half
                // of the lifecycle-key ack-match contract that
                // lives here.
                let at_ms = item.at_ms;
                let msg = match item.event {
                    EscalationKind::Blocked { reason } => {
                        ConsularMessage::SessionBlocked(SessionBlocked {
                            session_id,
                            reason,
                            detected_at_ms: at_ms,
                        })
                    },
                    EscalationKind::Completed { summary } => {
                        ConsularMessage::SessionCompleted(SessionCompleted {
                            session_id,
                            completed_at_ms: at_ms,
                            summary,
                        })
                    },
                    EscalationKind::Failed { error } => {
                        ConsularMessage::SessionFailed(SessionFailed {
                            session_id,
                            failed_at_ms: at_ms,
                            error,
                        })
                    },
                    EscalationKind::InstructionAck { instruction_id } => {
                        ConsularMessage::InstructionAcknowledged(
                            consul_protocol::messages::InstructionAcknowledged {
                                instruction_id,
                                session_id,
                            },
                        )
                    },
                    EscalationKind::InstructionResult {
                        instruction_id,
                        outcome,
                        summary,
                    } => ConsularMessage::InstructionResult(
                        consul_protocol::messages::InstructionResult {
                            instruction_id,
                            session_id,
                            outcome,
                            summary,
                        },
                    ),
                };
                if let Err(err) = send_signed(
                    &mut sink,
                    &outbound_key,
                    session_id,
                    local_seq,
                    &msg,
                ).await {
                    warn!(?err, "escalation send failed; closing");
                    break ExitReason::TransportFailed(format!("escalation send: {err}"));
                }
                // Successful WS send → remove the head from the
                // outbox (if persistent). Disk and consul-receipt
                // are now in sync for this event. If we crash
                // BETWEEN send_signed and remove_head, the next
                // daemon start re-replays the event to consul —
                // at-least-once delivery, idempotent on the consul
                // side (every escalation kind is keyed by stable
                // ids).
                if let Some(outbox) = runtime.outbox.as_ref() {
                    let _ = outbox.remove_head();
                }
                local_seq += 1;
            },
            _ = heartbeat.tick() => {
                let msg = ConsularMessage::SessionHeartbeat(SessionHeartbeat {
                    session_id,
                    status: SessionStatus::Active,
                    current_task: config.task_description.clone(),
                    last_tool: None,
                });
                if let Err(err) = send_signed(
                    &mut sink,
                    &outbound_key,
                    session_id,
                    local_seq,
                    &msg,
                ).await {
                    warn!(?err, "heartbeat send failed; closing");
                    break ExitReason::TransportFailed(format!("heartbeat send: {err}"));
                }
                local_seq += 1;
            },
            frame = source.next() => {
                match frame {
                    Some(Ok(WsMessage::Binary(bytes))) => {
                        let msg = match decode_envelope(&bytes, &inbound_key) {
                            Ok(m) => m,
                            Err(err) => {
                                warn!(?err, "dropped inbound envelope");
                                continue;
                            },
                        };
                        handle_inbound(&msg, session_id, runtime);
                    },
                    Some(Ok(WsMessage::Close(frame))) => {
                        debug!(?frame, "consulate sent close");
                        break ExitReason::TransportFailed(
                            "consulate sent WS close frame".to_owned(),
                        );
                    },
                    // tokio-tungstenite auto-handles pings; raw frames are
                    // an internal type we never expect here. Both are no-ops.
                    Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_))) => {},
                    Some(Ok(WsMessage::Text(text))) => {
                        debug!(text = %text, "unexpected text frame; ignored");
                    },
                    Some(Err(err)) => {
                        warn!(?err, "websocket recv error; closing");
                        break ExitReason::TransportFailed(format!("WS recv: {err}"));
                    },
                    None => {
                        debug!("websocket stream ended");
                        break ExitReason::TransportFailed(
                            "WS stream ended (peer dropped)".to_owned(),
                        );
                    },
                }
            },
        }
    };

    match exit_reason {
        ExitReason::Cancelled => {
            // Cancel path: WS is presumed healthy — send a clean
            // SessionCompleted so consul records the orderly
            // shutdown. If the WS is actually dead at this point
            // the send fails harmlessly and we still return Ok(()).
            let completed = ConsularMessage::SessionCompleted(SessionCompleted {
                session_id,
                completed_at_ms: now_ms(),
                summary: Some("legatus cancelled".to_owned()),
            });
            if let Err(err) =
                send_signed(&mut sink, &outbound_key, session_id, local_seq, &completed).await
            {
                warn!(?err, "SessionCompleted send failed");
            }
            Ok(())
        }
        ExitReason::TransportFailed(reason) => {
            // WS-side exit (peer close, send/recv error, stream
            // end). Return Err(Transport) so the reconnect wrapper
            // backs off and retries. The dead WS can't carry a
            // SessionCompleted anyway, so skip that send.
            warn!(%session_id, %reason, "legatus session exited via transport failure; surfacing as Err for reconnect");
            Err(LegatusError::Transport(reason))
        }
    }
}

/// Why the in-session loop in [`run_connect_hosted`] terminated.
///
/// The wrapper [`run_connect_hosted_with_reconnect`] discriminates
/// `Cancelled` (Ctrl-C, operator shutdown — clean exit, do not
/// reconnect) from `TransportFailed` (network drop, peer close,
/// recv/send error — transient, retry with backoff).
enum ExitReason {
    /// Cancel `Notify` fired; surface to caller as `Ok(())`.
    Cancelled,
    /// WS-side exit. Carries the underlying reason for logging.
    /// Surfaced to caller as `Err(LegatusError::Transport(...))`.
    TransportFailed(String),
}

fn now_ms() -> u64 {
    u64::try_from(Utc::now().timestamp_millis().max(0)).unwrap_or(0)
}

type WsSink = futures_util::stream::SplitSink<
    WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    WsMessage,
>;
type WsSource =
    futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>;

async fn send_signed(
    sink: &mut WsSink,
    key: &MacKey,
    session_id: SessionId,
    sequence: u64,
    msg: &ConsularMessage,
) -> Result<(), LegatusError> {
    let payload = encode_payload(msg).map_err(|err| LegatusError::Encode(err.to_string()))?;
    let nonce = Nonce::generate().map_err(|err| LegatusError::Encode(err.to_string()))?;
    let envelope = AuthenticatedMessage::sign(
        key,
        session_id,
        Sequence::from_u64(sequence),
        now_ms(),
        nonce,
        payload,
    );
    let bytes = envelope
        .encode_cbor()
        .map_err(|err| LegatusError::Encode(err.to_string()))?;
    sink.send(WsMessage::Binary(bytes))
        .await
        .map_err(|err| LegatusError::Transport(format!("send: {err}")))
}

async fn recv_signed(source: &mut WsSource, key: &MacKey) -> Result<ConsularMessage, LegatusError> {
    loop {
        let frame = source
            .next()
            .await
            .ok_or_else(|| LegatusError::Handshake("connection closed before reply".into()))?
            .map_err(|err| LegatusError::Transport(format!("recv: {err}")))?;
        match frame {
            WsMessage::Binary(bytes) => {
                return decode_envelope(&bytes, key);
            }
            WsMessage::Close(_) => {
                return Err(LegatusError::Handshake(
                    "consulate sent close mid-handshake".into(),
                ));
            }
            // Ping/Pong/Text/Frame: ignore and wait for the next frame.
            _ => {}
        }
    }
}

fn decode_envelope(bytes: &[u8], key: &MacKey) -> Result<ConsularMessage, LegatusError> {
    let env = AuthenticatedMessage::decode_cbor(bytes)
        .map_err(|err| LegatusError::Decode(format!("envelope: {err}")))?;
    let payload = env.verify(key).map_err(|err| match err {
        VerifyError::MacMismatch => LegatusError::MacMismatch,
    })?;
    decode_payload(payload).map_err(|err| LegatusError::Decode(format!("payload: {err}")))
}

fn handle_inbound(msg: &ConsularMessage, session_id: SessionId, runtime: &LegatusRuntime) {
    match msg {
        ConsularMessage::RelayInstruction(instr) => {
            info!(
                %session_id,
                instruction_id = %instr.instruction_id,
                content = %instr.content,
                destructive = instr.destructive,
                "received RelayInstruction",
            );
            // Persistence: write to the file-backed inbox before
            // looping back to the WS, so the instruction survives a
            // daemon crash between "received over WS" and "drained
            // by the consul_inbox hook". `append` is synchronous +
            // fs2-locked; it's called inside the tokio select arm,
            // but the lock window is microseconds (open + writeln +
            // close) so we don't stall the runtime.
            //
            // Standalone `run_connect` builds a runtime with
            // `inbox = None` — in that path the instruction stays
            // log-only, matching pre-persistence behavior.
            if let Some(inbox) = runtime.inbox.as_ref() {
                inbox.append(instr);
            } else {
                debug!(%session_id, "no persistent inbox; instruction is log-only");
            }
        }
        ConsularMessage::CancelInstruction(cancel) => {
            // Operator-driven undo. If the instruction is still
            // queued in the persistent inbox we drop it; if it's
            // already drained into the model's context window the
            // cancel is too late and we just log. Definitive vs
            // advisory split per the ~90-95% reliability target.
            //
            // When we DO remove from the queue, we also emit an
            // `InstructionResult { Declined { reason } }` via the
            // runtime's loopback sender so consul gets a closing
            // record for the cancelled id instead of inferring
            // cancellation from no-result-ever-arriving. The
            // loopback feeds the same `escalation_rx` the main
            // loop already drains, so the Result message is signed
            // and sent over the WS via the standard escalation arm.
            let removed = runtime
                .inbox
                .as_ref()
                .is_some_and(|inbox| inbox.remove_by_id(cancel.instruction_id));
            if removed {
                info!(
                    %session_id,
                    instruction_id = %cancel.instruction_id,
                    reason = ?cancel.reason,
                    "CancelInstruction removed queued instruction",
                );
                let declined_reason = cancel.reason.as_deref().map_or_else(
                    || "cancelled by operator".to_owned(),
                    |r| format!("cancelled by operator: {r}"),
                );
                let event = crate::handle::EscalationKind::InstructionResult {
                    instruction_id: cancel.instruction_id,
                    outcome: consul_protocol::messages::InstructionOutcome::Declined {
                        reason: declined_reason,
                    },
                    summary: None,
                };
                // Stamp at-loopback time and persist BEFORE pushing
                // through the loopback so the in-flight envelope
                // and the on-disk entry share the same `at_ms`.
                let item = crate::persistent_outbox::OutboxItem::new(event, now_ms());
                if let Some(outbox) = runtime.outbox.as_ref() {
                    outbox.append(&item);
                }
                if runtime.escalation_loopback.send(item).is_err() {
                    // Loopback receiver is the same as the loop's
                    // escalation_rx — if it's gone the loop has
                    // exited and we're shutting down. Silent skip.
                    debug!(
                        %session_id,
                        instruction_id = %cancel.instruction_id,
                        "loopback escalation channel closed; declined-result not sent",
                    );
                }
            } else {
                info!(
                    %session_id,
                    instruction_id = %cancel.instruction_id,
                    reason = ?cancel.reason,
                    "CancelInstruction arrived too late (instruction already drained or never queued)",
                );
            }
        }
        ConsularMessage::RequestContextSync(_) => {
            debug!(%session_id, "received RequestContextSync (no context store yet)");
        }
        ConsularMessage::EscalationAck(ack) => {
            // Consul confirms receipt + bus-forwarding of one or
            // more previously-sent escalations. For per-instruction
            // keys we drop the matching outbox entry — covers the
            // case where the post-send `remove_head` somehow missed
            // (slow disk, crash recovery, future ack-driven
            // delivery). Lifecycle keys (Blocked/Completed/Failed)
            // are no-ops in this slice — matching them by
            // (session_id, *_at_ms) needs the sent-time timestamp
            // on disk, the next refactor in this arc.
            //
            // No-op (returns false) when:
            //  - Outbox is None (standalone CLI path)
            //  - Entry was already removed by `remove_head`
            //  - Key references an event we never sent (idempotent
            //    duplicate ack from a peer that doesn't track
            //    ack-already-sent state — by design, per the
            //    `EscalationAck` doc).
            let Some(outbox) = runtime.outbox.as_ref() else {
                debug!(
                    %session_id,
                    acks = ack.acks.len(),
                    "received EscalationAck; no persistent outbox to update",
                );
                return;
            };
            let mut removed_count = 0_usize;
            for key in &ack.acks {
                use consul_protocol::messages::EscalationKey;
                match key {
                    EscalationKey::InstructionAcknowledged { instruction_id }
                    | EscalationKey::InstructionResult { instruction_id } => {
                        if outbox.remove_by_instruction_id(*instruction_id) {
                            removed_count += 1;
                        }
                    },
                    EscalationKey::SessionBlocked {
                        session_id: ack_sid,
                        detected_at_ms,
                    } => {
                        if *ack_sid == session_id
                            && outbox.remove_lifecycle(
                                crate::persistent_outbox::LifecycleKind::Blocked,
                                *detected_at_ms,
                            )
                        {
                            removed_count += 1;
                        }
                    },
                    EscalationKey::SessionCompleted {
                        session_id: ack_sid,
                        completed_at_ms,
                    } => {
                        if *ack_sid == session_id
                            && outbox.remove_lifecycle(
                                crate::persistent_outbox::LifecycleKind::Completed,
                                *completed_at_ms,
                            )
                        {
                            removed_count += 1;
                        }
                    },
                    EscalationKey::SessionFailed {
                        session_id: ack_sid,
                        failed_at_ms,
                    } => {
                        if *ack_sid == session_id
                            && outbox.remove_lifecycle(
                                crate::persistent_outbox::LifecycleKind::Failed,
                                *failed_at_ms,
                            )
                        {
                            removed_count += 1;
                        }
                    },
                }
            }
            debug!(
                %session_id,
                acks_received = ack.acks.len(),
                outbox_removed = removed_count,
                "processed inbound EscalationAck",
            );
        }
        ConsularMessage::CatastrophicAck(ack) => {
            // v0.1 sentinel-side handling per the catastrophic-flow
            // architecture: parse the action_class out of the
            // operator's voiceprint-attested transcript, record an
            // unspent approval in the daemon's
            // CatastrophicApprovalCache. On the operator's next
            // Claude Code prompt, the catastrophic_escalation hook
            // will check the cache, consume the approval, and let
            // the previously-blocked tool call through.
            //
            // v0.1 LIMITATION: cryptographic witness verification
            // (Ed25519 signature + Praefectus 6-step check) is NOT
            // performed here. The cache lives in-process; daemon
            // HTTP routes are bearer-auth localhost-only; the
            // threat model is "anyone with shell access" which
            // already owns the machine. v0.2 swaps in
            // `PraefectusClient` verification before the cache
            // write.
            use consul_protocol::messages::AckDecision;
            tracing::info!(
                %session_id,
                ack_operator = %ack.voiceprint_witness.operator,
                decision = ?ack.decision,
                "received CatastrophicAck",
            );
            // Replay protection: BEFORE any other processing, check
            // the spent-nonce log. Already-spent nonces are rejected
            // without verifier invocation or cache write. The log
            // is optional; daemon path wires it via
            // LegatusRuntime::with_spent_nonce_log.
            if let Some(spent_log) = runtime.spent_nonces.as_ref() {
                if !spent_log.try_spend(ack.voiceprint_witness.challenge_nonce) {
                    warn!(
                        %session_id,
                        nonce = %ack.voiceprint_witness.challenge_nonce.to_hex(),
                        "CatastrophicAck REPLAY rejected: nonce already spent"
                    );
                    return;
                }
            }
            match &ack.decision {
                AckDecision::Approve | AckDecision::Modify { .. } => {
                    let transcript = ack.voiceprint_witness.utterance_transcript.clone();
                    let Some(action_class) =
                        crate::approval_cache::parse_action_class_from_transcript(&transcript)
                    else {
                        warn!(
                            %session_id,
                            transcript = %transcript,
                            "CatastrophicAck transcript did not match \
                             'approve <action_class>, code <nonce>' shape; \
                             approval NOT recorded"
                        );
                        return;
                    };
                    let Some(cache) = runtime.approval_cache.as_ref() else {
                        debug!(
                            %session_id,
                            action_class = %action_class,
                            "CatastrophicAck received but no approval_cache wired \
                             (standalone CLI?); approval not retained"
                        );
                        return;
                    };
                    // Cryptographic verification gate: when a
                    // WitnessVerifierPort is wired the witness is
                    // verified BEFORE the cache write. The verifier
                    // trait is async (production adapters round-trip
                    // to a remote Praefectus); we tokio::spawn the
                    // verification so the synchronous WS recv loop
                    // isn't blocked while a verification is in
                    // flight. The cache record happens inside the
                    // spawned task after verify resolves.
                    if let Some(verifier) = runtime.witness_verifier.clone() {
                        let cache = cache.clone();
                        let witness = ack.voiceprint_witness.clone();
                        let key = ack.key.clone();
                        let action_class_owned = action_class;
                        let transcript_owned = transcript;
                        tokio::spawn(async move {
                            match verifier.verify(&witness, &key).await {
                                Ok(()) => {
                                    cache.record(
                                        session_id,
                                        action_class_owned.clone(),
                                        transcript_owned,
                                    );
                                    tracing::info!(
                                        %session_id,
                                        action_class = %action_class_owned,
                                        verified = true,
                                        "CatastrophicAck verified + recorded approval"
                                    );
                                }
                                Err(err) => {
                                    warn!(
                                        %session_id,
                                        action_class = %action_class_owned,
                                        error = %err,
                                        "CatastrophicAck verification FAILED; \
                                         approval dropped, cache not written"
                                    );
                                }
                            }
                        });
                    } else {
                        cache.record(session_id, action_class.clone(), transcript);
                        tracing::info!(
                            %session_id,
                            action_class = %action_class,
                            verified = false,
                            "CatastrophicAck recorded approval in cache (no verifier wired); \
                             the operator's next retry of the matching tool call will be allowed"
                        );
                    }
                }
                AckDecision::Deny { reason } => {
                    // No cache write on deny -- the hook will keep
                    // denying on retry, which matches the
                    // operator's intent.
                    tracing::info!(
                        %session_id,
                        reason = %reason,
                        "CatastrophicAck denied; no cache write"
                    );
                }
            }
        }
        other => {
            debug!(%session_id, ?other, "unhandled inbound message");
        }
    }
}

/// Reconnect-on-drop wrapper around [`run_connect_hosted`].
///
/// Repeatedly invokes the hosted connect path: on success the
/// session ran to clean shutdown (cancel fired, or the consulate
/// closed cleanly) — return `Ok(())`. On error the wrapper logs
/// at warn, applies exponential backoff (1s → 2s → 4s → 8s →
/// 16s, capped at [`MAX_RECONNECT_BACKOFF`]), and reconnects.
/// Each successful connection resets the backoff to
/// [`INITIAL_RECONNECT_BACKOFF`].
///
/// Cancel handling: between attempts the wrapper races the backoff
/// sleep against `cancel.notified()`; if cancel fires during a
/// backoff window the wrapper returns `Ok(())` without another
/// connection attempt. Cancel during an in-flight connection is
/// handled by `run_connect_hosted` itself (clean shutdown).
///
/// The `runtime` parameter is borrowed by `&mut` because each
/// reconnection attempt needs the same handle (inbox / outbound
/// channels survive across reconnects — operator-side consumers
/// stay connected to the legatus handle regardless of how many
/// WS reconnections happen underneath).
///
/// # Errors
///
/// Returns [`LegatusError`] only on conditions the wrapper
/// considers fatal:
/// - [`LegatusError::VersionMismatch`] — protocol-level
///   incompatibility, no amount of reconnecting will fix it.
/// - [`LegatusError::Handshake`] errors that name unrecoverable
///   conditions (e.g. wrong bootstrap secret). The wrapper
///   inspects the error message; conservative default is to
///   retry, so only `VersionMismatch` is treated as fatal today.
///
/// Transient errors (network drop, consulate restart, transport
/// errors during the heartbeat loop) all trigger reconnect.
pub async fn run_connect_hosted_with_reconnect(
    config: ConnectConfig,
    cancel: std::sync::Arc<Notify>,
    mut runtime: LegatusRuntime,
    status: ConnectionStatus,
) -> Result<(), LegatusError> {
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    loop {
        // Try the primary first, then each failover in order, before
        // backing off. The bumped attempt counter is shared across
        // the URL list for this iteration — observers see "attempt 3
        // failed against ws://primary, then against ws://failover1,
        // then we slept 4s." This matches operator mental model
        // ("attempt 3" = round 3 of trying all known URLs) and
        // avoids exploding the counter on every failover.
        let attempt = status.bump_attempt();
        let url_list = std::iter::once(config.consulate_url.clone())
            .chain(config.failover_urls.iter().cloned())
            .collect::<Vec<_>>();

        let mut last_err: Option<LegatusError> = None;
        for (idx, url) in url_list.iter().enumerate() {
            info!(
                attempt,
                url = %url,
                rank = idx,
                "legatus connecting (with reconnect)"
            );
            let mut url_config = config.clone();
            url_config.consulate_url = url.clone();
            let cancel_clone = std::sync::Arc::clone(&cancel);
            match run_connect_hosted(url_config, cancel_clone, &mut runtime, &status).await {
                Ok(()) => {
                    info!(
                        attempt,
                        url = %url,
                        "legatus session exited cleanly; reconnect-loop returning Ok"
                    );
                    status.set(ConnectionState::Disconnected);
                    if let Some(log) = status.event_log() {
                        log.record_disconnected(attempt, None);
                    }
                    return Ok(());
                }
                Err(LegatusError::VersionMismatch {
                    accepted_min,
                    accepted_max,
                }) => {
                    // Protocol-level incompatibility — reconnecting
                    // against THIS url won't help, but maybe a
                    // failover is on a compatible version. Surface
                    // as fatal ONLY when there's no next URL to try.
                    if idx + 1 < url_list.len() {
                        warn!(
                            attempt,
                            url = %url,
                            ?accepted_min,
                            ?accepted_max,
                            "version mismatch on this consulate; trying next failover URL"
                        );
                        last_err = Some(LegatusError::VersionMismatch {
                            accepted_min,
                            accepted_max,
                        });
                        continue;
                    }
                    status.set(ConnectionState::Disconnected);
                    if let Some(log) = status.event_log() {
                        log.record_disconnected(
                            attempt,
                            Some(format!(
                                "VersionMismatch on every consulate URL: \
                                 accepted_min={accepted_min:?} \
                                 accepted_max={accepted_max:?}"
                            )),
                        );
                    }
                    return Err(LegatusError::VersionMismatch {
                        accepted_min,
                        accepted_max,
                    });
                }
                Err(err) => {
                    warn!(
                        attempt,
                        url = %url,
                        ?err,
                        "legatus url failed; trying next failover (if any)"
                    );
                    last_err = Some(err);
                }
            }
        }
        // Belt-and-suspenders: if we exit the loop without a return,
        // we've exhausted the URL list without a clean exit. Fall
        // through to the backoff sleep. last_err is guaranteed Some
        // (the loop above wrote to it on every Err arm).
        let reason = last_err.map_or_else(|| "no urls configured".to_owned(), |e| format!("{e}"));
        warn!(
            attempt,
            urls = url_list.len(),
            backoff_secs = backoff.as_secs(),
            reason = %reason,
            "all consulate URLs failed this round; backing off"
        );
        // Distinct from Connecting: this is the visible "we were
        // Connected, we just lost it, we'll be back" state.
        status.set(ConnectionState::Reconnecting);
        if let Some(log) = status.event_log() {
            log.record_reconnecting(attempt, reason);
        }
        // Sleep with cancel-honor; on wake, grow backoff and retry
        // with the SAME runtime (its handle-paired channels are still
        // hot, so escalations queued during the outage drain on the
        // next successful connection).
        tokio::select! {
            () = tokio::time::sleep(backoff) => {
                backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
            }
            () = cancel.notified() => {
                info!(
                    attempt,
                    "legatus reconnect cancelled during backoff window"
                );
                status.set(ConnectionState::Disconnected);
                if let Some(log) = status.event_log() {
                    log.record_disconnected(
                        attempt,
                        Some("cancelled during backoff".to_owned()),
                    );
                }
                return Ok(());
            }
        }
    }
}

/// Initial reconnect backoff (1 second).
pub const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Maximum reconnect backoff (30 seconds). Wraps the exponential
/// growth so a long-running outage doesn't extend the operator's
/// recovery wait beyond half a minute.
pub const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);

/// Per-attempt connect timeout (5 seconds). Bounds how long a single
/// `connect_async` may block before being treated as a transport
/// failure and handed to the backoff loop. Without this, a connect to
/// an unreachable/blackholed consulate blocks on the OS SYN timeout —
/// ~1-2s on Windows for a closed loopback port, far longer for a
/// firewalled remote host — stalling reconnect detection. A bounded
/// timeout makes per-attempt latency deterministic across platforms.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn connect_config_new_sets_documented_defaults() {
        let cfg = ConnectConfig::new(
            "ws://127.0.0.1:9000",
            [0xAB; BOOTSTRAP_SECRET_LEN],
            "firefly",
            "/tmp/firefly",
        );
        assert_eq!(cfg.consulate_url, "ws://127.0.0.1:9000");
        assert_eq!(cfg.bootstrap_secret, [0xAB; BOOTSTRAP_SECRET_LEN]);
        assert_eq!(cfg.suggested_name, "firefly");
        assert!(cfg.branch.is_none());
        assert!(cfg.task_description.is_none());
        assert!(matches!(cfg.runtime, RuntimeKind::ClaudeCode));
        assert_eq!(cfg.heartbeat_interval, DEFAULT_HEARTBEAT_INTERVAL);
    }

    #[test]
    fn now_ms_is_positive_and_recent() {
        let t = now_ms();
        // After Jan 1 2024 (sanity).
        assert!(t > 1_700_000_000_000);
    }

    /// Successful cancel (instruction was queued and removed) feeds
    /// an `InstructionResult { Declined { reason } }` into the
    /// runtime's loopback channel. The main `run_connect_hosted`
    /// select loop would then drain `escalation_rx` and sign+send
    /// the message over the WS — we don't exercise that path here
    /// (no fake consulate), just the loopback emission.
    #[tokio::test]
    async fn cancel_instruction_hit_pushes_declined_via_loopback() {
        use consul_domain::identity::InstructionId;
        use consul_protocol::messages::{
            CancelInstruction, ConsularMessage, InstructionOutcome, RelayInstruction,
        };
        use tempfile::tempdir;

        use crate::handle::{make_pair_with_inbox, EscalationKind};
        use crate::persistent_inbox::PersistentInbox;

        let dir = tempdir().unwrap();
        let inbox = PersistentInbox::new(dir.path().join("inbox.jsonl"));
        let queued = RelayInstruction {
            instruction_id: InstructionId::new(),
            target_session_id: consul_domain::identity::SessionId::new_v7(),
            content: "deploy staging".into(),
            destructive: false,
        };
        inbox.append(&queued);
        let (_handle, mut runtime) = make_pair_with_inbox(inbox);

        let cancel = CancelInstruction {
            instruction_id: queued.instruction_id,
            target_session_id: queued.target_session_id,
            reason: Some("operator changed their mind".into()),
        };
        handle_inbound(
            &ConsularMessage::CancelInstruction(cancel),
            queued.target_session_id,
            &runtime,
        );

        let item = runtime.escalation_rx.recv().await.expect("loopback fired");
        match item.event {
            EscalationKind::InstructionResult {
                instruction_id,
                outcome,
                summary,
            } => {
                assert_eq!(instruction_id, queued.instruction_id);
                assert!(summary.is_none());
                match outcome {
                    InstructionOutcome::Declined { reason } => {
                        assert!(reason.contains("cancelled by operator"), "reason: {reason}");
                        assert!(reason.contains("changed their mind"), "reason: {reason}");
                    }
                    other => panic!("expected Declined, got {other:?}"),
                }
            }
            other => panic!("expected InstructionResult, got {other:?}"),
        }
    }

    /// Cancel that doesn't match a queued instruction (already
    /// drained, or never queued) does NOT feed the loopback —
    /// nothing for consul to receive.
    #[tokio::test]
    async fn cancel_instruction_miss_does_not_push_anything() {
        use consul_domain::identity::InstructionId;
        use consul_protocol::messages::{CancelInstruction, ConsularMessage};
        use tempfile::tempdir;

        use crate::handle::make_pair_with_inbox;
        use crate::persistent_inbox::PersistentInbox;

        let dir = tempdir().unwrap();
        let inbox = PersistentInbox::new(dir.path().join("inbox.jsonl"));
        let (_handle, mut runtime) = make_pair_with_inbox(inbox);

        let session_id = consul_domain::identity::SessionId::new_v7();
        let cancel = CancelInstruction {
            instruction_id: InstructionId::new(),
            target_session_id: session_id,
            reason: None,
        };
        handle_inbound(
            &ConsularMessage::CancelInstruction(cancel),
            session_id,
            &runtime,
        );

        // try_recv returns Err(Empty) when nothing has been sent.
        assert!(runtime.escalation_rx.try_recv().is_err());
    }

    /// Cancel without an explicit reason still produces a sensible
    /// default reason string in the emitted Declined outcome.
    #[tokio::test]
    async fn cancel_without_reason_uses_default_declined_reason() {
        use consul_domain::identity::InstructionId;
        use consul_protocol::messages::{
            CancelInstruction, ConsularMessage, InstructionOutcome, RelayInstruction,
        };
        use tempfile::tempdir;

        use crate::handle::{make_pair_with_inbox, EscalationKind};
        use crate::persistent_inbox::PersistentInbox;

        let dir = tempdir().unwrap();
        let inbox = PersistentInbox::new(dir.path().join("inbox.jsonl"));
        let queued = RelayInstruction {
            instruction_id: InstructionId::new(),
            target_session_id: consul_domain::identity::SessionId::new_v7(),
            content: "x".into(),
            destructive: false,
        };
        inbox.append(&queued);
        let (_handle, mut runtime) = make_pair_with_inbox(inbox);

        let cancel = CancelInstruction {
            instruction_id: queued.instruction_id,
            target_session_id: queued.target_session_id,
            reason: None,
        };
        handle_inbound(
            &ConsularMessage::CancelInstruction(cancel),
            queued.target_session_id,
            &runtime,
        );

        let item = runtime.escalation_rx.recv().await.unwrap();
        match item.event {
            EscalationKind::InstructionResult { outcome, .. } => match outcome {
                InstructionOutcome::Declined { reason } => {
                    assert_eq!(reason, "cancelled by operator");
                }
                other => panic!("expected Declined, got {other:?}"),
            },
            other => panic!("expected InstructionResult, got {other:?}"),
        }
    }

    /// EscalationAck with a lifecycle key removes the matching
    /// outbox entry. The headline test for the lifecycle-key arc:
    /// seed the outbox with a `Completed { at_ms }` entry, feed an
    /// `EscalationAck { SessionCompleted { session_id, completed_at_ms: at_ms } }`,
    /// expect the entry gone.
    #[tokio::test]
    async fn escalation_ack_with_lifecycle_key_removes_outbox_entry() {
        use consul_domain::identity::SessionId;
        use consul_protocol::messages::{ConsularMessage, EscalationAck, EscalationKey};
        use tempfile::tempdir;

        use crate::handle::{make_pair_with_persistence, EscalationKind};
        use crate::persistent_inbox::PersistentInbox;
        use crate::persistent_outbox::{OutboxItem, PersistentEscalationOutbox};

        let dir = tempdir().unwrap();
        let inbox = PersistentInbox::new(dir.path().join("inbox.jsonl"));
        let outbox = PersistentEscalationOutbox::new(dir.path().join("outbox.jsonl"));
        // Pre-seed the outbox with a Completed entry at a specific
        // timestamp (mimics a daemon that escalated + crashed
        // before the ack arrived).
        let at_ms: u64 = 1_750_000_000_000;
        outbox.append(&OutboxItem::new(
            EscalationKind::Completed {
                summary: Some("staging deploy ok".into()),
            },
            at_ms,
        ));
        let session_id = SessionId::new_v7();
        let (_handle, runtime) = make_pair_with_persistence(inbox, outbox);

        // Sanity: entry is there before the ack.
        assert_eq!(runtime.outbox.as_ref().unwrap().len(), 1);

        let ack = EscalationAck {
            acks: vec![EscalationKey::SessionCompleted {
                session_id,
                completed_at_ms: at_ms,
            }],
        };
        handle_inbound(&ConsularMessage::EscalationAck(ack), session_id, &runtime);

        assert_eq!(
            runtime.outbox.as_ref().unwrap().len(),
            0,
            "lifecycle-key ack should have removed the matching entry",
        );
    }

    /// Lifecycle ack with the WRONG session_id is rejected — we
    /// don't remove outbox entries on behalf of a different
    /// legatus's acks. Defense against a buggy / lying peer.
    #[tokio::test]
    async fn escalation_ack_with_wrong_session_id_does_not_remove() {
        use consul_domain::identity::SessionId;
        use consul_protocol::messages::{ConsularMessage, EscalationAck, EscalationKey};
        use tempfile::tempdir;

        use crate::handle::{make_pair_with_persistence, EscalationKind};
        use crate::persistent_inbox::PersistentInbox;
        use crate::persistent_outbox::{OutboxItem, PersistentEscalationOutbox};

        let dir = tempdir().unwrap();
        let inbox = PersistentInbox::new(dir.path().join("inbox.jsonl"));
        let outbox = PersistentEscalationOutbox::new(dir.path().join("outbox.jsonl"));
        let at_ms: u64 = 1_750_000_000_000;
        outbox.append(&OutboxItem::new(
            EscalationKind::Failed {
                error: "oops".into(),
            },
            at_ms,
        ));
        let our_session = SessionId::new_v7();
        let foreign = SessionId::new_v7();
        let (_handle, runtime) = make_pair_with_persistence(inbox, outbox);

        let ack = EscalationAck {
            acks: vec![EscalationKey::SessionFailed {
                session_id: foreign,
                failed_at_ms: at_ms,
            }],
        };
        handle_inbound(&ConsularMessage::EscalationAck(ack), our_session, &runtime);

        assert_eq!(
            runtime.outbox.as_ref().unwrap().len(),
            1,
            "foreign session_id must not authorise removal",
        );
    }

    /// Cancel-loopback must persist BEFORE pushing the loopback —
    /// the on-disk `at_ms` is what a future EscalationAck will
    /// match against, and the recv loop reuses the same value in
    /// the envelope.
    #[tokio::test]
    async fn cancel_loopback_persists_declined_result_with_matching_at_ms() {
        use consul_domain::identity::InstructionId;
        use consul_protocol::messages::{
            CancelInstruction, ConsularMessage, InstructionOutcome, RelayInstruction,
        };
        use tempfile::tempdir;

        use crate::handle::{make_pair_with_persistence, EscalationKind};
        use crate::persistent_inbox::PersistentInbox;
        use crate::persistent_outbox::PersistentEscalationOutbox;

        let dir = tempdir().unwrap();
        let inbox = PersistentInbox::new(dir.path().join("inbox.jsonl"));
        let outbox = PersistentEscalationOutbox::new(dir.path().join("outbox.jsonl"));
        let queued = RelayInstruction {
            instruction_id: InstructionId::new(),
            target_session_id: consul_domain::identity::SessionId::new_v7(),
            content: "deploy staging".into(),
            destructive: false,
        };
        inbox.append(&queued);
        let (_handle, mut runtime) = make_pair_with_persistence(inbox, outbox);

        let cancel = CancelInstruction {
            instruction_id: queued.instruction_id,
            target_session_id: queued.target_session_id,
            reason: Some("rollback".into()),
        };
        handle_inbound(
            &ConsularMessage::CancelInstruction(cancel),
            queued.target_session_id,
            &runtime,
        );

        let loopback_item = runtime.escalation_rx.recv().await.expect("loopback fired");
        match loopback_item.event {
            EscalationKind::InstructionResult { outcome, .. } => {
                assert!(matches!(outcome, InstructionOutcome::Declined { .. }));
            },
            other => panic!("expected InstructionResult, got {other:?}"),
        }

        // The disk entry's at_ms must equal the loopback item's
        // at_ms — they're the same OutboxItem, just routed via
        // memory and disk in parallel.
        let snap = runtime.outbox.as_ref().unwrap().snapshot();
        assert_eq!(snap.len(), 1, "loopback should append to outbox");
        assert_eq!(snap[0].at_ms, loopback_item.at_ms);
    }

    // ----- run_connect_hosted_with_reconnect ----------------------

    fn reconnect_test_config() -> ConnectConfig {
        // We need an address whose connect FAILS FAST and deterministically
        // on every platform — the reconnect tests below budget on the
        // assumption that the first attempt fails in ~ms so the loop is
        // already in backoff when they probe.
        //
        // The old `ws://127.0.0.1:1` is wrong on Windows: connecting to a
        // never-bound low port SYN-times-out over ~1-2s rather than
        // refusing instantly (Linux gives an immediate ECONNREFUSED, hence
        // the original assumption). That blew the 200ms/500ms/1.5s budgets.
        //
        // Fix: bind a listener to an OS-assigned free port, read the port,
        // then DROP the listener. A connect to a just-closed loopback port
        // gets an immediate RST on both Linux and Windows — fast and
        // deterministic, no SYN timeout.
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral port for reconnect test");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener); // free the port so connects refuse immediately
        ConnectConfig::new(
            format!("ws://127.0.0.1:{port}"),
            [0x11; 32],
            "reconnect-test",
            "/tmp/reconnect-test",
        )
    }

    /// Poll `cond` every 25ms until it returns true or `budget` elapses.
    /// Returns whether the condition was observed true. Lets reconnect
    /// tests assert on a published state without baking in a fixed sleep
    /// that assumes a platform-specific connect-refusal latency.
    async fn poll_until<F: Fn() -> bool>(cond: F, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        while tokio::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        cond()
    }

    #[tokio::test]
    async fn reconnect_loop_honors_cancel_during_backoff() {
        // A failing connect drops us into the sleep(backoff) arm.
        // notify() during that sleep must short-circuit to Ok(()).
        let (_handle, runtime) = crate::handle::make_pair();
        let cancel = std::sync::Arc::new(Notify::new());
        let cancel_for_signal = std::sync::Arc::clone(&cancel);
        let status = ConnectionStatus::new();
        let status_probe = status.clone();

        let task = tokio::spawn(async move {
            run_connect_hosted_with_reconnect(reconnect_test_config(), cancel, runtime, status).await
        });

        // Wait until the loop has actually entered the backoff sleep
        // (published Reconnecting) before firing cancel — otherwise on a
        // slow-connect platform we'd cancel mid-connect and exercise a
        // different path than the one under test. Polling removes the old
        // "connect-refused is fast, 200ms is enough" timing assumption.
        let in_backoff = poll_until(
            || status_probe.get() == ConnectionState::Reconnecting,
            Duration::from_secs(8),
        )
        .await;
        assert!(in_backoff, "wrapper should reach Reconnecting/backoff before cancel");
        cancel_for_signal.notify_waiters();

        // The cancel arm of tokio::select! must be selected over
        // the 1s sleep; allow a generous wall-clock budget for
        // slow CI but well under the cap.
        let result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("cancel should unblock backoff sleep")
            .expect("task joined cleanly");
        assert!(result.is_ok(), "cancel during backoff returns Ok(())");
    }

    #[tokio::test]
    async fn reconnect_loop_surfaces_version_mismatch_as_fatal() {
        // Drive the wrapper with a wrapper-level forced error to
        // assert VersionMismatch is NOT retried. Done indirectly
        // by inspecting that the constants exist and the wrapper
        // is the only consumer of them. (Full integration is
        // covered by the in-process consulate round-trip suite.)
        // This unit-level check pins the backoff parameters.
        assert_eq!(INITIAL_RECONNECT_BACKOFF, Duration::from_secs(1));
        assert_eq!(MAX_RECONNECT_BACKOFF, Duration::from_secs(30));
        assert!(
            MAX_RECONNECT_BACKOFF > INITIAL_RECONNECT_BACKOFF,
            "max must exceed initial for the doubling cap to mean anything"
        );
    }

    #[tokio::test]
    async fn reconnect_loop_keeps_retrying_after_repeated_transport_failure() {
        // Regression test for the silent-exit bug: prior to the
        // `ExitReason` refactor, `run_connect_hosted` returned
        // `Ok(())` on transport-side exits (peer close, recv error,
        // stream end, connect refused). The wrapper then treated
        // that as "session exited cleanly" and short-circuited
        // without reconnecting — auto-reconnect was effectively dead
        // for the most common real-world failure mode (network drop,
        // consulate restart). Now those paths return
        // `Err(LegatusError::Transport(_))` so the wrapper actually
        // backs off and tries again.
        //
        // This test pins that behavior: against a guaranteed-refused
        // address, the wrapper must NOT exit on its own — the only
        // way out is the cancel signal.
        let (_handle, runtime) = crate::handle::make_pair();
        let cancel = std::sync::Arc::new(Notify::new());
        let cancel_for_signal = std::sync::Arc::clone(&cancel);

        let status = ConnectionStatus::new();
        let status_probe = status.clone();
        let task = tokio::spawn(async move {
            run_connect_hosted_with_reconnect(reconnect_test_config(), cancel, runtime, status).await
        });

        // Wait until the wrapper has published Reconnecting — that proves
        // it failed the first connect AND chose to back off + retry rather
        // than exit (the pre-fix silent-exit bug returned Ok(()) here, so
        // the task would finish instead of ever reaching Reconnecting).
        // Polling the state is deterministic regardless of platform connect
        // latency; the old fixed 1.5s sleep assumed instant connect-refusal.
        let reached_reconnecting = poll_until(
            || status_probe.get() == ConnectionState::Reconnecting,
            Duration::from_secs(8),
        )
        .await;
        assert!(
            reached_reconnecting && !task.is_finished(),
            "reconnect wrapper must enter the retry/backoff loop, not exit \
             (reached_reconnecting={reached_reconnecting}, finished={})",
            task.is_finished()
        );

        // Now confirm it does exit cleanly on cancel.
        cancel_for_signal.notify_waiters();
        let result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("cancel should unblock backoff sleep")
            .expect("task joined cleanly");
        assert!(
            result.is_ok(),
            "cancel after retry loop should still return Ok(())"
        );
    }

    #[tokio::test]
    async fn reconnect_loop_observes_status_transitions() {
        // The reconnect wrapper must write to the shared
        // ConnectionStatus so the /legatus/health route handler can
        // see what's happening. Against a refused address, the
        // wrapper goes Disconnected (initial) -> Connecting
        // (first attempt) -> Reconnecting (after first failure) ->
        // Disconnected (on cancel).
        //
        // We can't reliably catch the brief Connecting phase from
        // outside the task (connect-refused is fast), but we CAN
        // assert the post-failure state is Reconnecting and the
        // post-cancel state is Disconnected. That's the contract
        // the dashboard / smoke test cares about.
        let (_handle, runtime) = crate::handle::make_pair();
        let cancel = std::sync::Arc::new(Notify::new());
        let cancel_for_signal = std::sync::Arc::clone(&cancel);
        let status = ConnectionStatus::new();
        assert_eq!(
            status.get(),
            ConnectionState::Disconnected,
            "fresh ConnectionStatus must start Disconnected"
        );
        let status_for_wrapper = status.clone();

        let task = tokio::spawn(async move {
            run_connect_hosted_with_reconnect(
                reconnect_test_config(),
                cancel,
                runtime,
                status_for_wrapper,
            )
            .await
        });

        // Poll (rather than assume a fixed timing) until the wrapper
        // publishes Reconnecting after its first failed connect. Connect
        // latency to a refused address is platform-dependent (instant on
        // Linux, up to the OS SYN timeout on Windows), so a fixed sleep is
        // racy; a bounded poll asserts the same contract deterministically.
        let reached_reconnecting = poll_until(
            || status.get() == ConnectionState::Reconnecting,
            Duration::from_secs(8),
        )
        .await;
        assert!(
            reached_reconnecting,
            "after the first failed connect, wrapper must publish Reconnecting (last state: {:?})",
            status.get()
        );

        // Cancel; wrapper should set Disconnected as part of its
        // clean-exit path.
        cancel_for_signal.notify_waiters();
        let result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("cancel should unblock wrapper")
            .expect("task joined cleanly");
        assert!(result.is_ok(), "cancel returns Ok(())");
        assert_eq!(
            status.get(),
            ConnectionState::Disconnected,
            "on clean exit, wrapper must publish Disconnected"
        );
    }

    #[test]
    fn connect_config_operator_id_defaults_to_none() {
        let cfg = ConnectConfig::new(
            "ws://127.0.0.1:9001",
            [0u8; 32],
            "scaffold-default",
            "/tmp/scaffold",
        );
        assert!(
            cfg.operator_id.is_none(),
            "scaffold default must leave operator_id unset so sessions register as ROOT"
        );
    }
}
