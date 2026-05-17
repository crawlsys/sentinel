//! Sentinel Infrastructure Layer
//!
//! IO, HTTP clients, filesystem, git operations.
//! Implements ports defined in the domain layer (`sentinel_domain::ports`).

pub mod activity_log;
pub mod anthropic;
pub mod config;
pub mod env;
pub mod error_log;
pub mod evidence_browserbase;
pub mod evidence_filesystem;
pub mod filesystem;
pub mod git;
pub mod interceptor;
pub mod ipc;
pub mod mcp_transport;
pub mod memory_mcp_client;
pub mod process;
pub mod proof_store;
pub mod qdrant;
pub mod rate_limit;
pub mod reversibility;
pub mod rig_classifier;
pub mod rig_judge;
pub mod security_log;
pub mod state_store;
pub mod stdin;
pub mod stdout;
pub mod transcript;
