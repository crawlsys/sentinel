//! End-to-end round-trip integration test: real in-process
//! Consulate <-> real sentinel-legatus over a WebSocket transport.
//!
//! Spawns a Consulate listener on an ephemeral TCP port, runs the
//! full registration handshake + session loop wired to an
//! escalation channel, then drives a sentinel-legatus runtime
//! through `run_connect_hosted` and asserts the bidirectional
//! flow specified by the Fabrica runbook
//! (`docs/runbooks/consul-sentinel-roundtrip.md` in the
//! legatus-consul-agent repo):
//!
//!   1. Consulate accepts the legatus connection (handshake +
//!      RegisterSession exchange).
//!   2. Consul dispatches a RelayInstruction down to the legatus.
//!   3. Legatus emits an InstructionAck back via
//!      `LegatusHandle::escalate` (simulating the role of the
//!      sentinel `consul_inbox` hook when it pops the inbox).
//!   4. Legatus emits an InstructionResult { Success } back
//!      (simulating the role of the sentinel `Stop` hook).
//!   5. Legatus emits a SessionCompleted on graceful shutdown.
//!
//! Each event is asserted on the Consulate's escalation bus, with
//! payload contents (instruction_id, outcome, summary) round-trip
//! verified end-to-end. This is the revival of task #13.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_const_for_fn,
    clippy::too_many_lines,
    clippy::match_wild_err_arm
)]

use std::sync::Arc;
use std::time::Duration;

use consul_domain::adapters::audit::InMemoryAuditSink;
use consul_domain::adapters::clock::TokioClock;
use consul_domain::identity::{ConnectionEpoch, KeyEpoch, SessionId};
use consul_domain::session::registry::SessionRegistry;
use consul_protocol::keys::{derive_mac_key_pair, BOOTSTRAP_SECRET_LEN};
use consul_protocol::messages::{InstructionId, InstructionOutcome, RelayInstruction, RuntimeKind};
use consul_storage::sqlite::{connect_in_memory_for_tests, SqliteSessionStore};
use consul_transport::websocket;
use consulate::connection_registry::ConnectionRegistry;
use consulate::escalation_bus::{EscalationEnvelope, EscalationEvent};
use pretty_assertions::assert_eq;
use sentinel_legatus::client::{run_connect_hosted, ConnectConfig};
use sentinel_legatus::handle::{make_pair, EscalationKind};
use sentinel_legatus::LegatusError;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, Notify};

type Registry = SessionRegistry<SqliteSessionStore, TokioClock, InMemoryAuditSink>;

const TEST_BOOTSTRAP_SECRET: [u8; BOOTSTRAP_SECRET_LEN] = [0x77; BOOTSTRAP_SECRET_LEN];

async fn build_registry() -> Registry {
    let pool = connect_in_memory_for_tests().await.unwrap();
    SessionRegistry::new(
        SqliteSessionStore::new(pool),
        TokioClock,
        InMemoryAuditSink::new(),
    )
}

struct SpawnedConsulate {
    url: String,
    session_id_rx: oneshot::Receiver<SessionId>,
    escalation_rx: mpsc::UnboundedReceiver<EscalationEnvelope>,
    connections: ConnectionRegistry,
}

async fn spawn_consulate() -> SpawnedConsulate {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());

    let registry = Arc::new(build_registry().await);
    let connections = ConnectionRegistry::new();
    let connections_task = connections.clone();

    let (session_id_tx, session_id_rx) = oneshot::channel();
    let (escalation_tx, escalation_rx) = mpsc::unbounded_channel::<EscalationEnvelope>();

    tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.unwrap();
        let transport = websocket::accept(stream).await.unwrap();
        let registered = consulate::handshake::run_registration_handshake(
            &transport,
            &TEST_BOOTSTRAP_SECRET,
            registry.as_ref(),
        )
        .await
        .unwrap();
        // Surface the consulate-assigned session id to the test
        // before the session loop starts blocking on I/O.
        let _ = session_id_tx.send(registered.session_id);

        let pair = derive_mac_key_pair(
            &registered.master_key,
            registered.session_id,
            ConnectionEpoch::INITIAL,
            KeyEpoch::INITIAL,
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        connections_task.register(registered.session_id, tx);
        let _ = consulate::session_loop::run_session_loop(
            &transport,
            registered.session_id,
            &pair.outbound,
            &pair.inbound,
            registry.as_ref(),
            &mut rx,
            Some(&escalation_tx),
        )
        .await;
        connections_task.unregister(registered.session_id);
    });

    SpawnedConsulate {
        url,
        session_id_rx,
        escalation_rx,
        connections,
    }
}

