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
    /// Consulate WebSocket URL (e.g. `ws://127.0.0.1:9000`).
    pub consulate_url: String,
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
            bootstrap_secret,
            suggested_name: suggested_name.into(),
            working_dir: working_dir.into(),
            branch: None,
            task_description: None,
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
    // no-op (the channel ends are gone).
    let (_handle, runtime) = crate::handle::make_pair();
    run_connect_hosted(config, cancel, runtime).await
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
    mut runtime: LegatusRuntime,
) -> Result<(), LegatusError> {
    info!(url = %config.consulate_url, "legatus connecting");
    let (ws, _) = tokio_tungstenite::connect_async(&config.consulate_url)
        .await
        .map_err(|err| {
            LegatusError::Transport(format!("connect {}: {err}", config.consulate_url))
        })?;
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
                accepted_min: Some(format!("{:?}", vm)),
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

    let exit_reason: Option<&'static str> = loop {
        tokio::select! {
            () = cancel.notified() => {
                info!(%session_id, "legatus cancelled; sending SessionCompleted");
                break Some("cancelled");
            },
            escalation = runtime.escalation_rx.recv() => {
                let Some(kind) = escalation else {
                    // All escalation senders dropped — host went
                    // away. Stay alive; the loop continues serving
                    // the WS until cancel or the consulate closes.
                    continue;
                };
                let msg = match kind {
                    EscalationKind::Blocked { reason } => {
                        ConsularMessage::SessionBlocked(SessionBlocked {
                            session_id,
                            reason,
                            detected_at_ms: now_ms(),
                        })
                    },
                    EscalationKind::Completed { summary } => {
                        ConsularMessage::SessionCompleted(SessionCompleted {
                            session_id,
                            completed_at_ms: now_ms(),
                            summary,
                        })
                    },
                    EscalationKind::Failed { error } => {
                        ConsularMessage::SessionFailed(SessionFailed {
                            session_id,
                            failed_at_ms: now_ms(),
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
                    break None;
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
                    break None;
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
                        handle_inbound(&msg, session_id, &runtime);
                    },
                    Some(Ok(WsMessage::Close(frame))) => {
                        debug!(?frame, "consulate sent close");
                        break None;
                    },
                    Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => {
                        // tokio-tungstenite auto-handles pings.
                    },
                    Some(Ok(WsMessage::Text(text))) => {
                        debug!(text = %text, "unexpected text frame; ignored");
                    },
                    Some(Ok(WsMessage::Frame(_))) => {},
                    Some(Err(err)) => {
                        warn!(?err, "websocket recv error; closing");
                        break None;
                    },
                    None => {
                        debug!("websocket stream ended");
                        break None;
                    },
                }
            },
        }
    };

    if exit_reason.is_some() {
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
    }
    Ok(())
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
    sink.send(WsMessage::Binary(bytes.into()))
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
            _ => continue,
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
                if runtime.escalation_loopback.send(event).is_err() {
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
        other => {
            debug!(%session_id, ?other, "unhandled inbound message");
        }
    }
}

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

        let event = runtime.escalation_rx.recv().await.expect("loopback fired");
        match event {
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

        let event = runtime.escalation_rx.recv().await.unwrap();
        match event {
            EscalationKind::InstructionResult { outcome, .. } => match outcome {
                InstructionOutcome::Declined { reason } => {
                    assert_eq!(reason, "cancelled by operator");
                }
                other => panic!("expected Declined, got {other:?}"),
            },
            other => panic!("expected InstructionResult, got {other:?}"),
        }
    }
}
