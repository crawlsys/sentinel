//! [`LegatusHandle`] — runtime control surface for an in-flight
//! legatus connection, used when the WS connection is hosted by a
//! long-running process (the sentinel daemon) and other components
//! need to push escalations onto it / pop received instructions
//! from it.
//!
//! Two pieces of plumbing:
//!
//! - **escalation channel** (`mpsc::UnboundedSender<EscalationKind>`)
//!   — handle holds the sender; the connect-loop's runtime holds the
//!   receiver. Calling [`LegatusHandle::escalate`] queues an
//!   escalation that the loop converts to a signed
//!   `SessionBlocked` / `SessionCompleted` / `SessionFailed` /
//!   `InstructionAcknowledged` / `InstructionResult` envelope and
//!   writes to the WS.
//! - **persistent inbox** ([`PersistentInbox`]) — both halves share
//!   the same file-backed FIFO. The connect loop appends every
//!   received `RelayInstruction`; HTTP route consumers (sentinel's
//!   `consul_inbox` hook via the daemon's
//!   `GET /legatus/inbox/next`) pop via
//!   [`LegatusHandle::try_pop_inbox`]. Replacing the pre-persistence
//!   `mpsc` channel makes the queue survive daemon restart — the
//!   instruction is on disk *before* consul gets an
//!   `InstructionAck`, and the hook pops it whenever it's next
//!   ready.
//!
//! Escalations are now also durable via
//! [`crate::persistent_outbox::PersistentEscalationOutbox`]: when
//! the daemon builds a handle through
//! [`make_pair_with_persistence`], every `escalate()` call appends
//! to a file-backed FIFO before sending to the mpsc, and the WS
//! recv loop removes from disk only after `send_signed` succeeds.
//! Closes the consistency gap where a daemon crash between
//! `escalate()` and the WS send would lose the event — the
//! headline case being the `InstructionResult { Declined }`
//! emitted from the cancel loopback in
//! [`crate::client::handle_inbound`].

use chrono::Utc;
use consul_domain::identity::InstructionId;
use consul_protocol::messages::{BlockReason, InstructionOutcome, RelayInstruction};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::persistent_inbox::PersistentInbox;
use crate::persistent_outbox::{OutboxItem, PersistentEscalationOutbox};

/// Any event the legatus wants to send to its consul over the
/// post-handshake WS. Covers session-lifecycle escalations
/// (`Blocked` / `Completed` / `Failed`) **and** per-instruction
/// accounting (`InstructionAck` / `InstructionResult`) — the
/// connect loop drains one channel and matches on the variant to
/// produce the right Consular Protocol envelope.
///
/// Name kept as `EscalationKind` for backwards compatibility with
/// the in-tree callers shipped in commits A-C of the
/// sentinel-legatus series; "escalation" is a slight misnomer for
/// `InstructionAck` (which is routine) but not worth a workspace-
/// wide rename today.
///
/// `Serialize`/`Deserialize` are derived with `#[serde(tag =
/// "kind", rename_all = "snake_case")]` so the daemon's HTTP
/// routes can take the JSON body directly. Hook clients construct
/// one of:
///
/// ```json
/// {"kind": "completed",          "summary": "deployed staging"}
/// {"kind": "failed",             "error":   "tool x crashed"}
/// {"kind": "blocked",            "reason":  {"kind": "permission_denied", "tool": "Bash"}}
/// {"kind": "instruction_ack",    "instruction_id": "<uuid>"}
/// {"kind": "instruction_result", "instruction_id": "<uuid>",
///                                "outcome": {"kind": "success"},
///                                "summary": "applied migration"}
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EscalationKind {
    /// Session is blocked awaiting human input / a permission
    /// decision / tool failure judgment. Becomes a
    /// `SessionBlocked` envelope.
    Blocked {
        /// Why the session is blocked.
        reason: BlockReason,
    },
    /// Session's current task completed. Becomes a
    /// `SessionCompleted` envelope.
    Completed {
        /// Optional one-line summary for the operator brief.
        summary: Option<String>,
    },
    /// Session failed catastrophically. Becomes a
    /// `SessionFailed` envelope.
    Failed {
        /// One-line error description.
        error: String,
    },
    /// Per-instruction: acknowledge receipt of a previously-
    /// dispatched `RelayInstruction`. Sentinel's `consul_inbox`
    /// hook emits this when draining each instruction so the
    /// operator's chat surface gets an "is on it" line.
    InstructionAck {
        /// The instruction being acknowledged.
        instruction_id: InstructionId,
    },
    /// Per-instruction: report outcome of a previously-
    /// acknowledged `RelayInstruction`. Sentinel's `Stop` hook
    /// emits this for each pending instruction tracked in the
    /// per-session inbox file. MVP: outcome is always `Success`
    /// (we don't classify mid-run failures yet).
    InstructionResult {
        /// The instruction this result corresponds to.
        instruction_id: InstructionId,
        /// Outcome classification.
        outcome: InstructionOutcome,
        /// Optional one-line summary for the operator.
        summary: Option<String>,
    },
}

