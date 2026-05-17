//! [`LegatusHandle`] — runtime control surface for an in-flight
//! legatus connection, used when the WS connection is hosted by a
//! long-running process (the sentinel daemon) and other components
//! need to push escalations onto it / pop received instructions
//! from it.
//!
//! Two channels:
//!
//! - **escalation** (`mpsc::UnboundedSender<EscalationKind>`) —
//!   handle holds the sender; the connect-loop's runtime holds the
//!   receiver. Calling [`LegatusHandle::escalate`] queues an
//!   escalation that the loop converts to a signed
//!   `SessionBlocked` / `SessionCompleted` / `SessionFailed`
//!   envelope and writes to the WS.
//! - **inbox** (`mpsc::UnboundedSender<RelayInstruction>`) — loop
//!   holds the sender; handle holds the receiver. The loop pushes
//!   every received `RelayInstruction` onto it; callers drain via
//!   [`LegatusHandle::try_pop_inbox`].
//!
//! Both channels are unbounded — slow consumers should never apply
//! backpressure to a session loop reading from a live WS.

use std::sync::Arc;

use consul_protocol::messages::{BlockReason, RelayInstruction};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};

/// The escalation event a caller asks the legatus to send.
///
/// `Serialize`/`Deserialize` are derived with `#[serde(tag = "kind",
/// rename_all = "snake_case")]` so the daemon's HTTP route can
/// take the JSON body directly. Hook clients construct one of:
///
/// ```json
/// {"kind": "completed", "summary": "deployed staging"}
/// {"kind": "failed",    "error":   "tool x crashed"}
/// {"kind": "blocked",   "reason":  {"kind": "permission_denied", "tool": "Bash"}}
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
}

/// Runtime control surface for an in-flight legatus connection.
///
/// Cheap to clone — all state is `Arc`/channel-based. Hand a clone
/// to every IPC route or hook that needs to push escalations or
/// pop the inbox.
#[derive(Clone)]
pub struct LegatusHandle {
    escalation_tx: mpsc::UnboundedSender<EscalationKind>,
    inbox_rx: Arc<Mutex<mpsc::UnboundedReceiver<RelayInstruction>>>,
}

/// Receiver-side of the channels — owned by the connect loop and
/// drained inside its main `tokio::select!`.
pub struct LegatusRuntime {
    pub(crate) escalation_rx: mpsc::UnboundedReceiver<EscalationKind>,
    pub(crate) inbox_tx: mpsc::UnboundedSender<RelayInstruction>,
}

/// Construct a paired `(handle, runtime)`. The handle goes to the
/// caller (e.g. the sentinel daemon's HTTP routes); the runtime
/// goes to [`crate::client::run_connect_hosted`].
#[must_use]
pub fn make_pair() -> (LegatusHandle, LegatusRuntime) {
    let (escalation_tx, escalation_rx) = mpsc::unbounded_channel();
    let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
    let handle = LegatusHandle {
        escalation_tx,
        inbox_rx: Arc::new(Mutex::new(inbox_rx)),
    };
    let runtime = LegatusRuntime {
        escalation_rx,
        inbox_tx,
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
    /// Returns `None` if the inbox is empty.
    pub async fn try_pop_inbox(&self) -> Option<RelayInstruction> {
        let mut rx = self.inbox_rx.lock().await;
        rx.try_recv().ok()
    }

    /// Blocking variant — waits until a `RelayInstruction` arrives
    /// or the runtime drops the sender (returns `None`).
    pub async fn pop_inbox(&self) -> Option<RelayInstruction> {
        let mut rx = self.inbox_rx.lock().await;
        rx.recv().await
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

    use super::*;

    fn fake_instruction() -> RelayInstruction {
        RelayInstruction {
            instruction_id: InstructionId::new(),
            target_session_id: SessionId::new_v7(),
            content: "test".into(),
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
    async fn pop_inbox_returns_pushed_instruction() {
        let (handle, runtime) = make_pair();
        runtime.inbox_tx.send(fake_instruction()).unwrap();
        let instr = handle.try_pop_inbox().await.unwrap();
        assert_eq!(instr.content, "test");
    }

    #[tokio::test]
    async fn try_pop_inbox_returns_none_when_empty() {
        let (handle, _runtime) = make_pair();
        assert!(handle.try_pop_inbox().await.is_none());
    }
}
