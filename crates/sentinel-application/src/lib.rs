//! Sentinel Application Layer
//!
//! Use cases and orchestration. Coordinates domain logic with
//! infrastructure ports for hook execution, proof management,
//! AI judging, and MCP tool handling.

pub mod channel_events;
pub mod classifier;
pub mod dedupe;
pub mod engine;
pub mod gate;
pub mod hook_metrics;
pub mod hooks;
pub mod interceptor;
pub mod judge_service;
pub mod mcp_handler;
pub mod ntfy_push;
pub mod pr_review;
pub mod project_init;
pub mod proof_engine;
pub mod scanner;
pub mod tokens;
pub mod verifier;
pub mod webhook_replay;