/// Runtime control surface for an in-flight legatus connection.
///
/// Cheap to clone — escalation channel is internally `Arc`'d and
/// both persistent file-handles are also `Clone` (just paths).
/// Hand a clone to every IPC route or hook that needs to push
/// escalations or pop the inbox.
#[derive(Clone)]
pub struct LegatusHandle {
    escalation_tx: mpsc::UnboundedSender<OutboxItem>,
    inbox: Option<PersistentInbox>,
    outbox: Option<PersistentEscalationOutbox>,
}

/// Receiver-side of the channels — owned by the connect loop and
/// drained inside its main `tokio::select!`.
///
/// `escalation_loopback` is a clone of the same sender the handle
/// holds, kept on the runtime side so the WS recv loop's inbound
/// handler can synthesize follow-up events (e.g. emit an
/// `InstructionResult { Declined }` after dropping a queued
/// instruction in response to a `CancelInstruction`). The
/// loopback feeds the same `escalation_rx` the loop already
/// drains, so there's exactly one path from "event" to "WS
/// send" — no duplicated sink handling, no second select arm.
///
/// `outbox` is a clone of the same file-backed escalation queue
/// the handle uses for persistence. The WS recv loop's escalation
/// arm calls `outbox.remove_head()` after each successful
/// `send_signed` so the disk stays in sync with what consul has
/// actually received.
pub struct LegatusRuntime {
    pub(crate) escalation_rx: mpsc::UnboundedReceiver<OutboxItem>,
    pub(crate) escalation_loopback: mpsc::UnboundedSender<OutboxItem>,
    pub(crate) inbox: Option<PersistentInbox>,
    pub(crate) outbox: Option<PersistentEscalationOutbox>,
}

/// Construct a paired `(handle, runtime)` with no persistent inbox
/// or outbox.
///
/// Used by the standalone `sentinel legatus connect` path, where
/// received instructions are log-only and outbound escalations
/// are in-memory only (no daemon to drain them / persist them).
#[must_use]
pub fn make_pair() -> (LegatusHandle, LegatusRuntime) {
    let (escalation_tx, escalation_rx) = mpsc::unbounded_channel();
    let handle = LegatusHandle {
        escalation_tx: escalation_tx.clone(),
        inbox: None,
        outbox: None,
    };
    let runtime = LegatusRuntime {
        escalation_rx,
        escalation_loopback: escalation_tx,
        inbox: None,
        outbox: None,
    };
    (handle, runtime)
}

/// Construct a paired `(handle, runtime)` backed by the given
/// [`PersistentInbox`] (no outbox). Used by tests that exercise
/// inbox persistence without wanting outbox file I/O.
#[must_use]
pub fn make_pair_with_inbox(inbox: PersistentInbox) -> (LegatusHandle, LegatusRuntime) {
    let (escalation_tx, escalation_rx) = mpsc::unbounded_channel();
    let handle = LegatusHandle {
        escalation_tx: escalation_tx.clone(),
        inbox: Some(inbox.clone()),
        outbox: None,
    };
    let runtime = LegatusRuntime {
        escalation_rx,
        escalation_loopback: escalation_tx,
        inbox: Some(inbox),
        outbox: None,
    };
    (handle, runtime)
}

