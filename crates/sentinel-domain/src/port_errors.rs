//! Bespoke domain error types for the hexagonal ports.
//!
//! Each IO-bearing port in [`crate::ports`] returns one of these instead of
//! `anyhow::Result`, so the domain's contracts are honest (a caller can see
//! *what* can go wrong) and the domain crate carries no `anyhow` dependency.
//! Infrastructure adapters map their internal errors into these via the
//! `From<std::io::Error>` impls or the `Backend(String)` catch-all.
//!
//! Pattern mirrors the pre-existing [`crate::dry_run::AuditorError`]: a small
//! enum per port with manual `Display` + `std::error::Error` impls (no
//! `thiserror` in the domain). Because every variant implements
//! `std::error::Error`, application callers that propagate with `?` inside an
//! `anyhow::Result` fn keep working unchanged — `anyhow::Error: From<E:
//! Error>` absorbs them.

use std::fmt;

/// Build the boilerplate for a simple port error enum:
/// `NotFound(String)` (optional), `Io(String)`, `Backend(String)`, plus
/// `Display`, `std::error::Error`, and `From<std::io::Error>`.
macro_rules! port_error {
    (
        $(#[$meta:meta])*
        $name:ident { $($(#[$vmeta:meta])* $variant:ident),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum $name {
            $($(#[$vmeta])* $variant(String),)+
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $(Self::$variant(msg) => write!(f, concat!(stringify!($variant), ": {}"), msg),)+
                }
            }
        }

        impl std::error::Error for $name {}

        impl From<std::io::Error> for $name {
            fn from(e: std::io::Error) -> Self {
                // Map to Backend — the one variant every port error has. Ports
                // with a dedicated `Io` variant still get an io-shaped message;
                // the distinction only matters where a caller branches on it
                // (FileSystemError), and adapters there can construct `Io`
                // explicitly.
                Self::Backend(e.to_string())
            }
        }

        impl $name {
            /// Wrap any displayable error as the backend/catch-all variant —
            /// the adapter's escape hatch for vendor/transport errors that
            /// don't map to a more specific variant.
            pub fn backend(e: impl fmt::Display) -> Self {
                Self::Backend(e.to_string())
            }
        }
    };
}

port_error! {
    /// Errors from [`crate::ports::GitStatusPort`] — git CLI/transport failures.
    GitError { Io, Backend }
}

port_error! {
    /// Errors from [`crate::ports::VectorStorePort`] — vector DB upsert/scroll
    /// transport or serialization failures.
    VectorStoreError { Io, Backend }
}

port_error! {
    /// Errors from [`crate::ports::FileSystemPort`]. `NotFound` is distinguished
    /// because callers branch on missing-vs-other (e.g. "no state file yet" is
    /// a normal first-run path, not a failure).
    FileSystemError { NotFound, Io, Backend }
}

port_error! {
    /// Errors from [`crate::ports::ProcessPort`] — spawn/exec failures.
    ProcessError { Io, Backend }
}

port_error! {
    /// Errors from [`crate::ports::LlmPort`] — provider unreachable, timed out,
    /// or returned an unusable response.
    LlmError { Unavailable, Timeout, Backend }
}

port_error! {
    /// Errors from [`crate::ports::MemoryMcpPort`] — the memory-mcp subprocess
    /// failed to spawn/handshake, the tool errored, or the payload was missing.
    MemoryMcpError { Io, Backend }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_variant_and_message() {
        assert_eq!(
            FileSystemError::NotFound("x".into()).to_string(),
            "NotFound: x"
        );
        assert_eq!(LlmError::Timeout("5s".into()).to_string(), "Timeout: 5s");
        assert_eq!(
            GitError::Backend("boom".into()).to_string(),
            "Backend: boom"
        );
    }

    #[test]
    fn from_io_error_maps_to_backend_variant() {
        // The blanket `From<io::Error>` maps to Backend (the universal variant).
        // Adapters that want the more specific `Io` variant construct it
        // explicitly; the auto-conversion just needs to not lose the message.
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let e: FileSystemError = io.into();
        assert!(matches!(e, FileSystemError::Backend(_)));
        assert!(e.to_string().contains("nope"));
    }

    #[test]
    fn backend_helper_wraps_displayable() {
        let e = ProcessError::backend("exit 1");
        assert_eq!(e, ProcessError::Backend("exit 1".into()));
    }

    #[test]
    fn errors_are_std_error() {
        // Compile-time proof each enum is a real std::error::Error (so anyhow
        // absorbs them and `?` keeps working in application callers).
        fn assert_err<E: std::error::Error>() {}
        assert_err::<GitError>();
        assert_err::<VectorStoreError>();
        assert_err::<FileSystemError>();
        assert_err::<ProcessError>();
        assert_err::<LlmError>();
        assert_err::<MemoryMcpError>();
    }
}
