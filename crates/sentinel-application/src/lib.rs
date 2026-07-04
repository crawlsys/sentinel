//! Sentinel Application Layer
//!
//! Use cases and orchestration. Coordinates domain logic with
//! infrastructure ports for hook execution, proof management,
//! AI judging, and MCP tool handling.

pub mod appraisal_store;
pub mod auditor;
pub mod autocron_config;
pub mod ba_orchestrator;
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
pub mod delegation_service;
pub mod deploy_freq;
pub mod dev_scorecard;
pub mod eval_run;
pub mod evidence_adapters;
pub mod gate;
pub mod hook_metrics;
pub mod hooks;
pub mod interceptor;
pub mod issue_suggest;
pub mod judge_service;
pub mod lead_time;
pub mod linear_code_audit;
pub mod linear_health_score;
pub mod linear_pm_audit;
pub mod mcp_guardian;
pub mod mcp_handler;
pub mod memory_telemetry;
pub mod operational_summary;
pub mod paths;
pub mod pr_review;
pub mod project_init;
pub mod proof_archive;
pub mod proof_engine;
pub mod reversibility_classifier;
pub mod roi;
pub mod sandboxed_judge;
pub mod scanner;
pub mod severity;
pub mod sla;
pub mod telemetry;
pub mod throughput;
pub mod token_cost;
pub mod tokens;
pub mod tracing_service;
pub mod trust;
pub mod verifier;
pub mod webhook_replay;
pub mod wip_snapshot;
