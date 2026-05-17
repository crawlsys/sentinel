//! Sentinel's legatus implementation.
//!
//! A legatus is the agent-side counterpart to a consul: it
//! represents one Claude Code session to a consul supervisor over
//! the Consular Protocol (WebSocket). The first commit ships a
//! standalone client driven from
//! [`sentinel legatus connect`](crate::client::connect) — opens
//! the WS, runs the registration handshake, sends heartbeats,
//! logs received [`RelayInstruction`]s, and emits a
//! [`SessionCompleted`] on graceful shutdown.
//!
//! [`RelayInstruction`]: consul_protocol::messages::RelayInstruction
//! [`SessionCompleted`]: consul_protocol::messages::SessionCompleted
//!
//! Follow-up commits will:
//! - Move the long-running WS connection into the sentinel daemon
//!   so it survives across per-hook sentinel invocations
//!   (`sentinel hook --event …`).
//! - Wire the real `PermissionDenied` + `Stop` hooks to emit
//!   `SessionBlocked` / `SessionCompleted` escalations.
//!
//! All protocol wire types live in `consul-protocol`; we depend on
//! that crate via a path dep on the sister repo
//! `legatus-consul-agent`. The `consul-domain` types
//! (`SessionId`, `SessionMasterKey`, etc.) come in transitively
//! through the same path dep.

pub mod client;
pub mod error;
pub mod handle;
pub mod persistent_inbox;

pub use client::{run_connect, run_connect_hosted, ConnectConfig};
pub use error::LegatusError;
pub use handle::{
    make_pair, make_pair_with_inbox, EscalationKind, EscalationSendError, LegatusHandle,
    LegatusRuntime,
};
pub use persistent_inbox::{default_inbox_path, PersistentInbox};

// Convenience re-exports so dependents (e.g. sentinel-cli,
// sentinel-application) can configure a legatus / build a
// RelayInstruction without a direct path-dep on consul-protocol
// / consul-domain.
pub use consul_domain::identity::{InstructionId, SessionId};
pub use consul_protocol::keys::BOOTSTRAP_SECRET_LEN;
pub use consul_protocol::messages::{
    BlockReason, InstructionOutcome, RelayInstruction, RuntimeKind,
};