fn sentinel_config(url: String) -> ConnectConfig {
    ConnectConfig {
        consulate_url: url,
        bootstrap_secret: TEST_BOOTSTRAP_SECRET,
        suggested_name: "sentinel-roundtrip".into(),
        runtime: RuntimeKind::ClaudeCode,
        working_dir: "/tmp/sentinel-roundtrip".into(),
        branch: Some("test".into()),
        task_description: Some("round-trip integration test".into()),
        heartbeat_interval: Duration::from_millis(75),
        operator_id: None,
    }
}

/// Pull from `escalation_rx` until `predicate` matches; panic on
/// timeout. Returns the matching envelope. Non-matching envelopes
/// are silently consumed (the test doesn't care about ordering
/// between unrelated events).
async fn wait_for<F>(
    escalation_rx: &mut mpsc::UnboundedReceiver<EscalationEnvelope>,
    predicate: F,
    label: &str,
) -> EscalationEnvelope
where
    F: Fn(&EscalationEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let now = tokio::time::Instant::now();
        assert!(now < deadline, "timeout waiting for {label}");
        let remaining = deadline - now;
        match tokio::time::timeout(remaining, escalation_rx.recv()).await {
            Ok(Some(env)) => {
                if predicate(&env.event) {
                    return env;
                }
            }
            Ok(None) => panic!("escalation channel closed while waiting for {label}"),
            Err(_) => panic!("timeout waiting for {label}"),
        }
    }
}

