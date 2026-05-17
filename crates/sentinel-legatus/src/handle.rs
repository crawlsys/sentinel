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
//! Escalations remain in-memory because they are produced by hooks
//! that have already returned to the operator's chat surface — if
//! the daemon crashes between `escalate()` and the WS send, the
//! event is "lost" only in the sense that the operator will see
//! the next Stop hook fire instead. Persisting escalations is a
//! future improvement (separate file, same lock discipline).

use consul_domain::identity::InstructionId;
use consul_protocol::messages::{BlockReason, InstructionOutcome, RelayInstruction};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::persistent_inbox::PersistentInbox;

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
/// [`PersistentInbox`] is also `Clone` (just a path). Hand a clone
/// to every IPC route or hook that needs to push escalations or
/// pop the inbox.
#[derive(Clone)]
pub struct LegatusHandle {
    escalation_tx: mpsc::UnboundedSender<EscalationKind>,
    inbox: Option<PersistentInbox>,
}

/// Receiver-side of the channels — owned by the connect loop and
/// drained inside its main `tokio::select!`.
pub struct LegatusRuntime {
    pub(crate) escalation_rx: mpsc::UnboundedReceiver<EscalationKind>,
    pub(crate) inbox: Option<PersistentInbox>,
}

/// Construct a paired `(handle, runtime)` with no persistent inbox.
/// Used by the standalone `sentinel legatus connect` path, where
/// received instructions are log-only (no daemon to drain them).
#[must_use]
pub fn make_pair() -> (LegatusHandle, LegatusRuntime) {
    let (escalation_tx, escalation_rx) = mpsc::unbounded_channel();
    let handle = LegatusHandle {
        escalation_tx,
        inbox: None,
    };
    let runtime = LegatusRuntime {
        escalation_rx,
        inbox: None,
    };
    (handle, runtime)
}

/// Construct a paired `(handle, runtime)` backed by the given
/// [`PersistentInbox`].
///
/// Both halves share the same file path; the inbox is cheap to
/// clone (path only). Used by the sentinel daemon path so
/// received instructions survive daemon restart.
#[must_use]
pub fn make_pair_with_inbox(inbox: PersistentInbox) -> (LegatusHandle, LegatusRuntime) {
    let (escalation_tx, escalation_rx) = mpsc::unbounded_channel();
    let handle = LegatusHandle {
        escalation_tx,
        inbox: Some(inbox.clone()),
    };
    let runtime = LegatusRuntime {
        escalation_rx,
        inbox: Some(inbox),
    };
    (handle, runtime)
}

impl LegatusHandle {
    /// Queue an escalation. Returns `Err` only if the legatus
    /// runtime has been dropped (connection closed / process
    /// shutting down).
    ///
    /// # Errors
    ///
    /// Returns [`EscalationSendError`] when the legatus runtime
    /// receiver has been dropped.
    pub fn escalate(&self, event: EscalationKind) -> Result<(), EscalationSendError> {
        self.escalation_tx
            .send(event)
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
        match received {
            EscalationKind::Completed { summary } => assert_eq!(summary.as_deref(), Some("ok")),
            other => panic!("expected Completed, got {other:?}"),
        }
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
}
