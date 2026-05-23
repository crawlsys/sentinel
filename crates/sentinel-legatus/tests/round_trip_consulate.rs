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
    let (handle, runtime) = make_pair();
    let legatus_task = tokio::spawn(async move {
        run_connect_hosted(sentinel_config(url), cancel_for_task, runtime).await
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
    let (handle, runtime) = make_pair();
    let _legatus_task = tokio::spawn(async move {
        run_connect_hosted(sentinel_config(url), cancel_for_task, runtime).await
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
