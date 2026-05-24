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

use std::sync::atomic::{AtomicU8, Ordering};
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
/// state. Internally just an `Arc<AtomicU8>` so cloning is two
/// pointer ops and reads/writes are wait-free.
///
/// The wrapper task and the HTTP route handler each hold a clone;
/// the daemon constructs the canonical one at startup.
#[derive(Clone, Debug, Default)]
pub struct ConnectionStatus {
    inner: Arc<AtomicU8>,
}

impl ConnectionStatus {
    /// Construct a fresh status in [`ConnectionState::Disconnected`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
