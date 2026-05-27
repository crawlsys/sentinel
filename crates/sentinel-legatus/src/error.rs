//! [`LegatusError`] — errors the legatus client can surface to
//! its caller (the `sentinel legatus connect` subcommand).

/// Errors produced during a legatus session lifetime.
#[derive(Debug, thiserror::Error)]
pub enum LegatusError {
    /// `--bootstrap-secret` couldn't be hex-decoded to 32 bytes.
    #[error("invalid bootstrap secret: {0}")]
    InvalidBootstrapSecret(String),

    /// WebSocket connect or read/write failed at the transport
    /// level.
    #[error("transport: {0}")]
    Transport(String),

    /// The handshake (Hello → Capabilities → `RegisterSession` →
    /// `SessionRegistered`) didn't complete as expected.
    #[error("handshake: {0}")]
    Handshake(String),

    /// Payload from consulate couldn't be parsed.
    #[error("decode: {0}")]
    Decode(String),

    /// CBOR encoding of an outbound envelope failed.
    #[error("encode: {0}")]
    Encode(String),

    /// MAC verification of an inbound envelope failed.
    #[error("mac mismatch on inbound envelope")]
    MacMismatch,

    /// Consulate rejected the protocol version we offered.
    #[error("protocol version mismatch: consulate offered {accepted_min:?}..{accepted_max:?}")]
    VersionMismatch {
        /// Lower bound consulate accepts.
        accepted_min: Option<String>,
        /// Upper bound consulate accepts.
        accepted_max: Option<String>,
    },
}