#[tokio::test]
async fn consulate_dispatches_then_observes_ack_result_completed() {
    let SpawnedConsulate {
        url,
        session_id_rx,
        mut escalation_rx,
        connections,
    } = spawn_consulate().await;

    // Spawn sentinel-legatus runtime in the background.
    let cancel = Arc::new(Notify::new());
    let cancel_for_task = cancel.clone();
    let (handle, mut runtime) = make_pair();
    let legatus_task = tokio::spawn(async move {
        run_connect_hosted(sentinel_config(url), cancel_for_task, &mut runtime).await
    });

    // Phase 1: consulate observes handshake + registration.
    let session_id = tokio::time::timeout(Duration::from_secs(5), session_id_rx)
        .await
        .expect("handshake should complete within 5s")
        .expect("session_id_tx should fire on successful registration");

    // Wait for the connection-registry to see the session.
    for _ in 0..200 {
        if !connections.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(connections.len(), 1, "exactly one legatus connection");

    // Phase 2: consulate dispatches a RelayInstruction down to the
    // legatus.
    let instruction_id = InstructionId::new();
    let instr = RelayInstruction {
        instruction_id,
        target_session_id: session_id,
        content: "deploy staging build 2026.05.23-rc1".into(),
        destructive: false,
    };
    connections
        .dispatch(session_id, instr.clone())
        .expect("dispatch should succeed");

    // Phase 3: simulate the sentinel consul_inbox hook acknowledging
    // the instruction. The legatus runtime translates this into an
    // InstructionAcknowledged envelope and forwards it over the WS.
    // Give the legatus loop a moment to receive + persist the relay
    // before we ack -- mirrors the real flow where the hook polls.
    tokio::time::sleep(Duration::from_millis(75)).await;
    handle
        .escalate(EscalationKind::InstructionAck { instruction_id })
        .expect("escalate ack should succeed");

    // Consulate should observe the InstructionAck envelope.
    let ack_env = wait_for(
        &mut escalation_rx,
        |e| matches!(e, EscalationEvent::InstructionAck(_)),
        "InstructionAck from sentinel-legatus",
    )
    .await;
    assert_eq!(ack_env.session_id, session_id);
    if let EscalationEvent::InstructionAck(ack) = ack_env.event {
        assert_eq!(ack.instruction_id, instruction_id);
        assert_eq!(ack.session_id, session_id);
    } else {
        panic!("expected InstructionAck variant");
    }

    // Phase 4: simulate the sentinel Stop hook reporting the
    // instruction's outcome.
    let result_summary = "applied terraform plan; 12 resources changed".to_string();
    handle
        .escalate(EscalationKind::InstructionResult {
            instruction_id,
            outcome: InstructionOutcome::Success,
            summary: Some(result_summary.clone()),
        })
        .expect("escalate result should succeed");

    let result_env = wait_for(
        &mut escalation_rx,
        |e| matches!(e, EscalationEvent::InstructionResult(_)),
        "InstructionResult from sentinel-legatus",
    )
    .await;
    assert_eq!(result_env.session_id, session_id);
    if let EscalationEvent::InstructionResult(result) = result_env.event {
        assert_eq!(result.instruction_id, instruction_id);
        assert_eq!(result.session_id, session_id);
        assert_eq!(result.outcome, InstructionOutcome::Success);
        assert_eq!(result.summary.as_deref(), Some(result_summary.as_str()));
    } else {
        panic!("expected InstructionResult variant");
    }

    // Phase 5: graceful shutdown sends SessionCompleted.
    handle
        .escalate(EscalationKind::Completed {
            summary: Some("round-trip test wrapping up".into()),
        })
        .expect("escalate completed should succeed");

    let completed_env = wait_for(
        &mut escalation_rx,
        |e| matches!(e, EscalationEvent::Completed(_)),
        "SessionCompleted from sentinel-legatus",
    )
    .await;
    assert_eq!(completed_env.session_id, session_id);
    if let EscalationEvent::Completed(c) = completed_env.event {
        assert_eq!(c.session_id, session_id);
        assert_eq!(
            c.summary.as_deref(),
            Some("round-trip test wrapping up"),
            "summary should round-trip"
        );
    } else {
        panic!("expected Completed variant");
    }

    cancel.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(2), legatus_task).await;
}

