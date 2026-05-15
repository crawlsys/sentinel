//! Sentinel Application Layer
//!
//! Use cases and orchestration. Coordinates domain logic with
//! infrastructure ports for hook execution, proof management,
//! AI judging, and MCP tool handling.

pub mod cache_efficiency;
pub mod channel_events;
pub mod classifier;
pub mod cost_per_point;
pub mod cycle_time;
pub mod dedupe;
pub mod engine;
pub mod evidence_adapters;
pub mod gate;
pub mod hook_metrics;
pub mod hooks;
pub mod interceptor;
pub mod judge_service;
pub mod mcp_handler;
pub mod paths;
pub mod sandboxed_judge;
pub mod pr_review;
pub mod project_init;
pub mod proof_archive;
pub mod proof_engine;
pub mod roi;
pub mod scanner;
pub mod tokens;
pub mod tracing_service;
pub mod trust;
pub mod verifier;
pub mod webhook_replay;
pub mod wip_snapshot;