/// Construct a paired `(handle, runtime)` backed by both a
/// [`PersistentInbox`] AND a [`PersistentEscalationOutbox`].
///
/// Used by the sentinel daemon path where both directions need
/// crash-recovery: received instructions survive daemon restart
/// (inbox) AND queued escalation events survive daemon restart
/// before they reach the WS (outbox).
#[must_use]
pub fn make_pair_with_persistence(
    inbox: PersistentInbox,
    outbox: PersistentEscalationOutbox,
) -> (LegatusHandle, LegatusRuntime) {
    let (escalation_tx, escalation_rx) = mpsc::unbounded_channel();
    let handle = LegatusHandle {
        escalation_tx: escalation_tx.clone(),
        inbox: Some(inbox.clone()),
        outbox: Some(outbox.clone()),
    };
    let runtime = LegatusRuntime {
        escalation_rx,
        escalation_loopback: escalation_tx,
        inbox: Some(inbox),
        outbox: Some(outbox),
    };
    (handle, runtime)
}

impl LegatusHandle {
    /// Queue an escalation. Returns `Err` only if the legatus
    /// runtime has been dropped (connection closed / process
    /// shutting down).
    ///
    /// Persistence: when this handle was built with a
    /// [`PersistentEscalationOutbox`] (via
    /// [`make_pair_with_persistence`]), the event is appended to
    /// disk BEFORE being sent to the mpsc. A daemon crash between
    /// this call and the WS recv loop's `send_signed` is therefore
    /// recoverable on next daemon start — the loop's startup
    /// replay drains the outbox into the mpsc as if the events
    /// had just been escalated. Best-effort: disk-write failures
    /// are logged at `warn` and the mpsc send still proceeds.
    ///
    /// # Errors
    ///
    /// Returns [`EscalationSendError`] when the legatus runtime
    /// receiver has been dropped.
    pub fn escalate(&self, event: EscalationKind) -> Result<(), EscalationSendError> {
        let at_ms = now_ms();
        let item = OutboxItem::new(event, at_ms);
        if let Some(outbox) = self.outbox.as_ref() {
            outbox.append(&item);
        }
        self.escalation_tx
            .send(item)
            .map_err(|_| EscalationSendError::RuntimeGone)
    }

    /// Non-blocking pop of the next received `RelayInstruction`.
    ///
    /// Returns `None` if the inbox is empty, the queue file is
    /// missing, or this handle was built without a persistent
    /// inbox (the standalone path). File I/O is wrapped in
    /// `tokio::task::spawn_blocking` so the executor never stalls
    /// on the advisory lock.
    pub async fn try_pop_inbox(&self) -> Option<RelayInstruction> {
        let inbox = self.inbox.clone()?;
        tokio::task::spawn_blocking(move || inbox.try_pop())
            .await
            .ok()
            .flatten()
    }

    /// Diagnostic: borrow the underlying `PersistentInbox`, if any.
    /// Exposed for tests + `sentinel legatus daemon-status`.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // Option::as_ref is not const.
    pub fn persistent_inbox(&self) -> Option<&PersistentInbox> {
        self.inbox.as_ref()
    }

    /// Diagnostic: borrow the underlying `PersistentEscalationOutbox`,
    /// if any. Exposed for the daemon's `/legatus/pending` HTTP route
    /// so operators can see how many escalations are queued on disk
    /// for delivery.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // Option::as_ref is not const.
    pub fn persistent_outbox(&self) -> Option<&PersistentEscalationOutbox> {
        self.outbox.as_ref()
    }
}

/// Unix-epoch millis. Used at-append time by
/// [`LegatusHandle::escalate`] and at-loopback time by the cancel
/// handler in `client.rs` so the on-disk `at_ms` matches what the
/// envelope carries on the wire.
fn now_ms() -> u64 {
    u64::try_from(Utc::now().timestamp_millis().max(0)).unwrap_or(0)
}

/// Returned by [`LegatusHandle::escalate`] when the runtime has
/// shut down.
#[derive(Debug, thiserror::Error)]
pub enum EscalationSendError {
    /// The legatus runtime exited (connection closed) so the
    /// receiver is gone.
    #[error("legatus runtime has shut down; escalation dropped")]
    RuntimeGone,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use consul_domain::identity::{InstructionId, SessionId};
    use consul_protocol::messages::RelayInstruction;
    use tempfile::tempdir;

    use super::*;

    fn fake_instruction(content: &str) -> RelayInstruction {
        RelayInstruction {
            instruction_id: InstructionId::new(),
            target_session_id: SessionId::new_v7(),
            content: content.into(),
            destructive: false,
        }
    }