#[tokio::test]
async fn instruction_outcome_failure_round_trips_with_error_body() {
    let SpawnedConsulate {
        url,
        session_id_rx,
        mut escalation_rx,
        connections,
    } = spawn_consulate().await;

    let cancel = Arc::new(Notify::new());
    let cancel_for_task = cancel.clone();
    let (handle, mut runtime) = make_pair();
    let _legatus_task = tokio::spawn(async move {
        run_connect_hosted(sentinel_config(url), cancel_for_task, &mut runtime).await
    });

    let session_id = tokio::time::timeout(Duration::from_secs(5), session_id_rx)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..200 {
        if !connections.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(connections.len(), 1);

    let instruction_id = InstructionId::new();
    connections
        .dispatch(
            session_id,
            RelayInstruction {
                instruction_id,
                target_session_id: session_id,
                content: "this will fail".into(),
                destructive: false,
            },
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(75)).await;

    handle
        .escalate(EscalationKind::InstructionResult {
            instruction_id,
            outcome: InstructionOutcome::Failure {
                error: "tool execution crashed: exit code 137 (OOM)".into(),
            },
            summary: Some("see crash log".into()),
        })
        .unwrap();

    let env = wait_for(
        &mut escalation_rx,
        |e| matches!(e, EscalationEvent::InstructionResult(_)),
        "InstructionResult { Failure }",
    )
    .await;
    if let EscalationEvent::InstructionResult(r) = env.event {
        match r.outcome {
            InstructionOutcome::Failure { error } => {
                assert!(error.contains("OOM"), "error body should round-trip");
            }
            other => panic!("expected Failure outcome, got {other:?}"),
        }
    } else {
        panic!("expected InstructionResult");
    }

    cancel.notify_one();
}

// ----------------------------------------------------------------------
// T1: live cross-repo end-to-end test for the voice-attested
// catastrophic flow.
//
// Exercises every wire-level piece sentinel + consulate must
// agree on:
//
//   1. Sentinel-legatus connects to a real consulate.
//   2. Sentinel emits SessionBlocked{CatastrophicPending} via
//      LegatusHandle::escalate.
//   3. The consulate forwards the SessionBlocked onto its
//      escalation bus; the test code (simulating consul-app's
//      CatastrophicAckProducer) observes it.
//   4. Test constructs a CatastrophicAck with a fixture
//      VoiceprintWitness and dispatches via
//      ConnectionRegistry::dispatch_catastrophic_ack.
//   5. The consulate session_loop's new outbound arm sends the
//      CatastrophicAck back to sentinel-legatus over the WS.
//   6. Sentinel-legatus's handle_inbound CatastrophicAck arm
//      records the approval in the daemon-held cache.
//   7. Test asserts the cache contains the matching approval.
//
// The CatastrophicAckProducer's gate-invocation logic is unit-
// tested in consul-app/src/catastrophic_producer.rs; this test
// covers the wire path between the producer's dispatch and the
// hook-visible cache state.
// ----------------------------------------------------------------------

#[tokio::test]
async fn t1_catastrophic_ack_round_trips_into_approval_cache() {
    use chrono::Utc;
    use consul_domain::identity::republic::{ChallengeNonce, OperatorId, VoiceprintWitness};
    use consul_protocol::messages::{
        AckDecision, BlockReason, CatastrophicAck, EscalationKey, SessionBlocked,
    };
    use sentinel_legatus::{CatastrophicApprovalCache, SpentNonceLog};

    let SpawnedConsulate {
        url,
        session_id_rx,
        mut escalation_rx,
        connections,
    } = spawn_consulate().await;

    // Wire the runtime with an approval cache + spent-nonce log
    // exactly as the production daemon does. The hook would
    // consume from this cache; the test asserts on it directly.
    let cancel = Arc::new(Notify::new());
    let cancel_for_task = cancel.clone();
    let (handle, runtime) = make_pair();
    let approval_cache = Arc::new(CatastrophicApprovalCache::new());
    let spent_nonces = Arc::new(SpentNonceLog::new());
    let mut runtime = runtime
        .with_approval_cache(approval_cache.clone())
        .with_spent_nonce_log(spent_nonces);
    let legatus_task = tokio::spawn(async move {
        run_connect_hosted(sentinel_config(url), cancel_for_task, &mut runtime).await
    });

    let session_id = tokio::time::timeout(Duration::from_secs(5), session_id_rx)
        .await
        .expect("handshake within 5s")
        .expect("session_id from handshake");

    for _ in 0..200 {
        if !connections.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(connections.len(), 1, "exactly one legatus connection");

    // Phase 1: sentinel-legatus emits the SessionBlocked. The
    // catastrophic_escalation hook does this in production via
    // escalate_fire_and_forget; here we drive it directly.
    let action_class = "Bash";
    handle
        .escalate(EscalationKind::Blocked {
            reason: BlockReason::CatastrophicPending {
                action_class: action_class.into(),
                action_summary: "rm -rf /var/log/old".into(),
            },
        })
        .expect("escalate CatastrophicPending");

    // Phase 2: consulate forwards onto the escalation bus.
    let blocked_env = wait_for(
        &mut escalation_rx,
        |e| {
            matches!(
                e,
                EscalationEvent::Blocked(b) if matches!(
                    b.reason,
                    BlockReason::CatastrophicPending { .. }
                )
            )
        },
        "SessionBlocked{CatastrophicPending} on consulate escalation bus",
    )
    .await;
    let blocked: SessionBlocked = match blocked_env.event {
        EscalationEvent::Blocked(b) => b,
        _ => unreachable!(),
    };

    // Phase 3: simulate consul-app's CatastrophicAckProducer.
    // Real producer would run the voice-attested gate; the test
    // synthesizes a witness + dispatches directly so we can
    // assert the WIRE path independently of the gate's behaviour
    // (that's covered in consul-app/src/catastrophic_producer.rs
    // unit tests).
    let operator = OperatorId::new();
    let nonce = ChallengeNonce::from_bytes([0xAB; 16]);
    let witness = VoiceprintWitness {
        operator,
        utterance_audio_hash: [0x11; 32],
        utterance_transcript: format!("approve {action_class}, code {}", nonce.to_hex()),
        challenge_nonce: nonce,
        signature: [0x22; 64],
        signed_at: Utc::now(),
    };
    let ack = CatastrophicAck {
        key: EscalationKey::SessionBlocked {
            session_id: blocked.session_id,
            detected_at_ms: blocked.detected_at_ms,
        },
        decision: AckDecision::Approve,
        signed_at: Utc::now(),
        voiceprint_witness: witness,
    };
    connections
        .dispatch_catastrophic_ack(blocked.session_id, ack)
        .expect("dispatch CatastrophicAck through consulate");

    // Phase 4: poll the daemon-held approval cache until the
    // sentinel-legatus inbound handler has parsed the witness +
    // recorded the approval. handle_inbound runs on the WS recv
    // task; the cache write happens out-of-band, so we poll
    // briefly.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(consumed) = approval_cache.consume(session_id, action_class) {
            assert!(
                consumed.transcript.contains("approve Bash"),
                "transcript should round-trip the operator's spoken phrase, got: {}",
                consumed.transcript
            );
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "approval did not land in cache within 3s (cache len = {})",
                approval_cache.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Single-use semantics: a second consume should miss.
    assert!(
        approval_cache.consume(session_id, action_class).is_none(),
        "approval should be consumed (single-use)"
    );

    cancel.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(2), legatus_task).await;
}

/// Companion test: replay protection. Same CatastrophicAck
/// dispatched twice -- the second one must NOT land in the cache
/// (the spent-nonce log rejects it).
#[tokio::test]
async fn t1_catastrophic_ack_replay_is_rejected() {
    use chrono::Utc;
    use consul_domain::identity::republic::{ChallengeNonce, OperatorId, VoiceprintWitness};
    use consul_protocol::messages::{
        AckDecision, BlockReason, CatastrophicAck, EscalationKey,
    };
    use sentinel_legatus::{CatastrophicApprovalCache, SpentNonceLog};

    let SpawnedConsulate {
        url,
        session_id_rx,
        mut escalation_rx,
        connections,
    } = spawn_consulate().await;

    let cancel = Arc::new(Notify::new());
    let cancel_for_task = cancel.clone();
    let (handle, runtime) = make_pair();
    let approval_cache = Arc::new(CatastrophicApprovalCache::new());
    let spent_nonces = Arc::new(SpentNonceLog::new());
    let mut runtime = runtime
        .with_approval_cache(approval_cache.clone())
        .with_spent_nonce_log(spent_nonces.clone());
    let legatus_task = tokio::spawn(async move {
        run_connect_hosted(sentinel_config(url), cancel_for_task, &mut runtime).await
    });

    let session_id = tokio::time::timeout(Duration::from_secs(5), session_id_rx)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..200 {
        if !connections.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let action_class = "Bash";
    handle
        .escalate(EscalationKind::Blocked {
            reason: BlockReason::CatastrophicPending {
                action_class: action_class.into(),
                action_summary: "drop database prod".into(),
            },
        })
        .unwrap();

    let blocked_env = wait_for(
        &mut escalation_rx,
        |e| matches!(e, EscalationEvent::Blocked(_)),
        "Blocked",
    )
    .await;
    let blocked = match blocked_env.event {
        EscalationEvent::Blocked(b) => b,
        _ => unreachable!(),
    };

    let nonce = ChallengeNonce::from_bytes([0xCD; 16]);
    let make_ack = || CatastrophicAck {
        key: EscalationKey::SessionBlocked {
            session_id: blocked.session_id,
            detected_at_ms: blocked.detected_at_ms,
        },
        decision: AckDecision::Approve,
        signed_at: Utc::now(),
        voiceprint_witness: VoiceprintWitness {
            operator: OperatorId::new(),
            utterance_audio_hash: [0x11; 32],
            utterance_transcript: format!("approve {action_class}, code {}", nonce.to_hex()),
            challenge_nonce: nonce,
            signature: [0x22; 64],
            signed_at: Utc::now(),
        },
    };

    // First ack lands.
    connections
        .dispatch_catastrophic_ack(blocked.session_id, make_ack())
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if approval_cache.consume(session_id, action_class).is_some() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("first approval did not land in cache");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Second dispatch with the SAME nonce -- replay-rejected by
    // the spent-nonce log; should NOT land in the cache.
    connections
        .dispatch_catastrophic_ack(blocked.session_id, make_ack())
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        approval_cache.consume(session_id, action_class).is_none(),
        "replayed CatastrophicAck must not land in approval cache"
    );
    assert_eq!(spent_nonces.len(), 1, "exactly one nonce spent");

    cancel.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(2), legatus_task).await;
}

