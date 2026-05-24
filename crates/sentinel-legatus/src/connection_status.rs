//! [`ConnectionStatus`] — lock-free observable state for the
//! legatus reconnect wrapper.
//!
//! The wrapper task and the daemon's HTTP route handler each hold a
//! clone of the same `ConnectionStatus`. The wrapper writes
//! transitions (Connecting -> Connected -> Reconnecting -> ...) as
//! they happen; the HTTP handler reads the current state for the
//! `GET /legatus/health` endpoint without locking the wrapper.
//!
//! The state is encoded as a single byte in an `AtomicU8` so reads
//! and writes are wait-free, which matters because the HTTP handler
//! must not block on a hot WebSocket loop.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

/// Current state of the legatus -> consulate connection.
///
/// Wire-encoded as a `u8` inside [`ConnectionStatus`]; the `repr(u8)`
/// discriminants below double as the storage values. Unknown bytes
/// decode to [`ConnectionState::Disconnected`] (the conservative
/// "we don't know, assume worst" choice).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ConnectionState {
    /// Not connected. Either the wrapper has not started yet, or it
    /// has finished (either cleanly via cancel or fatally via
    /// `VersionMismatch`). This is the initial state.
    Disconnected = 0,
    /// First connection attempt is in flight (TCP / WS handshake /
    /// registration). No reconnects have happened yet.
    Connecting = 1,
    /// Handshake + registration succeeded; the session loop is
    /// running and the wrapper considers the legatus reachable.
    Connected = 2,
    /// Previously connected, currently in the backoff window
    /// between retries. Will transition to Connecting on the next
    /// attempt.
    Reconnecting = 3,
}

impl ConnectionState {
    /// Decode a stored byte. Unknown bytes are treated as
    /// `Disconnected` (defensive default — never panic on read).
    #[must_use]
    pub const fn from_u8(byte: u8) -> Self {
        match byte {
            1 => Self::Connecting,
            2 => Self::Connected,
            3 => Self::Reconnecting,
            _ => Self::Disconnected,
        }
    }

    /// Stable string for the JSON wire form on the `/legatus/health`
    /// route. Lowercased snake-case so it matches the rest of the
    /// daemon API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
        }
    }
}

/// Observable, clone-cheap handle on the wrapper's connection
/// state. Internally an `Arc<AtomicU8>` (the live state byte) +
/// an optional `ConnectionEventLog` (the persistent JSONL stream).
/// Cloning is a handful of pointer ops; reads/writes are wait-free.
///
/// The wrapper task and the HTTP route handler each hold a clone;
/// the daemon constructs the canonical one at startup.
#[derive(Clone, Debug, Default)]
pub struct ConnectionStatus {
    inner: Arc<AtomicU8>,
    attempt: Arc<AtomicU64>,
    event_log: Option<crate::connection_event_log::ConnectionEventLog>,
}

impl ConnectionStatus {
    /// Construct a fresh status in [`ConnectionState::Disconnected`]
    /// with no event log attached. Tests that don't need on-disk
    /// history use this; production attaches a log via
    /// [`Self::with_event_log`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a persistent event log so every transition is also
    /// appended as a JSONL line. Cheap (the `ConnectionEventLog`
    /// is itself `Arc`-shaped internally).
    #[must_use]
    pub fn with_event_log(
        mut self,
        log: crate::connection_event_log::ConnectionEventLog,
    ) -> Self {
        self.event_log = Some(log);
        self
    }

    /// Borrow the optional event log. The wrapper uses this to
    /// record transitions; callers who only want the live state
    /// can ignore.
    #[must_use]
    pub fn event_log(&self) -> Option<&crate::connection_event_log::ConnectionEventLog> {
        self.event_log.as_ref()
    }

    /// Snapshot the current state.
    #[must_use]
    pub fn get(&self) -> ConnectionState {
        ConnectionState::from_u8(self.inner.load(Ordering::Acquire))
    }

    /// Publish a new state. Cheap, lock-free, ordered against later
    /// `get()` calls in other tasks (Release/Acquire pair).
    pub fn set(&self, state: ConnectionState) {
        self.inner.store(state as u8, Ordering::Release);
    }

    /// Snapshot the current attempt counter — incremented once per
    /// call into `run_connect_hosted` by the reconnect wrapper.
    /// Useful in `/legatus/health` for "how many reconnects has
    /// this daemon survived?" and in the event log for correlating
    /// transitions.
    #[must_use]
    pub fn attempt(&self) -> u64 {
        self.attempt.load(Ordering::Acquire)
    }

    /// Increment + return the new attempt counter. The reconnect
    /// wrapper calls this once at the top of each attempt iteration
    /// so observers (and the event log) see a monotonic series.
    pub fn bump_attempt(&self) -> u64 {
        self.attempt.fetch_add(1, Ordering::AcqRel) + 1
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disconnected() {
        assert_eq!(ConnectionStatus::new().get(), ConnectionState::Disconnected);
    }

    #[test]
    fn transitions_round_trip_through_atomic() {
        let s = ConnectionStatus::new();
        s.set(ConnectionState::Connecting);
        assert_eq!(s.get(), ConnectionState::Connecting);
        s.set(ConnectionState::Connected);
        assert_eq!(s.get(), ConnectionState::Connected);
        s.set(ConnectionState::Reconnecting);
        assert_eq!(s.get(), ConnectionState::Reconnecting);
        s.set(ConnectionState::Disconnected);
        assert_eq!(s.get(), ConnectionState::Disconnected);
    }

    #[test]
    fn clone_is_shared_view_of_same_atomic() {
        let a = ConnectionStatus::new();
        let b = a.clone();
        a.set(ConnectionState::Connected);
        assert_eq!(
            b.get(),
            ConnectionState::Connected,
            "clones must observe writes through the original — they share an Arc"
        );
    }

    #[test]
    fn unknown_byte_decodes_to_disconnected() {
        // Defensive: from_u8 must never panic, and must give a
        // conservative answer for any byte that isn't a known
        // discriminant.
        for b in [4u8, 5, 99, 255] {
            assert_eq!(ConnectionState::from_u8(b), ConnectionState::Disconnected);
        }
    }

    #[test]
    fn as_str_is_stable_for_each_variant() {
        // The /legatus/health JSON wire form depends on these
        // strings; pin them so a future refactor doesn't silently
        // break dashboards / smoke tests.
        assert_eq!(ConnectionState::Disconnected.as_str(), "disconnected");
        assert_eq!(ConnectionState::Connecting.as_str(), "connecting");
        assert_eq!(ConnectionState::Connected.as_str(), "connected");
        assert_eq!(ConnectionState::Reconnecting.as_str(), "reconnecting");
    }
}
