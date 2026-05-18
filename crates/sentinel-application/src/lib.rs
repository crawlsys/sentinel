//! Sentinel Application Layer
//!
//! Use cases and orchestration. Coordinates domain logic with
//! infrastructure ports for hook execution, proof management,
//! AI judging, and MCP tool handling.

pub mod appraisal_store;
pub mod auditor;
pub mod cache_efficiency;
pub mod capability_router;
pub mod change_failure;
pub mod channel_events;
pub mod classifier;
pub mod cost_per_point;
pub mod cycle_time;
pub mod cycle_time_analytics;
pub mod cycle_time_prediction;
pub mod dedupe;
pub mod deploy_freq;
pub mod engine;
pub mod evidence_adapters;
pub mod gate;
pub mod hook_metrics;
pub mod hooks;
pub mod interceptor;
pub mod judge_service;
pub mod lead_time;
pub mod legatus_client;
pub mod mcp_handler;
pub mod sandboxed_judge;
pub mod pr_review;
pub mod project_init;
pub mod proof_archive;
pub mod proof_engine;
pub mod reversibility_classifier;
pub mod roi;
pub mod scanner;
pub mod sla;
pub mod tokens;
pub mod tracing_service;
pub mod trust;
pub mod verifier;
pub mod webhook_replay;
pub mod wip_snapshot;