/// Regression test for the `ExitReason::TransportFailed` fix.
///
/// Before the fix, `run_connect_hosted` returned `Ok(())` when the
/// consulate-side WebSocket closed mid-session (peer close, recv
/// error, stream end). The reconnect wrapper interpreted that as a
/// clean exit and short-circuited — auto-reconnect was effectively
/// dead for the most common production failure mode (network drop,
/// consulate restart).
///
/// This test pins the new behaviour: after a clean handshake +
/// registration, the consulate-side task immediately drops the
/// transport. The sentinel-side `run_connect_hosted` must surface
/// that as `Err(LegatusError::Transport(_))`.
#[tokio::test]
async fn run_connect_hosted_surfaces_transport_failure_on_remote_close() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let registry = Arc::new(build_registry().await);

    // Drop-the-WS consulate: accept, handshake, register — then
    // let the transport drop on scope exit so the legatus loop
    // sees `source.next() == None` (stream ended) on the very next
    // poll.
    tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.unwrap();
        let transport = websocket::accept(stream).await.unwrap();
        let _registered = consulate::handshake::run_registration_handshake(
            &transport,
            &TEST_BOOTSTRAP_SECRET,
            registry.as_ref(),
        )
        .await
        .unwrap();
        // Intentional: do NOT enter session_loop. Drop `transport`
        // here so the legatus WS sees the peer close.
        drop(transport);
    });

    let cancel = Arc::new(Notify::new());
    let cancel_for_task = cancel.clone();
    let (_handle, mut runtime) = make_pair();
    let legatus_task = tokio::spawn(async move {
        run_connect_hosted(sentinel_config(url), cancel_for_task, &mut runtime).await
    });

    // The legatus loop should observe the peer close within a few
    // hundred ms (one heartbeat tick at most, given the 75ms
    // interval in `sentinel_config`). Allow generous slack for CI.
    let result = tokio::time::timeout(Duration::from_secs(5), legatus_task)
        .await
        .expect("legatus should exit within 5s of remote close")
        .expect("task joined cleanly");

    match result {
        Err(LegatusError::Transport(reason)) => {
            // The reason should mention which path detected the
            // failure (WS recv, stream end, etc.) — useful for
            // operators tailing logs. Don't pin the exact string,
            // just that one of the expected transport paths fired.
            assert!(
                reason.contains("WS")
                    || reason.contains("transport")
                    || reason.contains("close")
                    || reason.contains("peer dropped"),
                "transport-failure reason should describe the WS path, got: {reason}"
            );
        }
        Ok(()) => panic!(
            "run_connect_hosted returned Ok(()) on remote close — the pre-fix bug. \
             Wrapper would not reconnect."
        ),
        Err(other) => panic!(
            "expected LegatusError::Transport on remote close, got: {other:?}"
        ),
    }
}
