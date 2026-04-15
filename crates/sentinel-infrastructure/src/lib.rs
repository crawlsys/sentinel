//! Sentinel Infrastructure Layer
//!
//! IO, HTTP clients, filesystem, git operations.
//! Implements ports defined in the domain layer (`sentinel_domain::ports`).

pub mod activity_log;
pub mod filesystem;
pub mod process;
pub mod qdrant;
pub mod anthropic;
pub mod config;
pub mod error_log;
pub mod git;
pub mod ipc;
pub mod mcp_transport;
pub mod proof_store;
pub mod rate_limit;
pub mod rig_classifier;
pub mod rig_judge;
pub mod security_log;
pub mod state_store;
pub mod stdin;
pub mod stdout;
pub mod transcript;