    #[tokio::test]
    async fn escalate_succeeds_when_runtime_is_alive() {
        let (handle, mut runtime) = make_pair();
        handle
            .escalate(EscalationKind::Completed {
                summary: Some("ok".into()),
            })
            .unwrap();
        let received = runtime.escalation_rx.recv().await.unwrap();
        match received.event {
            EscalationKind::Completed { summary } => assert_eq!(summary.as_deref(), Some("ok")),
            other => panic!("expected Completed, got {other:?}"),
        }
        // escalate() stamps at_ms from now_ms(); the value isn't
        // 0 unless the system clock is genuinely at 1970.
        assert!(received.at_ms > 0, "at_ms should be stamped, got 0");
    }

    #[tokio::test]
    async fn escalate_errors_after_runtime_drop() {
        let (handle, runtime) = make_pair();
        drop(runtime);
        let err = handle
            .escalate(EscalationKind::Failed {
                error: "x".into(),
            })
            .unwrap_err();
        assert!(matches!(err, EscalationSendError::RuntimeGone));
    }

    #[tokio::test]
    async fn try_pop_inbox_returns_none_for_standalone_pair() {
        // make_pair() builds an inbox-less pair; pop is always None.
        let (handle, _runtime) = make_pair();
        assert!(handle.try_pop_inbox().await.is_none());
    }

    #[tokio::test]
    async fn try_pop_inbox_drains_persistent_inbox() {
        let dir = tempdir().unwrap();
        let inbox = PersistentInbox::new(dir.path().join("legatus-inbox.jsonl"));
        // Pre-seed as if the runtime had received an instruction.
        let instr = fake_instruction("hello");
        inbox.append(&instr);

        let (handle, _runtime) = make_pair_with_inbox(inbox);
        let popped = handle.try_pop_inbox().await.unwrap();
        assert_eq!(popped.content, "hello");
        // Second pop returns None — single-item queue.
        assert!(handle.try_pop_inbox().await.is_none());
    }

    #[tokio::test]
    async fn handle_and_runtime_share_persistent_inbox() {
        // Appends made via runtime.inbox are visible to handle.
        let dir = tempdir().unwrap();
        let inbox = PersistentInbox::new(dir.path().join("legatus-inbox.jsonl"));
        let (handle, runtime) = make_pair_with_inbox(inbox);
        let runtime_inbox = runtime.inbox.as_ref().unwrap();

        runtime_inbox.append(&fake_instruction("a"));
        runtime_inbox.append(&fake_instruction("b"));

        let first = handle.try_pop_inbox().await.unwrap();
        let second = handle.try_pop_inbox().await.unwrap();
        assert_eq!(first.content, "a");
        assert_eq!(second.content, "b");
    }

    #[tokio::test]
    async fn persistent_outbox_accessor_returns_some_when_seeded() {
        // The /legatus/pending HTTP route reads outbox state through
        // this accessor — make sure it isn't accidentally None on a
        // pair built with persistence.
        let dir = tempdir().unwrap();
        let inbox = PersistentInbox::new(dir.path().join("legatus-inbox.jsonl"));
        let outbox =
            PersistentEscalationOutbox::new(dir.path().join("legatus-escalations.jsonl"));
        let (handle, mut runtime) = make_pair_with_persistence(inbox, outbox);

        let outbox_ref = handle.persistent_outbox().expect("outbox is seeded");
        assert_eq!(outbox_ref.len(), 0);

        handle
            .escalate(EscalationKind::Failed {
                error: "boom".into(),
            })
            .unwrap();
        // escalate() appends to the outbox synchronously before the
        // mpsc send, so the count is visible immediately.
        assert_eq!(handle.persistent_outbox().unwrap().len(), 1);
        // Drain the item so the bounded mpsc doesn't keep
        // `runtime` and its outbox-borrowing futures alive past the
        // test (the underlying file lives in `dir`, dropped at end).
        let _ = runtime.escalation_rx.recv().await;
    }

    #[tokio::test]
    async fn persistent_outbox_accessor_returns_none_for_standalone_pair() {
        // make_pair() and make_pair_with_inbox() build outbox-less
        // pairs; the accessor must report that honestly so the
        // pending route returns 0 rather than panicking.
        let (handle, _runtime) = make_pair();
        assert!(handle.persistent_outbox().is_none());

        let dir = tempdir().unwrap();
        let inbox = PersistentInbox::new(dir.path().join("legatus-inbox.jsonl"));
        let (handle, _runtime) = make_pair_with_inbox(inbox);
        assert!(handle.persistent_outbox().is_none());
    }
}
