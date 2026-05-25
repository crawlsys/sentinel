//! Sentinel Application Layer
//!
//! Use cases and orchestration. Coordinates domain logic with
//! infrastructure ports for hook execution, proof management,
//! AI judging, and MCP tool handling.

pub mod appraisal_store;
pub mod auditor;
pub mod ba_orchestrator;
pub mod cache_efficiency;
pub mod capability_router;
pub mod change_failure;
pub mod channel_events;
pub mod classifier;
pub mod constitution_gate_runtime;
pub mod cost_per_point;
pub mod cycle_time;
pub mod cycle_time_analytics;
pub mod cycle_time_prediction;
pub mod dedupe;
pub mod deploy_freq;
pub mod engine;
pub mod eval_run;
pub mod evidence_adapters;
pub mod gate;
pub mod hook_metrics;
pub mod hooks;
pub mod interceptor;
pub mod issue_suggest;
pub mod judge_enforcement;
pub mod judge_service;
pub mod lead_time;
pub mod legatus_client;
pub mod master_dashboard;
pub mod mcp_handler;
pub mod pr_review;
/// Sentinel-side Praefectus client surface (Fabrica ADR-001 §1 + ADR-002 §3).
/// Trait + in-memory stub; production HTTP/IPC adapter deferred.
pub mod praefectus_client;
pub mod project_init;
pub mod witness_verifier_adapter;
pub mod proof_archive;
pub mod proof_engine;
pub mod reversibility_classifier;
pub mod roi;
pub mod sandboxed_judge;
pub mod scanner;
pub mod sla;
pub mod throughput;
pub mod tokens;
pub mod tracing_service;
pub mod trust;
pub mod verifier;
pub mod webhook_replay;
pub mod wip_snapshot;
