//! Sentinel Infrastructure Layer
//!
//! IO, HTTP clients, filesystem, git operations.
//! Implements ports defined in the application layer.

pub mod activity_log;
pub mod anthropic;
pub mod config;
pub mod rig_judge;
pub mod error_log;
pub mod git;
pub mod ipc;
pub mod mcp_transport;
pub mod proof_store;
pub mod state_store;
pub mod stdin;
pub mod stdout;
pub mod transcript;
