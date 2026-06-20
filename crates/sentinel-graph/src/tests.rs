//! Unit tests for the phase-graph engine.
//!
//! The headline test is [`fresh_process_restores_checkpoint`]: it proves the
//! durable-execution property that the whole integration rests on — a graph
//! compiled in one "process" checkpoints to sqlite, and a *separately
//! compiled* graph (standing in for the next sentinel hook invocation) reads
//! that checkpoint back via `load_latest`.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
};

use langgraph_core::application::services::ChatModelStream;
use langgraph_core::domain::value_objects::{Message, TokenUsage, ToolCall, START};
use sentinel_domain::judge::JudgeModel;
use sentinel_domain::workflow::{
    DyadVerdicts, RoleDyad, SkillWorkflow, StepStatus, WorkflowPhase, WorkflowState, WorkflowStep,
};

use crate::{
    compile_skill_graph_with_backend, compile_skill_graph_with_checkpointer, next_phase_target,
    phase_checkpointer, required_phase_edge_contract, required_phase_node_metadata,
    required_phase_node_runtime_contract, required_phase_runtime_contract,
    required_phase_schema_contract, PhaseCheckpointerConfig, PhaseGraphCheckpointSnapshot,
    PhaseGraphCheckpointWrite, PhaseGraphEdgeInfo, PhaseGraphErrorEvent, PhaseGraphNodeInfo,
    PhaseGraphState, PhaseGraphStreamPart, PhaseGraphWriteHistoryEntry, V3PhaseProjectionDrain,
    Verdict,
};

static PHASE_CHECKPOINTER_ENV_LOCK: Mutex<()> = Mutex::new(());

fn with_phase_checkpointer_env<R>(
    backend: Option<&str>,
    postgres_url: Option<&str>,
    postgres_schema: Option<&str>,
    tenant_scope: Option<&str>,
    f: impl FnOnce() -> R,
) -> R {
    with_phase_checkpointer_env_full(
        backend,
        postgres_url,
        postgres_schema,
        None,
        None,
        tenant_scope,
        f,
    )
}

fn with_phase_checkpointer_env_full<R>(
    backend: Option<&str>,
    postgres_url: Option<&str>,
    postgres_schema: Option<&str>,
    redis_url: Option<&str>,
    redis_ttl_secs: Option<&str>,
    tenant_scope: Option<&str>,
    f: impl FnOnce() -> R,
) -> R {
    let _guard = PHASE_CHECKPOINTER_ENV_LOCK
        .lock()
        .expect("phase checkpointer env lock poisoned");
    let previous_backend = std::env::var_os(PhaseCheckpointerConfig::BACKEND_ENV);
    let previous_url = std::env::var_os(PhaseCheckpointerConfig::POSTGRES_URL_ENV);
    let previous_schema = std::env::var_os(PhaseCheckpointerConfig::POSTGRES_SCHEMA_ENV);
    let previous_redis_url = std::env::var_os(PhaseCheckpointerConfig::REDIS_URL_ENV);
    let previous_redis_ttl = std::env::var_os(PhaseCheckpointerConfig::REDIS_TTL_SECS_ENV);
    let previous_tenant = std::env::var_os(crate::LANGGRAPH_TENANT_ENV);

    match backend {
        Some(value) => std::env::set_var(PhaseCheckpointerConfig::BACKEND_ENV, value),
        None => std::env::remove_var(PhaseCheckpointerConfig::BACKEND_ENV),
    }
    match postgres_url {
        Some(value) => std::env::set_var(PhaseCheckpointerConfig::POSTGRES_URL_ENV, value),
        None => std::env::remove_var(PhaseCheckpointerConfig::POSTGRES_URL_ENV),
    }
    match postgres_schema {
        Some(value) => std::env::set_var(PhaseCheckpointerConfig::POSTGRES_SCHEMA_ENV, value),
        None => std::env::remove_var(PhaseCheckpointerConfig::POSTGRES_SCHEMA_ENV),
    }
    match redis_url {
        Some(value) => std::env::set_var(PhaseCheckpointerConfig::REDIS_URL_ENV, value),
        None => std::env::remove_var(PhaseCheckpointerConfig::REDIS_URL_ENV),
    }
    match redis_ttl_secs {
        Some(value) => std::env::set_var(PhaseCheckpointerConfig::REDIS_TTL_SECS_ENV, value),
        None => std::env::remove_var(PhaseCheckpointerConfig::REDIS_TTL_SECS_ENV),
    }
    match tenant_scope {
        Some(value) => std::env::set_var(crate::LANGGRAPH_TENANT_ENV, value),
        None => std::env::remove_var(crate::LANGGRAPH_TENANT_ENV),
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

    match previous_backend {
        Some(value) => std::env::set_var(PhaseCheckpointerConfig::BACKEND_ENV, value),
        None => std::env::remove_var(PhaseCheckpointerConfig::BACKEND_ENV),
    }
    match previous_url {
        Some(value) => std::env::set_var(PhaseCheckpointerConfig::POSTGRES_URL_ENV, value),
        None => std::env::remove_var(PhaseCheckpointerConfig::POSTGRES_URL_ENV),
    }
    match previous_schema {
        Some(value) => std::env::set_var(PhaseCheckpointerConfig::POSTGRES_SCHEMA_ENV, value),
        None => std::env::remove_var(PhaseCheckpointerConfig::POSTGRES_SCHEMA_ENV),
    }
    match previous_redis_url {
        Some(value) => std::env::set_var(PhaseCheckpointerConfig::REDIS_URL_ENV, value),
        None => std::env::remove_var(PhaseCheckpointerConfig::REDIS_URL_ENV),
    }
    match previous_redis_ttl {
        Some(value) => std::env::set_var(PhaseCheckpointerConfig::REDIS_TTL_SECS_ENV, value),
        None => std::env::remove_var(PhaseCheckpointerConfig::REDIS_TTL_SECS_ENV),
    }
    match previous_tenant {
        Some(value) => std::env::set_var(crate::LANGGRAPH_TENANT_ENV, value),
        None => std::env::remove_var(crate::LANGGRAPH_TENANT_ENV),
    }

    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn phase(id: &str) -> WorkflowPhase {
    WorkflowPhase {
        id: id.to_string(),
        file: format!("{id}.md"),
        required: true,
        judge: JudgeModel::Sonnet,
        description: format!("{id} phase"),
        required_dyad: None,
    }
}

fn step(id: &str) -> WorkflowStep {
    WorkflowStep {
        id: id.to_string(),
        description: format!("{id} step"),
        blocker: false,
        baseline_threshold: 0,
        judge: None,
        timeout_ms: None,
        retry_policy: Default::default(),
        circuit_breaker: Default::default(),
        provides: Vec::new(),
        requires: Vec::new(),
        external: Vec::new(),
        inaccessible: false,
        deprecated: None,
        r#override: None,
        extra: serde_json::Value::Null,
    }
}

/// Minimal 3-phase workflow fixture.
fn fixture() -> SkillWorkflow {
    SkillWorkflow {
        skill: "linear".to_string(),
        phases: vec![phase("claim"), phase("fetch"), phase("review")],
        blocked_tool_prefixes: Vec::new(),
        blocked_bash_patterns: Vec::new(),
        bash_allowlist: Vec::new(),
    }
}

fn fixture_for(skill: &str) -> SkillWorkflow {
    SkillWorkflow {
        skill: skill.to_string(),
        ..fixture()
    }
}

async fn seed_gate(graph: &crate::CompiledPhaseGraph, skill: &str, session_id: &str) {
    graph
        .run_until_gate(skill, session_id)
        .await
        .expect("initial gate checkpoint");
}

#[test]
fn phase_checkpointer_config_defaults_to_sqlite() {
    with_phase_checkpointer_env(None, None, None, None, || {
        let config =
            PhaseCheckpointerConfig::from_env("/tmp/sentinel-phase.db").expect("sqlite config");
        assert_eq!(
            config,
            PhaseCheckpointerConfig::Sqlite {
                database_path: "/tmp/sentinel-phase.db".to_string()
            }
        );
        assert_eq!(config.backend_name(), "sqlite");
        assert_eq!(config.scope_name(), "database_path:/tmp/sentinel-phase.db");
    });
}

#[test]
fn phase_checkpointer_config_accepts_postgres_schema() {
    with_phase_checkpointer_env(
        Some("postgres"),
        Some("postgres://sentinel:sentinel@localhost/sentinel"),
        Some("phase_graph"),
        Some("legatus_ai"),
        || {
            let config =
                PhaseCheckpointerConfig::from_env("/tmp/ignored.db").expect("postgres config");
            assert_eq!(
                config,
                PhaseCheckpointerConfig::Postgres {
                    database_url: "postgres://sentinel:sentinel@localhost/sentinel".to_string(),
                    schema: "phase_graph".to_string(),
                }
            );
            assert_eq!(config.backend_name(), "postgres");
            assert_eq!(config.scope_name(), "schema:phase_graph");
        },
    );
}

#[test]
fn phase_checkpointer_config_requires_postgres_schema() {
    with_phase_checkpointer_env(
        Some("postgres"),
        Some("postgres://sentinel:sentinel@localhost/sentinel"),
        None,
        Some("legatus_ai"),
        || {
            let err = PhaseCheckpointerConfig::from_env("/tmp/ignored.db")
                .expect_err("postgres schema must be required");
            let message = err.to_string();
            assert!(message.contains(PhaseCheckpointerConfig::BACKEND_ENV));
            assert!(message.contains(PhaseCheckpointerConfig::POSTGRES_SCHEMA_ENV));
            assert!(
                !message.contains("implicit Postgres schema"),
                "postgres selection must not use an implicit Postgres schema: {message}"
            );
        },
    );
}

#[test]
fn phase_checkpointer_config_accepts_redis_ttl() {
    with_phase_checkpointer_env_full(
        Some("redis"),
        None,
        None,
        Some("redis://localhost:6379/0"),
        Some("3600"),
        Some("legatus_ai"),
        || {
            let config =
                PhaseCheckpointerConfig::from_env("/tmp/ignored.db").expect("redis config");
            assert_eq!(
                config,
                PhaseCheckpointerConfig::Redis {
                    redis_url: "redis://localhost:6379/0".to_string(),
                    ttl_seconds: Some(3600),
                }
            );
            assert_eq!(config.backend_name(), "redis");
            assert_eq!(config.scope_name(), "ttl_seconds:3600");
        },
    );
}

#[test]
fn phase_checkpointer_config_accepts_redis_without_ttl() {
    with_phase_checkpointer_env_full(
        Some("redis"),
        None,
        None,
        Some("redis://localhost:6379/1"),
        None,
        Some("legatus_ai"),
        || {
            let config =
                PhaseCheckpointerConfig::from_env("/tmp/ignored.db").expect("redis config");
            assert_eq!(config.backend_name(), "redis");
            assert_eq!(config.scope_name(), "ttl_seconds:none");
        },
    );
}

#[test]
fn phase_checkpointer_config_rejects_backend_aliases_without_normalization() {
    with_phase_checkpointer_env_full(
        Some("redis-checkpoint"),
        None,
        None,
        Some("redis://localhost:6379/1"),
        None,
        Some("legatus_ai"),
        || {
            let err = PhaseCheckpointerConfig::from_env("/tmp/ignored.db")
                .expect_err("redis-checkpoint alias must be rejected");
            let message = err.to_string();

            assert!(
                message.contains("unsupported phase graph checkpointer backend 'redis-checkpoint'")
            );
            assert!(message.contains("expected sqlite, postgres, or redis"));
        },
    );

    with_phase_checkpointer_env(
        Some("postgresql"),
        Some("postgres://sentinel:sentinel@localhost/sentinel"),
        None,
        Some("legatus_ai"),
        || {
            let err = PhaseCheckpointerConfig::from_env("/tmp/ignored.db")
                .expect_err("postgresql alias must be rejected");
            let message = err.to_string();

            assert!(message.contains("unsupported phase graph checkpointer backend 'postgresql'"));
            assert!(message.contains("expected sqlite, postgres, or redis"));
        },
    );
}

#[test]
fn phase_checkpointer_config_requires_postgres_url() {
    with_phase_checkpointer_env(Some("postgres"), None, None, None, || {
        let err = PhaseCheckpointerConfig::from_env("/tmp/ignored.db")
            .expect_err("postgres URL must be required");
        let message = err.to_string();
        assert!(message.contains(PhaseCheckpointerConfig::POSTGRES_URL_ENV));
        assert!(
            !message.contains("sqlite"),
            "postgres selection must not use sqlite: {message}"
        );
    });
}

#[test]
fn phase_checkpointer_config_requires_tenant_scope_for_postgres() {
    with_phase_checkpointer_env(
        Some("postgres"),
        Some("postgres://sentinel:sentinel@localhost/sentinel"),
        Some("phase_graph"),
        None,
        || {
            let err = PhaseCheckpointerConfig::from_env("/tmp/ignored.db")
                .expect_err("postgres config must require tenant scope");
            let message = err.to_string();
            assert!(message.contains(PhaseCheckpointerConfig::BACKEND_ENV));
            assert!(message.contains(crate::LANGGRAPH_TENANT_ENV));
            assert!(message.contains("tenant-scoped"));
            assert!(
                !message.contains("sqlite"),
                "postgres selection must not use sqlite: {message}"
            );
        },
    );
}

#[test]
fn phase_checkpointer_config_requires_redis_url() {
    with_phase_checkpointer_env_full(Some("redis"), None, None, None, None, None, || {
        let err = PhaseCheckpointerConfig::from_env("/tmp/ignored.db")
            .expect_err("redis URL must be required");
        let message = err.to_string();
        assert!(message.contains(PhaseCheckpointerConfig::REDIS_URL_ENV));
        assert!(
            !message.contains("sqlite"),
            "redis selection must not use sqlite: {message}"
        );
    });
}

#[cfg(all(feature = "postgres", feature = "redis"))]
#[test]
fn explicit_hosted_phase_checkpointer_config_requires_tenant_scope() {
    with_phase_checkpointer_env(None, None, None, None, || {
        let postgres_err =
            crate::tenant_scope_for_phase_checkpointer_config(&PhaseCheckpointerConfig::Postgres {
                database_url: "postgres://sentinel:sentinel@localhost/sentinel".to_string(),
                schema: "phase_graph".to_string(),
            })
            .expect_err("explicit postgres config must require tenant scope");
        assert!(postgres_err
            .to_string()
            .contains(crate::LANGGRAPH_TENANT_ENV));
        assert!(postgres_err.to_string().contains("tenant-scoped"));

        let redis_err =
            crate::tenant_scope_for_phase_checkpointer_config(&PhaseCheckpointerConfig::Redis {
                redis_url: "redis://localhost:6379/0".to_string(),
                ttl_seconds: Some(60),
            })
            .expect_err("explicit redis config must require tenant scope");
        assert!(redis_err.to_string().contains(crate::LANGGRAPH_TENANT_ENV));
        assert!(redis_err.to_string().contains("tenant-scoped"));
    });
}

#[test]
fn phase_checkpointer_config_requires_tenant_scope_for_redis() {
    with_phase_checkpointer_env_full(
        Some("redis"),
        None,
        None,
        Some("redis://localhost:6379/0"),
        None,
        None,
        || {
            let err = PhaseCheckpointerConfig::from_env("/tmp/ignored.db")
                .expect_err("redis config must require tenant scope");
            let message = err.to_string();
            assert!(message.contains(PhaseCheckpointerConfig::BACKEND_ENV));
            assert!(message.contains(crate::LANGGRAPH_TENANT_ENV));
            assert!(message.contains("tenant-scoped"));
            assert!(
                !message.contains("sqlite"),
                "redis selection must not use sqlite: {message}"
            );
        },
    );
}

#[test]
fn phase_checkpointer_config_rejects_empty_backend_env() {
    with_phase_checkpointer_env(Some("   "), None, None, None, || {
        let err = PhaseCheckpointerConfig::from_env("/tmp/ignored.db")
            .expect_err("empty backend must be rejected");
        let message = err.to_string();
        assert!(message.contains(PhaseCheckpointerConfig::BACKEND_ENV));
        assert!(message.contains("set but empty"));
    });
}

#[test]
fn phase_checkpointer_config_rejects_empty_postgres_schema_env() {
    with_phase_checkpointer_env(
        Some("postgres"),
        Some("postgres://sentinel:sentinel@localhost/sentinel"),
        Some("   "),
        Some("legatus_ai"),
        || {
            let err = PhaseCheckpointerConfig::from_env("/tmp/ignored.db")
                .expect_err("empty schema must be rejected when configured");
            let message = err.to_string();
            assert!(message.contains(PhaseCheckpointerConfig::POSTGRES_SCHEMA_ENV));
            assert!(message.contains("set but empty"));
        },
    );
}

#[test]
fn phase_checkpointer_config_rejects_invalid_redis_ttl_env() {
    with_phase_checkpointer_env_full(
        Some("redis"),
        None,
        None,
        Some("redis://localhost:6379/0"),
        Some("0"),
        Some("legatus_ai"),
        || {
            let err = PhaseCheckpointerConfig::from_env("/tmp/ignored.db")
                .expect_err("zero Redis TTL must be rejected when configured");
            let message = err.to_string();
            assert!(message.contains(PhaseCheckpointerConfig::REDIS_TTL_SECS_ENV));
            assert!(message.contains("greater than zero"));
        },
    );
}

#[test]
fn phase_checkpointer_config_rejects_unknown_backend() {
    with_phase_checkpointer_env(Some("unsupported"), None, None, None, || {
        let err = PhaseCheckpointerConfig::from_env("/tmp/ignored.db")
            .expect_err("unknown backend must be rejected");
        let message = err.to_string();
        assert!(message.contains("unsupported phase graph checkpointer backend 'unsupported'"));
        assert!(message.contains("expected sqlite, postgres, or redis"));
    });
}

#[test]
fn phase_thread_id_is_tenant_scoped_when_configured() {
    let scoped = sentinel_domain::langgraph_thread::tenant_scoped_thread_id(
        "sentinel.phase.linear.session-123".to_string(),
        Some("legatus_ai"),
    )
    .expect("valid tenant scope");

    assert_eq!(
        scoped,
        "tenant:legatus_ai:sentinel.phase.linear.session-123"
    );
}

#[test]
fn phase_thread_id_rejects_malformed_tenant_scope() {
    let err = sentinel_domain::langgraph_thread::tenant_scoped_thread_id(
        "sentinel.phase.linear.session-123".to_string(),
        Some("tenant:escape"),
    )
    .expect_err("tenant delimiter injection must fail");
    let message = err.to_string();

    assert!(message.contains(crate::LANGGRAPH_TENANT_ENV));
    assert!(message.contains("invalid characters"));
}

#[cfg(not(feature = "postgres"))]
#[tokio::test]
async fn postgres_phase_saver_request_fails_without_postgres_feature() {
    let result = crate::phase_checkpointer_for_config(PhaseCheckpointerConfig::Postgres {
        database_url: "postgres://sentinel:sentinel@localhost/sentinel".to_string(),
        schema: "phase_graph".to_string(),
    })
    .await;
    let err = match result {
        Ok(_) => panic!("postgres backend must require postgres feature"),
        Err(err) => err,
    };
    assert!(
        err.to_string()
            .contains("built without the postgres feature"),
        "unexpected error: {err}"
    );
}

#[cfg(not(feature = "redis"))]
#[tokio::test]
async fn redis_phase_saver_request_fails_without_redis_feature() {
    let result = crate::phase_checkpointer_for_config(PhaseCheckpointerConfig::Redis {
        redis_url: "redis://localhost:6379/0".to_string(),
        ttl_seconds: Some(60),
    })
    .await;
    let err = match result {
        Ok(_) => panic!("redis backend must require redis feature"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("built without the redis feature"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn compiles_linear_workflow_with_all_phases() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    assert_eq!(
        graph.phase_ids(),
        &[
            "claim".to_string(),
            "fetch".to_string(),
            "review".to_string()
        ],
    );
}

#[tokio::test]
async fn compiled_phase_graph_thread_id_uses_compiled_tenant_scope_after_env_drift() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_backend(
        &fixture(),
        saver.into_saver(),
        "redis".to_string(),
        "ttl_seconds:none".to_string(),
        Some("legatus_ai".to_string()),
    )
    .expect("compile hosted phase graph");

    with_phase_checkpointer_env_full(
        Some("redis"),
        None,
        None,
        Some("redis://localhost:6379/0"),
        None,
        Some("wrong_tenant"),
        || {
            let topology = graph.introspect("sess-hosted").expect("hosted topology");
            assert_eq!(
                topology.thread_id,
                "tenant:legatus_ai:sentinel.phase.linear.sess-hosted"
            );
            assert_eq!(
                topology.checkpointer_tenant_scope.as_deref(),
                Some("legatus_ai")
            );
            assert!(topology.nodes.iter().all(|node| {
                node.metadata
                    .get("sentinel.checkpointer_tenant_scope")
                    .map(String::as_str)
                    == Some("legatus_ai")
            }));
        },
    );
}

#[tokio::test]
async fn compiled_phase_graph_rejects_missing_hosted_tenant_scope() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let result = compile_skill_graph_with_backend(
        &fixture(),
        saver.into_saver(),
        "postgres".to_string(),
        "schema:phase_graph".to_string(),
        None,
    );
    let err = match result {
        Ok(_) => panic!("hosted phase graph must carry tenant scope"),
        Err(err) => err,
    };

    assert!(err.to_string().contains(crate::LANGGRAPH_TENANT_ENV));
    assert!(err.to_string().contains("tenant-scoped"));
}

#[tokio::test]
async fn compiled_phase_graph_rejects_sqlite_tenant_scope() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let result = compile_skill_graph_with_backend(
        &fixture(),
        saver.into_saver(),
        "sqlite".to_string(),
        "database_path::memory:".to_string(),
        Some("legatus_ai".to_string()),
    );
    let err = match result {
        Ok(_) => panic!("sqlite phase graph must not carry tenant scope"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("SQLite phase graph"));
    assert!(err
        .to_string()
        .contains("must not carry hosted tenant metadata"));
}

#[tokio::test]
async fn graph_introspection_exposes_langgraph_runtime_contract() {
    let checkpointer = phase_checkpointer(":memory:").await.expect("checkpointer");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), checkpointer).expect("compile");
    let topology = graph.introspect("sess-topology").expect("topology");

    assert_eq!(topology.skill, "linear");
    assert_eq!(topology.thread_id, "sentinel.phase.linear.sess-topology");
    assert!(topology.durable_checkpointer);
    assert_eq!(topology.checkpointer_backend, "sqlite");
    assert_eq!(topology.checkpointer_scope, "database_path::memory:");
    assert_eq!(topology.checkpointer_tenant_scope, None);
    assert!(topology.auto_checkpoint);
    assert!(
        topology.max_iterations > topology.phase_order.len(),
        "LangGraph max_iterations should leave room for interrupt re-entry"
    );
    assert_eq!(
        topology.phase_order,
        vec![
            "claim".to_string(),
            "fetch".to_string(),
            "review".to_string()
        ]
    );
    let state_schema = topology.schemas.state.as_ref().expect("state schema");
    assert_eq!(state_schema["type"], "object");
    assert_eq!(state_schema["properties"]["skill"]["const"], "linear");
    assert_eq!(state_schema["x-sentinel"]["graph"], "phase");
    assert_eq!(
        state_schema["properties"]["phase_order"]["const"],
        serde_json::json!(["claim", "fetch", "review"])
    );
    assert_eq!(
        state_schema["x-sentinel"]["required_phases"],
        serde_json::json!(["claim", "fetch", "review"])
    );
    assert_eq!(
        state_schema["properties"]["replay_events"]["type"],
        serde_json::json!("array")
    );
    assert_eq!(
        state_schema["properties"]["phase_error_events"]["items"]["properties"]["phase_id"]["enum"],
        serde_json::json!(["claim", "fetch", "review"])
    );
    assert!(topology.schemas.input.is_some());
    assert!(topology.schemas.output.is_some());
    assert!(topology.schemas.context.is_none());

    let claim = topology
        .nodes
        .iter()
        .find(|node| node.id == "claim")
        .expect("claim node");
    assert_eq!(
        claim.metadata.get("sentinel.phase").map(String::as_str),
        Some("claim")
    );
    assert_eq!(
        claim.metadata.get("sentinel.graph").map(String::as_str),
        Some("phase")
    );
    assert_eq!(
        claim.metadata.get("sentinel.node").map(String::as_str),
        Some("claim")
    );
    assert_eq!(
        claim.metadata.get("sentinel.skill").map(String::as_str),
        Some("linear")
    );
    assert_eq!(
        claim
            .metadata
            .get("sentinel.checkpointer_backend")
            .map(String::as_str),
        Some("sqlite")
    );
    assert_eq!(
        claim
            .metadata
            .get("sentinel.checkpointer_scope")
            .map(String::as_str),
        Some("database_path::memory:")
    );
    assert_eq!(
        claim
            .metadata
            .get("sentinel.checkpointer_tenant_scope")
            .map(String::as_str),
        Some("")
    );
    assert!(claim.has_timeout_policy);
    assert!(claim.has_error_handler);
    assert!(claim.interrupt_after);
    assert!(!claim.interrupt_before);
    assert!(!claim.deferred);
    assert!(topology.nodes.iter().all(|node| {
        node.has_timeout_policy
            && node.has_error_handler
            && node.interrupt_after
            && !node.interrupt_before
            && node.metadata.get("sentinel.graph").map(String::as_str) == Some("phase")
            && node.metadata.get("sentinel.node").map(String::as_str) == Some(node.id.as_str())
            && node.metadata.get("sentinel.skill").map(String::as_str) == Some("linear")
            && node.metadata.get("sentinel.phase").map(String::as_str) == Some(node.id.as_str())
            && node
                .metadata
                .get("sentinel.checkpointer_tenant_scope")
                .map(String::as_str)
                == Some("")
    }));

    assert!(
        topology
            .edges
            .iter()
            .any(|edge| edge.from == "claim" && edge.kind == "conditional"),
        "phase routing must be represented as a LangGraph conditional edge"
    );
    for source in [START, "claim", "fetch", "review"] {
        let count = topology
            .edges
            .iter()
            .filter(|edge| edge.from == source && edge.kind == "conditional")
            .count();
        assert_eq!(
            count, 1,
            "{source} must expose exactly one LangGraph conditional edge"
        );
    }
    assert!(topology.edges.iter().all(|edge| edge.kind == "conditional"));
}

#[test]
fn phase_node_metadata_validator_fails_on_missing_or_mismatched_values() {
    fn node(id: &str, key: &str, value: Option<&str>) -> PhaseGraphNodeInfo {
        let mut metadata = BTreeMap::new();
        if let Some(value) = value {
            metadata.insert(key.to_string(), value.to_string());
        }
        PhaseGraphNodeInfo {
            id: id.to_string(),
            deferred: false,
            barrier_on: Vec::new(),
            metadata,
            has_error_handler: true,
            has_timeout_policy: true,
            interrupt_before: false,
            interrupt_after: true,
        }
    }

    let nodes = vec![node(
        "claim",
        "sentinel.checkpointer_backend",
        Some("sqlite"),
    )];
    required_phase_node_metadata(
        "linear",
        &nodes,
        "sentinel.checkpointer_backend",
        "checkpointer backend",
        "sqlite",
    )
    .expect("matching metadata passes");

    let missing = vec![node("claim", "sentinel.checkpointer_scope", None)];
    let err = required_phase_node_metadata(
        "linear",
        &missing,
        "sentinel.checkpointer_scope",
        "checkpointer scope",
        "database_path::memory:",
    )
    .expect_err("missing metadata fails");
    assert!(err.to_string().contains("sentinel.checkpointer_scope"));

    let mismatched = vec![node(
        "claim",
        "sentinel.checkpointer_scope",
        Some("schema:phase_graph"),
    )];
    let err = required_phase_node_metadata(
        "linear",
        &mismatched,
        "sentinel.checkpointer_scope",
        "checkpointer scope",
        "database_path::memory:",
    )
    .expect_err("mismatched metadata fails");
    assert!(err.to_string().contains("schema:phase_graph"));
    assert!(err.to_string().contains("database_path::memory:"));
}

#[test]
fn phase_node_runtime_contract_requires_enterprise_langgraph_configuration() {
    fn node(id: &str) -> PhaseGraphNodeInfo {
        let mut metadata = BTreeMap::new();
        metadata.insert("sentinel.graph".to_string(), "phase".to_string());
        metadata.insert("sentinel.node".to_string(), id.to_string());
        metadata.insert("sentinel.skill".to_string(), "linear".to_string());
        metadata.insert("sentinel.phase".to_string(), id.to_string());
        metadata.insert(
            "sentinel.checkpointer_backend".to_string(),
            "sqlite".to_string(),
        );
        metadata.insert(
            "sentinel.checkpointer_scope".to_string(),
            "database_path::memory:".to_string(),
        );
        metadata.insert(
            "sentinel.checkpointer_tenant_scope".to_string(),
            String::new(),
        );
        PhaseGraphNodeInfo {
            id: id.to_string(),
            deferred: false,
            barrier_on: Vec::new(),
            metadata,
            has_error_handler: true,
            has_timeout_policy: true,
            interrupt_before: false,
            interrupt_after: true,
        }
    }

    let phase_ids = vec!["claim".to_string()];
    let nodes = vec![node("claim")];
    required_phase_node_runtime_contract(
        "linear",
        &phase_ids,
        &nodes,
        "sqlite",
        "database_path::memory:",
        None,
    )
    .expect("fully configured phase node passes");

    let mut missing_timeout = vec![node("claim")];
    missing_timeout[0].has_timeout_policy = false;
    let err = required_phase_node_runtime_contract(
        "linear",
        &phase_ids,
        &missing_timeout,
        "sqlite",
        "database_path::memory:",
        None,
    )
    .expect_err("missing timeout fails");
    assert!(err.to_string().contains("timeout policy"));

    let mut missing_handler = vec![node("claim")];
    missing_handler[0].has_error_handler = false;
    let err = required_phase_node_runtime_contract(
        "linear",
        &phase_ids,
        &missing_handler,
        "sqlite",
        "database_path::memory:",
        None,
    )
    .expect_err("missing error handler fails");
    assert!(err.to_string().contains("error handler"));

    let mut wrong_interrupt = vec![node("claim")];
    wrong_interrupt[0].interrupt_after = false;
    let err = required_phase_node_runtime_contract(
        "linear",
        &phase_ids,
        &wrong_interrupt,
        "sqlite",
        "database_path::memory:",
        None,
    )
    .expect_err("missing post-node interrupt fails");
    assert!(err.to_string().contains("interrupt after"));

    let err = required_phase_node_runtime_contract(
        "linear",
        &phase_ids,
        &[node("claim"), node("unexpected")],
        "sqlite",
        "database_path::memory:",
        None,
    )
    .expect_err("unexpected LangGraph node fails");
    assert!(err.to_string().contains("unexpected LangGraph node"));
}

#[test]
fn phase_node_runtime_contract_enforces_tenant_scope_metadata() {
    fn node(id: &str, tenant_scope: Option<&str>) -> PhaseGraphNodeInfo {
        let mut metadata = BTreeMap::new();
        metadata.insert("sentinel.graph".to_string(), "phase".to_string());
        metadata.insert("sentinel.node".to_string(), id.to_string());
        metadata.insert("sentinel.skill".to_string(), "linear".to_string());
        metadata.insert("sentinel.phase".to_string(), id.to_string());
        metadata.insert(
            "sentinel.checkpointer_backend".to_string(),
            "redis".to_string(),
        );
        metadata.insert(
            "sentinel.checkpointer_scope".to_string(),
            "ttl_seconds:none".to_string(),
        );
        if let Some(tenant_scope) = tenant_scope {
            metadata.insert(
                "sentinel.checkpointer_tenant_scope".to_string(),
                tenant_scope.to_string(),
            );
        }
        PhaseGraphNodeInfo {
            id: id.to_string(),
            deferred: false,
            barrier_on: Vec::new(),
            metadata,
            has_error_handler: true,
            has_timeout_policy: true,
            interrupt_before: false,
            interrupt_after: true,
        }
    }

    let phase_ids = vec!["claim".to_string()];
    let nodes = vec![node("claim", Some("legatus_ai"))];
    required_phase_node_runtime_contract(
        "linear",
        &phase_ids,
        &nodes,
        "redis",
        "ttl_seconds:none",
        Some("legatus_ai"),
    )
    .expect("matching hosted tenant metadata passes");

    let missing = vec![node("claim", None)];
    let err = required_phase_node_runtime_contract(
        "linear",
        &phase_ids,
        &missing,
        "redis",
        "ttl_seconds:none",
        Some("legatus_ai"),
    )
    .expect_err("missing hosted tenant metadata fails");
    assert!(err
        .to_string()
        .contains("sentinel.checkpointer_tenant_scope"));

    let mismatched = vec![node("claim", Some("other_tenant"))];
    let err = required_phase_node_runtime_contract(
        "linear",
        &phase_ids,
        &mismatched,
        "redis",
        "ttl_seconds:none",
        Some("legatus_ai"),
    )
    .expect_err("mismatched hosted tenant metadata fails");
    assert!(err.to_string().contains("other_tenant"));
    assert!(err.to_string().contains("legatus_ai"));
}

#[test]
fn phase_schema_contract_requires_langgraph_authority_schemas() {
    let state = Some(serde_json::json!({
        "type": "object",
        "x-sentinel": {
            "graph": "phase",
            "workflow_skill": "linear",
            "authority": "langgraph"
        }
    }));
    let schema = Some(serde_json::json!({ "type": "object" }));
    required_phase_schema_contract("linear", &state, &schema, &schema)
        .expect("complete phase schema contract passes");

    let err = required_phase_schema_contract("linear", &state, &None, &schema)
        .expect_err("missing input schema must fail");
    assert!(err.to_string().contains("input schema"));

    let wrong_graph = Some(serde_json::json!({
        "type": "object",
        "x-sentinel": {
            "graph": "decision",
            "workflow_skill": "linear",
            "authority": "langgraph"
        }
    }));
    let err = required_phase_schema_contract("linear", &wrong_graph, &schema, &schema)
        .expect_err("wrong graph marker must fail");
    assert!(err.to_string().contains("x-sentinel.graph 'decision'"));

    let missing_authority = Some(serde_json::json!({
        "type": "object",
        "x-sentinel": {
            "graph": "phase",
            "workflow_skill": "linear"
        }
    }));
    let err = required_phase_schema_contract("linear", &missing_authority, &schema, &schema)
        .expect_err("missing authority marker must fail");
    assert!(err.to_string().contains("x-sentinel.authority"));
}

#[test]
fn phase_edge_contract_requires_langgraph_conditional_routing_shape() {
    fn edge(from: &str, kind: &str) -> PhaseGraphEdgeInfo {
        PhaseGraphEdgeInfo {
            from: from.to_string(),
            kind: kind.to_string(),
            to: None,
        }
    }

    let phase_ids = vec!["claim".to_string(), "fetch".to_string()];
    let edges = vec![
        edge(START, "conditional"),
        edge("claim", "conditional"),
        edge("fetch", "conditional"),
    ];
    required_phase_edge_contract("linear", &phase_ids, &edges)
        .expect("complete phase edge contract passes");

    let missing_start = vec![edge("claim", "conditional"), edge("fetch", "conditional")];
    let err = required_phase_edge_contract("linear", &phase_ids, &missing_start)
        .expect_err("missing START routing must fail");
    assert!(err.to_string().contains("missing conditional routing"));
    assert!(err.to_string().contains(START));

    let missing_phase = vec![edge(START, "conditional"), edge("claim", "conditional")];
    let err = required_phase_edge_contract("linear", &phase_ids, &missing_phase)
        .expect_err("missing phase routing must fail");
    assert!(err.to_string().contains("fetch"));

    let duplicate = vec![
        edge(START, "conditional"),
        edge("claim", "conditional"),
        edge("claim", "conditional"),
        edge("fetch", "conditional"),
    ];
    let err = required_phase_edge_contract("linear", &phase_ids, &duplicate)
        .expect_err("duplicate routing must fail");
    assert!(err.to_string().contains("duplicate conditional routing"));

    let non_conditional = vec![
        edge(START, "conditional"),
        edge("claim", "conditional"),
        edge("fetch", "conditional"),
        edge("fetch", "static"),
    ];
    let err = required_phase_edge_contract("linear", &phase_ids, &non_conditional)
        .expect_err("non-conditional phase edge must fail");
    assert!(err.to_string().contains("non-conditional edge"));

    let unexpected = vec![
        edge(START, "conditional"),
        edge("claim", "conditional"),
        edge("fetch", "conditional"),
        edge("ghost", "conditional"),
    ];
    let err = required_phase_edge_contract("linear", &phase_ids, &unexpected)
        .expect_err("unexpected source must fail");
    assert!(err.to_string().contains("unexpected LangGraph edge source"));
}

#[test]
fn phase_runtime_contract_requires_durable_auto_checkpointed_execution() {
    required_phase_runtime_contract("linear", true, true, 100, 3)
        .expect("durable auto-checkpointed phase runtime passes");

    let err = required_phase_runtime_contract("linear", false, true, 100, 3)
        .expect_err("missing checkpointer must fail");
    assert!(err.to_string().contains("durable LangGraph checkpointer"));

    let err = required_phase_runtime_contract("linear", true, false, 100, 3)
        .expect_err("disabled auto-checkpointing must fail");
    assert!(err.to_string().contains("auto-checkpointing"));

    let err = required_phase_runtime_contract("linear", true, true, 3, 3)
        .expect_err("insufficient iteration headroom must fail");
    assert!(err.to_string().contains("max_iterations 3"));
}

#[tokio::test]
async fn empty_workflow_is_rejected() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let empty = SkillWorkflow {
        skill: "broken".to_string(),
        phases: vec![],
        blocked_tool_prefixes: Vec::new(),
        blocked_bash_patterns: Vec::new(),
        bash_allowlist: Vec::new(),
    };
    assert!(compile_skill_graph_with_checkpointer(&empty, saver).is_err());
}

#[tokio::test]
async fn graph_schema_rejects_state_for_different_workflow_contract() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    let mut state = PhaseGraphState::new(
        "linear",
        "schema-contract",
        vec!["claim".into(), "review".into()],
    );
    state.current_phase = Some(0);

    let err = graph
        .update_state_as_node("schema-contract", state, "claim")
        .await
        .expect_err("invalid graph state must fail schema validation");

    assert!(
        err.to_string()
            .contains("phase_order must match the compiled workflow exactly"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn graph_state_round_trips_through_workflow_state() {
    let mut s = PhaseGraphState::new("linear", "sess-1", vec!["claim".into(), "fetch".into()]);
    s.current_phase = Some(1);
    s.completed_phases = vec!["claim".into()];
    s.step_states.push(sentinel_domain::workflow::StepState {
        step_id: "0.1".into(),
        phase_id: "claim".into(),
        status: StepStatus::Completed,
        started_at: None,
        completed_at: None,
        summary: Some("claimed".into()),
    });
    s.current_step = Some("0.1".into());
    s.last_verdict = Verdict::Pass;

    let ws = s.to_workflow_state();
    assert_eq!(ws.current_phase, Some(1));
    assert_eq!(ws.completed_phases, vec!["claim".to_string()]);
    assert_eq!(ws.dyad_verdicts, s.dyad_verdicts);
    assert_eq!(ws.step_states.len(), 1);
    assert_eq!(ws.step_states[0].step_id, "0.1");
    assert_eq!(ws.step_states[0].phase_id, "claim");
    assert_eq!(ws.step_states[0].status, StepStatus::Completed);
    assert_eq!(ws.step_states[0].summary.as_deref(), Some("claimed"));
    assert_eq!(ws.current_step, s.current_step);

    let back = PhaseGraphState::from_workflow_state(&ws, vec!["claim".into(), "fetch".into()]);
    assert_eq!(back.current_phase, Some(1));
    assert_eq!(back.completed_phases, s.completed_phases);
    assert_eq!(back.dyad_verdicts, s.dyad_verdicts);
    assert_eq!(back.step_states.len(), 1);
    assert_eq!(back.step_states[0].step_id, "0.1");
    assert_eq!(back.step_states[0].phase_id, "claim");
    assert_eq!(back.step_states[0].status, StepStatus::Completed);
    assert_eq!(back.step_states[0].summary.as_deref(), Some("claimed"));
    assert_eq!(back.current_step, s.current_step);
    // Verdict is graph-transient — not carried on the domain type.
    assert_eq!(back.last_verdict, Verdict::Pending);
}

#[tokio::test]
async fn graph_schema_rejects_phase_error_event_for_unknown_phase() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    let mut state = PhaseGraphState::new(
        "linear",
        "schema-error-event",
        vec!["claim".into(), "fetch".into(), "review".into()],
    );
    state.phase_error_events.push(PhaseGraphErrorEvent {
        phase_id: "unknown".into(),
        node_id: "claim".into(),
        error: "boom".into(),
    });

    let err = graph
        .update_state_as_node("schema-error-event", state, "claim")
        .await
        .expect_err("invalid phase error event must fail schema validation");

    assert!(
        err.to_string()
            .contains("phase error event phase 'unknown' is not in the compiled workflow"),
        "unexpected error: {err}"
    );
}

#[test]
fn pass_advances_to_next_phase() {
    let order = vec![
        "claim".to_string(),
        "fetch".to_string(),
        "review".to_string(),
    ];
    assert_eq!(next_phase_target(Verdict::Pass, 0, &order), "fetch");
    assert_eq!(next_phase_target(Verdict::Pass, 1, &order), "review");
}

#[test]
fn pass_on_last_phase_routes_to_end() {
    let order = vec![
        "claim".to_string(),
        "fetch".to_string(),
        "review".to_string(),
    ];
    assert_eq!(next_phase_target(Verdict::Pass, 2, &order), "__end__");
}

#[test]
fn fail_loops_back_to_same_phase() {
    let order = vec![
        "claim".to_string(),
        "fetch".to_string(),
        "review".to_string(),
    ];
    assert_eq!(next_phase_target(Verdict::Fail, 1, &order), "fetch");
}

#[test]
fn pending_stays_on_same_phase() {
    let order = vec!["claim".to_string(), "fetch".to_string()];
    assert_eq!(next_phase_target(Verdict::Pending, 0, &order), "claim");
}

/// THE durability proof: a checkpoint written by one compiled graph is read
/// back by a freshly-compiled graph sharing the same sqlite file — exactly
/// the cross-process-invocation contract sentinel's hook model needs.
#[tokio::test]
async fn fresh_process_restores_checkpoint() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let db = tmp.path().join("phases.db");
    let db_path = db.to_str().expect("utf8 path");

    let session = "sess-durable";
    let phase_order = vec![
        "claim".to_string(),
        "fetch".to_string(),
        "review".to_string(),
    ];

    // --- "process 1": compile, advance to phase 1, checkpoint ---
    {
        let saver = phase_checkpointer(db_path).await.expect("saver-1");
        let graph =
            compile_skill_graph_with_checkpointer(&fixture(), saver.clone()).expect("compile-1");

        let mut state = PhaseGraphState::new("linear", session, phase_order.clone());
        state.current_phase = Some(1);
        state.completed_phases = vec!["claim".to_string()];

        graph
            .update_state_as_node(session, state, "claim")
            .await
            .expect("persist checkpoint");
    }

    // --- "process 2": fresh compile + saver on the SAME db, load_latest ---
    {
        let saver = phase_checkpointer(db_path).await.expect("saver-2");
        let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile-2");

        let restored = graph
            .load_latest(session)
            .await
            .expect("load_latest ok")
            .expect("checkpoint present");

        assert_eq!(restored.current_phase, Some(1));
        assert_eq!(restored.completed_phases, vec!["claim".to_string()]);
    }
}

#[tokio::test]
async fn apply_verdict_pass_advances_and_persists() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    seed_gate(&graph, "linear", "sess-pass").await;
    let s = graph
        .apply_verdict("linear", "sess-pass", "claim", true)
        .await
        .expect("apply pass");
    assert_eq!(s.completed_phases, vec!["claim".to_string()]);
    assert_eq!(s.current_phase, Some(1)); // advanced to fetch
    assert!(!s.complete);

    // Re-load proves it persisted as a checkpoint.
    let reloaded = graph
        .load_latest("sess-pass")
        .await
        .expect("load")
        .expect("present");
    assert_eq!(reloaded.completed_phases, vec!["claim".to_string()]);
    assert_eq!(reloaded.current_phase, Some(1));
}

#[tokio::test]
async fn run_until_gate_executes_phase_node_and_persists_interrupt_state() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    let gated = graph
        .run_until_gate("linear", "sess-gate")
        .await
        .expect("run to gate");

    assert_eq!(gated.current_phase, Some(0));
    assert_eq!(gated.last_verdict, Verdict::Pending);

    let persisted = graph
        .load_latest("sess-gate")
        .await
        .expect("load")
        .expect("checkpoint");
    assert_eq!(persisted.current_phase, Some(0));
    assert_eq!(persisted.completed_phases, Vec::<String>::new());
}

#[tokio::test]
async fn run_until_gate_rejects_skill_mismatch_before_checkpoint_seed() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    let err = graph
        .run_until_gate("deploy", "skill-mismatch-session")
        .await
        .expect_err("compiled graph skill must be authoritative");

    assert!(err.to_string().contains("compiled for skill 'linear'"));
    assert!(
        graph
            .load_latest("skill-mismatch-session")
            .await
            .expect("load_latest should still work")
            .is_none(),
        "skill mismatch must not seed a LangGraph checkpoint"
    );
}

#[tokio::test]
async fn run_until_gate_report_captures_langgraph_stream_parts() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    let report = graph
        .run_until_gate_report("linear", "sess-stream-report")
        .await
        .expect("run to gate report");

    assert_eq!(report.state.current_phase, Some(0));
    assert!(
        report
            .stream
            .iter()
            .all(|part| part.stream_protocol == "v3"),
        "stream report must expose the LangGraph v3 typed protocol"
    );
    assert!(
        report
            .stream
            .iter()
            .any(|part| part.payload_kind == "values"),
        "stream report must include LangGraph values payloads"
    );
    assert!(
        report
            .stream
            .iter()
            .any(|part| part.payload_kind == "updates"),
        "stream report must include LangGraph update payloads"
    );
    assert!(
        report
            .stream
            .iter()
            .any(|part| part.payload_kind == "tasks"),
        "stream report must include LangGraph task payloads"
    );
    assert!(
        report
            .stream
            .iter()
            .any(|part| part.payload_kind == "debug"),
        "stream report must include LangGraph debug payloads"
    );
    assert!(
        report.stream.iter().any(|part| {
            part.payload_kind == "checkpoints"
                && part
                    .payload_json
                    .pointer("/source/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("stream_update")
                && part
                    .payload_json
                    .pointer("/source/node")
                    .and_then(serde_json::Value::as_str)
                    == Some("claim")
        }),
        "stream report must include the persisted claim gate checkpoint"
    );
    assert!(
        report.stream.iter().any(|part| {
            part.payload_kind == "custom"
                && part.payload_json["type"] == "sentinel.phase_gate"
                && part.payload_json["phase_id"] == "claim"
                && part.payload_json["phase_index"] == serde_json::json!(0)
        }),
        "stream report must include Sentinel custom phase gate payload"
    );
    assert!(
        report.stream.iter().all(|part| !part.node_id.is_empty()),
        "every stream part must preserve its LangGraph node id"
    );
    assert!(
        report
            .stream
            .iter()
            .all(|part| part.subgraph_namespace.is_empty()),
        "top-level phase runs must preserve an empty v3 subgraph namespace"
    );

    let writes = graph
        .phase_writes_history("sess-stream-report", Some("state"))
        .await
        .expect("state writes");
    assert!(
        writes.iter().any(|entry| entry.node_id == "claim"),
        "stream-created checkpoint writes must be visible in LangGraph write history"
    );
    let snapshots = graph
        .phase_snapshots("sess-stream-report")
        .await
        .expect("snapshots");
    let latest = snapshots.last().expect("latest snapshot");
    let latest_stream_checkpoint = report
        .stream
        .iter()
        .find(|part| {
            part.payload_kind == "checkpoints"
                && part
                    .payload_json
                    .get("checkpoint_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(latest.checkpoint_id.as_str())
        })
        .expect("stream exposes latest checkpoint");
    assert_eq!(
        latest_stream_checkpoint.payload_json["thread_id"],
        latest.thread_id
    );
    assert_eq!(
        latest_stream_checkpoint.payload_json["step_number"],
        serde_json::json!(latest.step_number)
    );
    assert_eq!(
        latest_stream_checkpoint.payload_json["state"],
        serde_json::to_value(&report.state).expect("state serializes")
    );
}

#[tokio::test]
async fn phase_v3_message_drain_records_all_chat_model_subchannels() {
    let (stream, mut senders) = ChatModelStream::channels("phase_llm");
    senders.dispatch_text_delta("phase text").await;
    senders.dispatch_reasoning_delta("phase reasoning").await;
    senders
        .dispatch_tool_call(ToolCall::new(
            "call_1",
            "phase_lookup",
            HashMap::from([("phase".to_string(), serde_json::json!("claim"))]),
        ))
        .await;
    senders.dispatch_usage(TokenUsage::new(3, 5, 8)).await;
    senders.finalize(Message::assistant("phase output"));
    drop(senders);

    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(stream).await.expect("send chat stream");
    drop(tx);

    let drain = crate::CompiledPhaseGraph::drain_v3_messages(rx)
        .await
        .expect("message drain");
    let events: Vec<_> = drain
        .stream
        .iter()
        .map(|part| part.event_type.as_str())
        .collect();
    for expected in [
        "MessageStreamStarted",
        "MessageTextDelta",
        "MessageReasoningDelta",
        "MessageToolCall",
        "MessageUsage",
        "MessageOutput",
    ] {
        assert!(
            events.contains(&expected),
            "missing {expected} in {events:?}"
        );
    }
    assert!(drain.stream.iter().any(|part| {
        part.event_type == "MessageTextDelta" && part.payload_json["delta"] == "phase text"
    }));
    assert!(drain.stream.iter().any(|part| {
        part.event_type == "MessageReasoningDelta"
            && part.payload_json["delta"] == "phase reasoning"
    }));
    assert!(drain.stream.iter().any(|part| {
        part.event_type == "MessageToolCall"
            && part.payload_json["tool_call"]["name"] == "phase_lookup"
    }));
    assert!(drain.stream.iter().any(|part| {
        part.event_type == "MessageUsage" && part.payload_json["usage"]["total_tokens"] == 8
    }));
    assert!(drain.stream.iter().any(|part| {
        part.event_type == "MessageOutput"
            && part.payload_json["message"]["content"] == "phase output"
    }));
}

#[test]
fn phase_v3_projection_preserves_task_error_detail() {
    let mut drain = V3PhaseProjectionDrain::default();

    drain.push(PhaseGraphStreamPart {
        stream_protocol: "v3".into(),
        event_type: "Task".into(),
        node_id: "claim".into(),
        timestamp: "2026-06-18T00:00:00Z".into(),
        superstep: 1,
        payload_kind: "tasks".into(),
        payload_json: serde_json::json!({
            "status": "error",
            "error": "schema rejected missing current_phase"
        }),
        subgraph_namespace: Vec::new(),
    });

    assert_eq!(
        drain.node_error.as_deref(),
        Some("phase stream failed at node claim: schema rejected missing current_phase")
    );
}

#[tokio::test]
async fn apply_verdict_reenters_next_phase_through_langgraph_runtime() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    graph
        .run_until_gate("linear", "sess-next")
        .await
        .expect("initial gate");
    let s = graph
        .apply_verdict("linear", "sess-next", "claim", true)
        .await
        .expect("apply pass");

    assert_eq!(s.completed_phases, vec!["claim".to_string()]);
    assert_eq!(s.current_phase, Some(1));
    assert_eq!(s.last_verdict, Verdict::Pending);

    let history = graph.phase_history("sess-next").await.expect("history");
    assert!(
        history.iter().any(|h| h.current_phase == Some(1)),
        "expected graph execution to persist a fetch-phase checkpoint"
    );
}

#[tokio::test]
async fn apply_verdict_report_returns_reentry_stream_for_next_gate() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    graph
        .run_until_gate("linear", "sess-verdict-report")
        .await
        .expect("initial gate");
    let report = graph
        .apply_verdict_report("linear", "sess-verdict-report", "claim", true)
        .await
        .expect("apply pass report");

    assert_eq!(report.state.current_phase, Some(1));
    assert_eq!(report.state.completed_phases, vec!["claim".to_string()]);
    assert!(
        report
            .stream
            .iter()
            .all(|part| part.stream_protocol == "v3"),
        "non-terminal verdict report must expose LangGraph v3 stream evidence"
    );
    assert!(
        report
            .stream
            .iter()
            .any(|part| part.payload_kind == "values"),
        "non-terminal verdict report must expose LangGraph values stream"
    );
    assert!(
        report
            .stream
            .iter()
            .any(|part| part.payload_kind == "updates"),
        "non-terminal verdict report must expose LangGraph v3 updates stream"
    );
    assert!(
        report
            .stream
            .iter()
            .any(|part| part.payload_kind == "tasks"),
        "non-terminal verdict report must expose LangGraph v3 tasks stream"
    );
    assert!(
        report.stream.iter().any(|part| {
            part.payload_kind == "checkpoints"
                && part
                    .payload_json
                    .pointer("/source/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("stream_update")
                && part
                    .payload_json
                    .pointer("/source/node")
                    .and_then(serde_json::Value::as_str)
                    == Some("fetch")
        }),
        "non-terminal verdict report must expose next gate checkpoint stream"
    );
    assert!(
        report.stream.iter().any(|part| {
            part.payload_kind == "custom"
                && part.payload_json["type"] == "sentinel.phase_gate"
                && part.payload_json["phase_id"] == "fetch"
                && part.payload_json["phase_index"] == serde_json::json!(1)
        }),
        "non-terminal verdict report must expose next gate custom stream"
    );
}

#[tokio::test]
async fn graph_threads_are_isolated_by_langgraph_config_thread_id() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    graph
        .run_until_gate("linear", "thread-a")
        .await
        .expect("gate a");
    graph
        .apply_verdict("linear", "thread-a", "claim", true)
        .await
        .expect("advance a");

    let b = graph
        .run_until_gate("linear", "thread-b")
        .await
        .expect("gate b");

    assert_eq!(b.current_phase, Some(0));
    assert!(b.completed_phases.is_empty());

    let a = graph
        .load_latest("thread-a")
        .await
        .expect("load a")
        .expect("a checkpoint");
    assert_eq!(a.current_phase, Some(1));
    assert_eq!(a.completed_phases, vec!["claim".to_string()]);
}

#[tokio::test]
async fn same_session_different_skills_use_distinct_langgraph_threads() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let linear = compile_skill_graph_with_checkpointer(&fixture_for("linear"), saver.clone())
        .expect("linear");
    let deploy =
        compile_skill_graph_with_checkpointer(&fixture_for("deploy"), saver).expect("deploy");

    seed_gate(&linear, "linear", "shared-session").await;
    linear
        .apply_verdict("linear", "shared-session", "claim", true)
        .await
        .expect("linear claim");

    assert!(
        deploy
            .load_latest("shared-session")
            .await
            .expect("deploy load")
            .is_none(),
        "deploy must not see linear's checkpoint"
    );

    deploy
        .run_until_gate("deploy", "shared-session")
        .await
        .expect("deploy gate");

    let linear_latest = linear
        .load_latest("shared-session")
        .await
        .expect("linear load")
        .expect("linear checkpoint");
    assert_eq!(linear_latest.skill, "linear");
    assert_eq!(linear_latest.completed_phases, vec!["claim".to_string()]);

    let deploy_latest = deploy
        .load_latest("shared-session")
        .await
        .expect("deploy load")
        .expect("deploy checkpoint");
    assert_eq!(deploy_latest.skill, "deploy");
    assert!(deploy_latest.completed_phases.is_empty());
}

#[tokio::test]
async fn apply_verdict_fail_keeps_phase() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    seed_gate(&graph, "linear", "sess-fail").await;
    let s = graph
        .apply_verdict("linear", "sess-fail", "claim", false)
        .await
        .expect("apply fail");
    assert!(s.completed_phases.is_empty());
    assert_eq!(s.current_phase, Some(0)); // stayed on claim
    assert!(!s.complete);
}

#[tokio::test]
async fn apply_verdict_rejects_out_of_order_required_phase() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    seed_gate(&graph, "linear", "sess-order").await;
    let err = graph
        .apply_verdict("linear", "sess-order", "fetch", true)
        .await
        .expect_err("fetch cannot pass before claim");

    assert!(matches!(
        err,
        crate::GraphEngineError::PhaseOrderViolation {
            ref phase,
            ref missing,
            ..
        } if phase == "fetch" && missing == "claim"
    ));
}

#[tokio::test]
async fn apply_verdict_without_checkpoint_fails_closed() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    let mut stale = WorkflowState::new("linear", "stale-no-checkpoint");
    stale.current_phase = Some(1);
    stale.completed_phases = vec!["claim".to_string()];

    let err = graph
        .apply_verdict("linear", "stale-no-checkpoint", "fetch", true)
        .await
        .expect_err("phase verdicts must require durable checkpoint history");

    assert!(matches!(
        err,
        crate::GraphEngineError::MissingCheckpoint {
            ref skill,
            ref session_id,
            ref phase,
        } if skill == "linear" && session_id == "stale-no-checkpoint" && phase == "fetch"
    ));
    assert!(
        graph
            .load_latest("stale-no-checkpoint")
            .await
            .expect("load")
            .is_none(),
        "rejected verdict must not create a checkpoint"
    );
}

#[tokio::test]
async fn apply_verdict_rejects_completed_phase_without_new_checkpoint() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    seed_gate(&graph, "linear", "duplicate-session").await;
    graph
        .apply_verdict("linear", "duplicate-session", "claim", true)
        .await
        .expect("initial claim completion");
    let checkpoint_count = graph
        .phase_snapshots("duplicate-session")
        .await
        .expect("snapshots")
        .len();

    for passed in [true, false] {
        let err = graph
            .apply_verdict("linear", "duplicate-session", "claim", passed)
            .await
            .expect_err("completed phase verdicts must be replayed first");
        assert!(matches!(
            err,
            crate::GraphEngineError::PhaseAlreadyCompleted {
                ref skill,
                ref session_id,
                ref phase,
            } if skill == "linear" && session_id == "duplicate-session" && phase == "claim"
        ));
    }

    assert_eq!(
        graph
            .phase_snapshots("duplicate-session")
            .await
            .expect("snapshots")
            .len(),
        checkpoint_count,
        "duplicate verdicts must not append graph checkpoints"
    );
}

#[tokio::test]
async fn apply_verdict_completes_when_required_phases_pass_even_with_optional_tail() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let mut workflow = fixture();
    workflow.phases.insert(
        1,
        WorkflowPhase {
            required: false,
            ..phase("audit")
        },
    );
    let graph = compile_skill_graph_with_checkpointer(&workflow, saver).expect("compile");
    seed_gate(&graph, "linear", "sess-optional").await;

    let after_claim = graph
        .apply_verdict("linear", "sess-optional", "claim", true)
        .await
        .expect("claim passes");
    assert_eq!(
        after_claim.current_phase,
        Some(2),
        "optional audit is skipped"
    );

    let after_fetch = graph
        .apply_verdict("linear", "sess-optional", "fetch", true)
        .await
        .expect("fetch passes");
    assert_eq!(after_fetch.current_phase, Some(3));

    let completed = graph
        .apply_verdict("linear", "sess-optional", "review", true)
        .await
        .expect("review passes");
    assert!(completed.complete, "all required phases are complete");
    assert_eq!(
        completed.completed_phases,
        vec![
            "claim".to_string(),
            "fetch".to_string(),
            "review".to_string()
        ]
    );
}

#[tokio::test]
async fn apply_verdict_enforces_role_dyad_from_checkpoint() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let mut workflow = fixture();
    workflow.phases[0].required_dyad = Some(RoleDyad {
        reviewer: true,
        tester: false,
    });
    let graph = compile_skill_graph_with_checkpointer(&workflow, saver).expect("compile");
    seed_gate(&graph, "linear", "sess-dyad").await;

    let err = graph
        .apply_verdict("linear", "sess-dyad", "claim", true)
        .await
        .expect_err("missing reviewer must block");
    assert!(matches!(
        err,
        crate::GraphEngineError::DyadUnsatisfied { .. }
    ));

    let implementer = DyadVerdicts {
        implementer: Some("builder".to_string()),
        reviewer_pass_by: None,
        tester_pass_by: None,
    };
    graph
        .run_until_gate("linear", "sess-dyad-ok")
        .await
        .expect("initial gate checkpoint");
    graph
        .update_dyad_verdicts("linear", "sess-dyad-ok", "claim", implementer)
        .await
        .expect("implementer checkpoint");
    let reviewer = DyadVerdicts {
        implementer: None,
        reviewer_pass_by: Some("reviewer".to_string()),
        tester_pass_by: None,
    };
    let dyad_checkpoint = graph
        .update_dyad_verdicts("linear", "sess-dyad-ok", "claim", reviewer)
        .await
        .expect("dyad checkpoint");
    let dyad = dyad_checkpoint
        .dyad_verdicts
        .get("claim")
        .expect("merged dyad verdict");
    assert_eq!(dyad.implementer.as_deref(), Some("builder"));
    assert_eq!(dyad.reviewer_pass_by.as_deref(), Some("reviewer"));
    let passed = graph
        .apply_verdict("linear", "sess-dyad-ok", "claim", true)
        .await
        .expect("checkpointed separate reviewer satisfies dyad");
    assert_eq!(passed.completed_phases, vec!["claim".to_string()]);
}

#[tokio::test]
async fn update_dyad_without_checkpoint_fails_closed() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    let err = graph
        .update_dyad_verdicts(
            "linear",
            "missing-dyad-checkpoint",
            "claim",
            DyadVerdicts {
                implementer: Some("builder".to_string()),
                reviewer_pass_by: None,
                tester_pass_by: None,
            },
        )
        .await
        .expect_err("dyad updates must require durable checkpoint history");

    assert!(matches!(
        err,
        crate::GraphEngineError::MissingCheckpoint {
            skill,
            session_id,
            phase,
        } if skill == "linear" && session_id == "missing-dyad-checkpoint" && phase == "claim"
    ));
    assert!(graph
        .load_latest("missing-dyad-checkpoint")
        .await
        .expect("load")
        .is_none());
}

#[tokio::test]
async fn update_dyad_rejects_completed_phase_without_new_checkpoint() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let mut workflow = fixture();
    workflow.phases[0].required_dyad = Some(RoleDyad {
        reviewer: true,
        tester: false,
    });
    let graph = compile_skill_graph_with_checkpointer(&workflow, saver).expect("compile");

    graph
        .run_until_gate("linear", "sealed-dyad-session")
        .await
        .expect("initial gate checkpoint");
    graph
        .update_dyad_verdicts(
            "linear",
            "sealed-dyad-session",
            "claim",
            DyadVerdicts {
                implementer: Some("builder".to_string()),
                reviewer_pass_by: None,
                tester_pass_by: None,
            },
        )
        .await
        .expect("implementer checkpoint");
    graph
        .update_dyad_verdicts(
            "linear",
            "sealed-dyad-session",
            "claim",
            DyadVerdicts {
                implementer: None,
                reviewer_pass_by: Some("reviewer".to_string()),
                tester_pass_by: None,
            },
        )
        .await
        .expect("reviewer checkpoint");
    graph
        .apply_verdict("linear", "sealed-dyad-session", "claim", true)
        .await
        .expect("phase completion");
    let checkpoint_count = graph
        .phase_snapshots("sealed-dyad-session")
        .await
        .expect("snapshots")
        .len();

    let err = graph
        .update_dyad_verdicts(
            "linear",
            "sealed-dyad-session",
            "claim",
            DyadVerdicts {
                implementer: None,
                reviewer_pass_by: Some("late-reviewer".to_string()),
                tester_pass_by: None,
            },
        )
        .await
        .expect_err("completed phase dyad edits must be replayed first");
    assert!(matches!(
        err,
        crate::GraphEngineError::PhaseAlreadyCompleted {
            ref skill,
            ref session_id,
            ref phase,
        } if skill == "linear" && session_id == "sealed-dyad-session" && phase == "claim"
    ));
    assert_eq!(
        graph
            .phase_snapshots("sealed-dyad-session")
            .await
            .expect("snapshots")
            .len(),
        checkpoint_count,
        "rejected dyad rewrite must not append graph checkpoints"
    );
}

#[tokio::test]
async fn apply_verdict_without_checkpoint_rejects_cached_dyad_verdicts() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let mut workflow = fixture();
    workflow.phases[0].required_dyad = Some(RoleDyad {
        reviewer: true,
        tester: false,
    });
    let graph = compile_skill_graph_with_checkpointer(&workflow, saver).expect("compile");

    let mut stale = WorkflowState::new("linear", "stale-dyad-no-checkpoint");
    stale.record_implementer("claim", "builder");
    stale.record_reviewer_pass("claim", "reviewer");
    let err = graph
        .apply_verdict("linear", "stale-dyad-no-checkpoint", "claim", true)
        .await
        .expect_err("phase verdicts must require durable checkpoint history");

    assert!(matches!(
        err,
        crate::GraphEngineError::MissingCheckpoint {
            skill,
            session_id,
            phase,
        } if skill == "linear" && session_id == "stale-dyad-no-checkpoint" && phase == "claim"
    ));
    assert!(
        graph
            .load_latest("stale-dyad-no-checkpoint")
            .await
            .expect("load")
            .is_none(),
        "rejected cached dyad must not create a checkpoint"
    );
}

#[tokio::test]
async fn apply_verdict_pass_on_last_phase_completes() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    seed_gate(&graph, "linear", "s").await;

    graph
        .apply_verdict("linear", "s", "claim", true)
        .await
        .expect("p1");
    graph
        .apply_verdict("linear", "s", "fetch", true)
        .await
        .expect("p2");
    let s = graph
        .apply_verdict("linear", "s", "review", true)
        .await
        .expect("p3");

    assert_eq!(
        s.completed_phases,
        vec![
            "claim".to_string(),
            "fetch".to_string(),
            "review".to_string()
        ],
    );
    assert!(s.complete);
}

#[tokio::test]
async fn apply_verdict_report_terminal_completion_has_no_next_gate_stream() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    seed_gate(&graph, "linear", "terminal-report").await;

    graph
        .apply_verdict("linear", "terminal-report", "claim", true)
        .await
        .expect("claim");
    graph
        .apply_verdict("linear", "terminal-report", "fetch", true)
        .await
        .expect("fetch");
    let report = graph
        .apply_verdict_report("linear", "terminal-report", "review", true)
        .await
        .expect("terminal report");

    assert!(report.state.complete);
    assert!(
        report.stream.is_empty(),
        "terminal completion has no next phase gate to stream"
    );
}

#[tokio::test]
async fn apply_verdict_unknown_phase_errors() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    assert!(graph
        .apply_verdict("linear", "s", "nonexistent", true)
        .await
        .is_err());
}

#[tokio::test]
async fn phase_history_accumulates_checkpoints() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    seed_gate(&graph, "linear", "h").await;

    graph
        .apply_verdict("linear", "h", "claim", true)
        .await
        .expect("p1");
    graph
        .apply_verdict("linear", "h", "fetch", true)
        .await
        .expect("p2");

    let history = graph.phase_history("h").await.expect("history");
    // At least one checkpoint per verdict; latest reflects 2 completed phases.
    assert!(
        history.len() >= 2,
        "expected >=2 checkpoints, got {}",
        history.len()
    );
    let latest = history.last().expect("non-empty");
    assert_eq!(
        latest.completed_phases,
        vec!["claim".to_string(), "fetch".to_string()]
    );
}

#[tokio::test]
async fn phase_snapshots_preserve_checkpoint_metadata() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    seed_gate(&graph, "linear", "meta-history").await;

    graph
        .apply_verdict("linear", "meta-history", "claim", true)
        .await
        .expect("claim verdict");

    let snapshots = graph
        .phase_snapshots("meta-history")
        .await
        .expect("snapshots");
    assert!(
        snapshots.len() >= 2,
        "expected interrupt + verdict checkpoints, got {}",
        snapshots.len()
    );

    for window in snapshots.windows(2) {
        assert!(window[0].step_number <= window[1].step_number);
    }
    assert!(snapshots
        .iter()
        .all(|snapshot| !snapshot.checkpoint_id.is_empty()));
    assert!(snapshots
        .iter()
        .all(|snapshot| !snapshot.thread_id.is_empty()));
    assert!(snapshots
        .iter()
        .all(|snapshot| !snapshot.created_at.is_empty()));
    assert!(
        snapshots
            .iter()
            .skip(1)
            .any(|snapshot| snapshot.parent_checkpoint_id.is_some()),
        "non-root checkpoints should preserve parent lineage"
    );
    assert!(
        snapshots.iter().any(|snapshot| !snapshot.writes.is_empty()),
        "runtime checkpoints should expose recorded writes"
    );
    let latest = snapshots.last().expect("latest snapshot");
    assert_eq!(latest.state.completed_phases, vec!["claim".to_string()]);
}

#[tokio::test]
async fn phase_writes_history_streams_checkpoint_channel_values() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    seed_gate(&graph, "linear", "write-history").await;

    graph
        .apply_verdict("linear", "write-history", "claim", true)
        .await
        .expect("claim verdict");

    let writes = graph
        .phase_writes_history("write-history", None)
        .await
        .expect("writes");
    assert!(
        writes.iter().any(|write| write.channel == "state"),
        "state channel writes should be exposed"
    );
    assert!(
        writes
            .iter()
            .any(|write| write.channel == "parallel_runtime"),
        "update_state_as_node writes should expose parallel runtime channel"
    );
    assert!(writes
        .iter()
        .all(|write| !write.checkpoint_id.is_empty() && !write.ts.is_empty()));
    assert!(writes
        .iter()
        .all(|write| write.value_len > 0 && write.value_sha256.len() == 64));

    let completed_state_write =
        writes
            .iter()
            .filter(|write| write.channel == "state")
            .find(|write| {
                write
                    .value_json
                    .get("completed_phases")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|phases| phases.iter().any(|phase| phase == "claim"))
            });
    assert!(
        completed_state_write.is_some(),
        "write history should decode state channel JSON"
    );

    let state_only = graph
        .phase_writes_history("write-history", Some("state"))
        .await
        .expect("state writes");
    assert!(state_only.iter().all(|write| write.channel == "state"));
}

#[tokio::test]
async fn phase_writes_history_errors_without_checkpoint_writes() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    let err = graph
        .phase_writes_history("no-history", None)
        .await
        .expect_err("write history must require durable checkpoint writes");
    assert!(
        err.to_string().contains("write history is empty"),
        "unexpected error: {err}"
    );
}

fn evidence_state(skill: &str, session_id: &str, completed: &[&str]) -> PhaseGraphState {
    let mut state = PhaseGraphState::new(
        skill,
        session_id,
        vec![
            "claim".to_string(),
            "fetch".to_string(),
            "review".to_string(),
        ],
    );
    state.completed_phases = completed.iter().map(|phase| (*phase).to_string()).collect();
    state.complete = completed.len() == 3;
    state
}

fn evidence_checkpoint(
    skill: &str,
    session_id: &str,
    checkpoint_id: &str,
    step_number: u64,
    state: PhaseGraphState,
    node_id: &str,
) -> PhaseGraphCheckpointSnapshot {
    PhaseGraphCheckpointSnapshot {
        checkpoint_id: checkpoint_id.to_string(),
        parent_checkpoint_id: None,
        thread_id: format!("sentinel.phase.{skill}.{session_id}"),
        step_number,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        tags: BTreeMap::new(),
        source: None,
        writes: vec![PhaseGraphCheckpointWrite {
            node_id: node_id.to_string(),
            channel: "state".to_string(),
            ts: "2026-01-01T00:00:00Z".to_string(),
        }],
        state,
    }
}

fn evidence_checkpoint_stream_part(
    snapshot: &PhaseGraphCheckpointSnapshot,
    node_id: &str,
    state: &PhaseGraphState,
) -> PhaseGraphStreamPart {
    PhaseGraphStreamPart {
        stream_protocol: "v3".to_string(),
        event_type: "Checkpoint".to_string(),
        node_id: node_id.to_string(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        superstep: snapshot.step_number,
        payload_kind: "checkpoints".to_string(),
        payload_json: serde_json::json!({
            "checkpoint_id": snapshot.checkpoint_id,
            "parent_checkpoint_id": snapshot.parent_checkpoint_id,
            "thread_id": snapshot.thread_id,
            "step_number": snapshot.step_number,
            "source": {
                "type": "stream_update",
                "node": node_id,
            },
            "state": serde_json::to_value(state).expect("state serializes"),
        }),
        subgraph_namespace: Vec::new(),
    }
}

fn evidence_write(
    checkpoint_id: &str,
    step_number: u64,
    node_id: &str,
    value_json: serde_json::Value,
) -> PhaseGraphWriteHistoryEntry {
    let skill = value_json
        .get("skill")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("linear");
    let session_id = value_json
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("session");
    PhaseGraphWriteHistoryEntry {
        thread_id: format!("sentinel.phase.{skill}.{session_id}"),
        checkpoint_id: checkpoint_id.to_string(),
        step_number,
        channel: "state".to_string(),
        node_id: node_id.to_string(),
        ts: "2026-01-01T00:00:00Z".to_string(),
        value_len: 1,
        value_sha256: "0".repeat(64),
        value_json,
    }
}

#[test]
fn checkpoint_write_evidence_accepts_matching_latest_state() {
    let skill = "linear";
    let session_id = "evidence-valid";
    let older = evidence_state(skill, session_id, &[]);
    let latest = evidence_state(skill, session_id, &["claim"]);
    let snapshots = vec![
        evidence_checkpoint(skill, session_id, "checkpoint-1", 1, older, "claim"),
        evidence_checkpoint(
            skill,
            session_id,
            "checkpoint-2",
            2,
            latest.clone(),
            "claim",
        ),
    ];
    let writes = vec![
        evidence_write(
            "checkpoint-1",
            1,
            "claim",
            serde_json::to_value(&snapshots[0].state).expect("json"),
        ),
        evidence_write(
            "checkpoint-2",
            2,
            "claim",
            serde_json::to_value(&latest).expect("json"),
        ),
    ];

    crate::CompiledPhaseGraph::validate_checkpoint_write_evidence(
        skill,
        &format!("sentinel.phase.{skill}.{session_id}"),
        &latest,
        &snapshots,
        &writes,
    )
    .expect("matching latest checkpoint and state write must pass");
}

#[test]
fn stream_latest_checkpoint_evidence_accepts_matching_payload() {
    let skill = "linear";
    let session_id = "stream-evidence-valid";
    let state = evidence_state(skill, session_id, &["claim"]);
    let snapshot =
        evidence_checkpoint(skill, session_id, "checkpoint-2", 2, state.clone(), "claim");
    let stream = vec![evidence_checkpoint_stream_part(&snapshot, "claim", &state)];

    crate::CompiledPhaseGraph::validate_stream_latest_checkpoint_evidence(
        &stream,
        &snapshot,
        &format!("sentinel.phase.{skill}.{session_id}"),
        "claim",
        &state,
    )
    .expect("matching latest stream checkpoint evidence must pass");
}

#[test]
fn stream_latest_checkpoint_evidence_rejects_non_v3_payload() {
    let skill = "linear";
    let session_id = "stream-evidence-v2";
    let state = evidence_state(skill, session_id, &["claim"]);
    let snapshot =
        evidence_checkpoint(skill, session_id, "checkpoint-2", 2, state.clone(), "claim");
    let mut stream = vec![evidence_checkpoint_stream_part(&snapshot, "claim", &state)];
    stream[0].stream_protocol = "v2".to_string();

    let err = crate::CompiledPhaseGraph::validate_stream_latest_checkpoint_evidence(
        &stream,
        &snapshot,
        &format!("sentinel.phase.{skill}.{session_id}"),
        "claim",
        &state,
    )
    .expect_err("non-v3 stream evidence must fail");
    assert!(
        err.to_string().contains("expected v3"),
        "unexpected error: {err}"
    );
}

#[test]
fn stream_latest_checkpoint_evidence_rejects_missing_latest_checkpoint_payload() {
    let skill = "linear";
    let session_id = "stream-evidence-missing-latest";
    let state = evidence_state(skill, session_id, &["claim"]);
    let latest = evidence_checkpoint(skill, session_id, "checkpoint-2", 2, state.clone(), "claim");
    let previous =
        evidence_checkpoint(skill, session_id, "checkpoint-1", 1, state.clone(), "claim");
    let stream = vec![evidence_checkpoint_stream_part(&previous, "claim", &state)];

    let err = crate::CompiledPhaseGraph::validate_stream_latest_checkpoint_evidence(
        &stream,
        &latest,
        &format!("sentinel.phase.{skill}.{session_id}"),
        "claim",
        &state,
    )
    .expect_err("stream without latest checkpoint payload must fail");
    assert!(
        err.to_string()
            .contains("omitted latest checkpoint payload"),
        "unexpected error: {err}"
    );
}

#[test]
fn stream_latest_checkpoint_evidence_rejects_forged_state_payload() {
    let skill = "linear";
    let session_id = "stream-evidence-forged-state";
    let state = evidence_state(skill, session_id, &["claim"]);
    let snapshot =
        evidence_checkpoint(skill, session_id, "checkpoint-2", 2, state.clone(), "claim");
    let forged = evidence_state(skill, session_id, &[]);
    let stream = vec![evidence_checkpoint_stream_part(&snapshot, "claim", &forged)];

    let err = crate::CompiledPhaseGraph::validate_stream_latest_checkpoint_evidence(
        &stream,
        &snapshot,
        &format!("sentinel.phase.{skill}.{session_id}"),
        "claim",
        &state,
    )
    .expect_err("forged stream state must fail");
    assert!(
        err.to_string().contains("state mismatch"),
        "unexpected error: {err}"
    );
}

#[test]
fn checkpoint_write_evidence_rejects_stale_latest_checkpoint() {
    let skill = "linear";
    let session_id = "evidence-stale-checkpoint";
    let expected = evidence_state(skill, session_id, &["claim"]);
    let stale = evidence_state(skill, session_id, &[]);
    let snapshots = vec![evidence_checkpoint(
        skill,
        session_id,
        "checkpoint-1",
        1,
        stale.clone(),
        "claim",
    )];
    let writes = vec![evidence_write(
        "checkpoint-1",
        1,
        "claim",
        serde_json::to_value(&stale).expect("json"),
    )];

    let err = crate::CompiledPhaseGraph::validate_checkpoint_write_evidence(
        skill,
        &format!("sentinel.phase.{skill}.{session_id}"),
        &expected,
        &snapshots,
        &writes,
    )
    .expect_err("stale latest checkpoint must fail");
    assert!(
        err.to_string().contains("latest checkpoint state mismatch"),
        "unexpected error: {err}"
    );
}

#[test]
fn checkpoint_write_evidence_rejects_forged_latest_write() {
    let skill = "linear";
    let session_id = "evidence-forged-write";
    let expected = evidence_state(skill, session_id, &["claim"]);
    let snapshots = vec![evidence_checkpoint(
        skill,
        session_id,
        "checkpoint-2",
        2,
        expected.clone(),
        "claim",
    )];
    let writes = vec![evidence_write(
        "checkpoint-2",
        2,
        "claim",
        serde_json::to_value(evidence_state(skill, session_id, &[])).expect("json"),
    )];

    let err = crate::CompiledPhaseGraph::validate_checkpoint_write_evidence(
        skill,
        &format!("sentinel.phase.{skill}.{session_id}"),
        &expected,
        &snapshots,
        &writes,
    )
    .expect_err("forged latest write must fail");
    assert!(
        err.to_string()
            .contains("latest state-channel write mismatch"),
        "unexpected error: {err}"
    );
}

#[test]
fn checkpoint_write_evidence_rejects_mismatched_write_thread() {
    let skill = "linear";
    let session_id = "evidence-mismatched-write-thread";
    let expected = evidence_state(skill, session_id, &["claim"]);
    let snapshots = vec![evidence_checkpoint(
        skill,
        session_id,
        "checkpoint-2",
        2,
        expected.clone(),
        "claim",
    )];
    let mut writes = vec![evidence_write(
        "checkpoint-2",
        2,
        "claim",
        serde_json::to_value(&expected).expect("json"),
    )];
    writes[0].thread_id = "sentinel.phase.linear.other-session".to_string();

    let err = crate::CompiledPhaseGraph::validate_checkpoint_write_evidence(
        skill,
        &format!("sentinel.phase.{skill}.{session_id}"),
        &expected,
        &snapshots,
        &writes,
    )
    .expect_err("mismatched write thread must fail");
    assert!(
        err.to_string().contains("write history") && err.to_string().contains("other-session"),
        "unexpected error: {err}"
    );
}

#[test]
fn checkpoint_write_evidence_rejects_out_of_order_write_history() {
    let skill = "linear";
    let session_id = "evidence-out-of-order-write";
    let older = evidence_state(skill, session_id, &[]);
    let latest = evidence_state(skill, session_id, &["claim"]);
    let snapshots = vec![
        evidence_checkpoint(skill, session_id, "checkpoint-1", 1, older.clone(), "claim"),
        evidence_checkpoint(
            skill,
            session_id,
            "checkpoint-2",
            2,
            latest.clone(),
            "claim",
        ),
    ];
    let writes = vec![
        evidence_write(
            "checkpoint-2",
            2,
            "claim",
            serde_json::to_value(&latest).expect("json"),
        ),
        evidence_write(
            "checkpoint-1",
            1,
            "claim",
            serde_json::to_value(&older).expect("json"),
        ),
    ];

    let err = crate::CompiledPhaseGraph::validate_checkpoint_write_evidence(
        skill,
        &format!("sentinel.phase.{skill}.{session_id}"),
        &latest,
        &snapshots,
        &writes,
    )
    .expect_err("out-of-order write history must fail");
    assert!(
        err.to_string().contains("write history") && err.to_string().contains("not oldest-first"),
        "unexpected error: {err}"
    );
}

#[test]
fn checkpoint_write_evidence_rejects_out_of_order_checkpoint_history() {
    let skill = "linear";
    let session_id = "evidence-out-of-order";
    let older = evidence_state(skill, session_id, &[]);
    let latest = evidence_state(skill, session_id, &["claim"]);
    let snapshots = vec![
        evidence_checkpoint(
            skill,
            session_id,
            "checkpoint-2",
            2,
            latest.clone(),
            "claim",
        ),
        evidence_checkpoint(skill, session_id, "checkpoint-1", 1, older.clone(), "claim"),
    ];
    let writes = vec![
        evidence_write(
            "checkpoint-2",
            2,
            "claim",
            serde_json::to_value(latest).expect("json"),
        ),
        evidence_write(
            "checkpoint-1",
            1,
            "claim",
            serde_json::to_value(&older).expect("json"),
        ),
    ];

    let err = crate::CompiledPhaseGraph::validate_checkpoint_write_evidence(
        skill,
        &format!("sentinel.phase.{skill}.{session_id}"),
        &older,
        &snapshots,
        &writes,
    )
    .expect_err("out-of-order checkpoint history must fail");
    assert!(
        err.to_string().contains("not oldest-first"),
        "unexpected error: {err}"
    );
}

/// Time-travel: complete claim+fetch, then replay fetch — it forks back to a
/// state where fetch is no longer completed and `current_phase` points at fetch.
#[tokio::test]
async fn replay_phase_forks_before_target() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    seed_gate(&graph, "linear", "r").await;

    graph
        .apply_verdict("linear", "r", "claim", true)
        .await
        .expect("p1");
    graph
        .update_step(
            "linear",
            "r",
            "claim",
            "0.1",
            &step("0.1"),
            StepStatus::Completed,
            Some("claim step".into()),
        )
        .await
        .expect("claim step");
    graph
        .apply_verdict("linear", "r", "fetch", true)
        .await
        .expect("p2");
    graph
        .update_step(
            "linear",
            "r",
            "fetch",
            "1.1",
            &step("1.1"),
            StepStatus::InProgress,
            Some("fetch step".into()),
        )
        .await
        .expect("fetch step");

    let forked = graph
        .replay_phase("linear", "r", "fetch", "QA requested a fresh fetch pass")
        .await
        .expect("replay");
    // claim stays done (it's before fetch); fetch is dropped for the re-run.
    assert_eq!(forked.completed_phases, vec!["claim".to_string()]);
    assert_eq!(forked.current_phase, Some(1)); // back on fetch
    assert!(!forked.complete);
    assert_eq!(forked.step_states.len(), 1);
    assert_eq!(forked.step_states[0].phase_id, "claim");
    assert_eq!(forked.step_states[0].step_id, "0.1");
    assert_eq!(forked.current_step, None);
    assert_eq!(forked.replay_events.len(), 1);
    let event = &forked.replay_events[0];
    assert_eq!(event.phase_id, "fetch");
    assert_eq!(event.reason, "QA requested a fresh fetch pass");
    assert_eq!(event.superseded_completed_phases, vec!["fetch".to_string()]);
    assert_eq!(event.superseded_step_states.len(), 1);
    assert_eq!(event.superseded_step_states[0].phase_id, "fetch");
    assert_eq!(event.superseded_step_states[0].step_id, "1.1");

    // The fork persisted as the new latest checkpoint.
    let latest = graph
        .load_latest("r")
        .await
        .expect("load")
        .expect("present");
    assert_eq!(latest.completed_phases, vec!["claim".to_string()]);
    assert_eq!(latest.step_states.len(), 1);
    assert_eq!(latest.step_states[0].phase_id, "claim");

    graph
        .apply_verdict("linear", "r", "fetch", true)
        .await
        .expect("re-complete fetch");
    let second_fork = graph
        .replay_phase("linear", "r", "fetch", "Second audited fetch replay")
        .await
        .expect("second replay");
    assert_eq!(second_fork.replay_events.len(), 2);
    assert_eq!(
        second_fork
            .replay_events
            .iter()
            .map(|event| event.reason.as_str())
            .collect::<Vec<_>>(),
        vec![
            "QA requested a fresh fetch pass",
            "Second audited fetch replay"
        ]
    );
}

#[tokio::test]
async fn replay_unknown_phase_errors() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    assert!(graph
        .replay_phase("linear", "r", "ghost", "operator-requested replay")
        .await
        .is_err());
}

#[tokio::test]
async fn replay_without_checkpoint_errors_instead_of_seeding() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    let err = graph
        .replay_phase(
            "linear",
            "new-session",
            "claim",
            "operator-requested replay",
        )
        .await
        .expect_err("replay must require durable checkpoint history");

    assert!(matches!(
        err,
        crate::GraphEngineError::MissingCheckpoint {
            skill,
            session_id,
            phase,
        } if skill == "linear" && session_id == "new-session" && phase == "claim"
    ));
    assert!(graph
        .load_latest("new-session")
        .await
        .expect("load")
        .is_none());
}

#[tokio::test]
async fn replay_requires_target_phase_to_be_completed() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    seed_gate(&graph, "linear", "partial-session").await;
    graph
        .apply_verdict("linear", "partial-session", "claim", true)
        .await
        .expect("claim");

    let err = graph
        .replay_phase(
            "linear",
            "partial-session",
            "fetch",
            "operator-requested replay",
        )
        .await
        .expect_err("uncompleted phase must not be replayable");

    assert!(matches!(
        err,
        crate::GraphEngineError::PhaseNotCompletedForReplay {
            skill,
            session_id,
            phase,
        } if skill == "linear" && session_id == "partial-session" && phase == "fetch"
    ));
}

#[tokio::test]
async fn replay_requires_non_empty_reason() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");

    let err = graph
        .replay_phase("linear", "r", "claim", "   ")
        .await
        .expect_err("blank replay reason must be rejected");

    assert!(matches!(
        err,
        crate::GraphEngineError::InvalidReplayReason { skill, phase }
            if skill == "linear" && phase == "claim"
    ));
}

#[tokio::test]
async fn update_step_persists_through_langgraph_checkpoint_history() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let db = tmp.path().join("steps.db");
    let db_path = db.to_str().expect("utf8 path");
    let workflow = fixture();

    let saver = phase_checkpointer(db_path).await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&workflow, saver).expect("compile");
    graph
        .run_until_gate("linear", "step-session")
        .await
        .expect("initial gate checkpoint");
    let mut step_policy = step("0.1");
    step_policy.timeout_ms = Some(30_000);
    step_policy.retry_policy.max_attempts = 3;
    step_policy.retry_policy.backoff_ms = 250;
    step_policy.retry_policy.retry_on = vec!["timeout".to_string()];
    step_policy.circuit_breaker.failure_threshold = 2;
    step_policy.circuit_breaker.cooldown_ms = 5_000;
    let updated = graph
        .update_step(
            "linear",
            "step-session",
            "claim",
            "0.1",
            &step_policy,
            StepStatus::Completed,
            Some("claim drafted".into()),
        )
        .await
        .expect("step update");
    assert_eq!(updated.step_states.len(), 1);
    assert_eq!(updated.step_states[0].status, StepStatus::Completed);
    assert_eq!(
        updated.step_states[0].summary.as_deref(),
        Some("claim drafted")
    );

    let fresh_saver = phase_checkpointer(db_path).await.expect("fresh saver");
    let fresh =
        compile_skill_graph_with_checkpointer(&workflow, fresh_saver).expect("fresh compile");
    let latest = fresh
        .load_latest("step-session")
        .await
        .expect("load")
        .expect("present");
    assert_eq!(latest.step_states.len(), 1);
    assert_eq!(latest.step_states[0].step_id, "0.1");
    assert_eq!(latest.step_states[0].phase_id, "claim");
    assert_eq!(latest.step_states[0].status, StepStatus::Completed);
    assert_eq!(latest.step_policy_evidence.len(), 1);
    let policy = &latest.step_policy_evidence[0];
    assert_eq!(policy.phase_id, "claim");
    assert_eq!(policy.step_id, "0.1");
    assert_eq!(policy.timeout_ms, Some(30_000));
    assert_eq!(policy.retry_max_attempts, 3);
    assert_eq!(policy.retry_backoff_ms, 250);
    assert_eq!(policy.retry_on, vec!["timeout".to_string()]);
    assert_eq!(policy.circuit_failure_threshold, 2);
    assert_eq!(policy.circuit_cooldown_ms, 5_000);

    let history = fresh.phase_history("step-session").await.expect("history");
    assert!(
        history.iter().any(|state| !state.step_states.is_empty()),
        "step update must be visible in graph checkpoint history"
    );
}

#[tokio::test]
async fn update_step_rejects_terminal_rewrite_without_new_checkpoint() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    graph
        .run_until_gate("linear", "duplicate-step-session")
        .await
        .expect("initial gate checkpoint");
    graph
        .update_step(
            "linear",
            "duplicate-step-session",
            "claim",
            "0.1",
            &step("0.1"),
            StepStatus::Completed,
            Some("sealed step".into()),
        )
        .await
        .expect("initial step completion");
    let checkpoint_count = graph
        .phase_snapshots("duplicate-step-session")
        .await
        .expect("snapshots")
        .len();

    for status in [StepStatus::Completed, StepStatus::InProgress] {
        let err = graph
            .update_step(
                "linear",
                "duplicate-step-session",
                "claim",
                "0.1",
                &step("0.1"),
                status,
                Some("rewrite".into()),
            )
            .await
            .expect_err("terminal step state must be replayed before rewrite");
        assert!(matches!(
            err,
            crate::GraphEngineError::StepAlreadyTerminal {
                ref skill,
                ref session_id,
                ref phase,
                ref step_id,
                ref status,
            } if skill == "linear"
                && session_id == "duplicate-step-session"
                && phase == "claim"
                && step_id == "0.1"
                && status == "completed"
        ));
    }

    assert_eq!(
        graph
            .phase_snapshots("duplicate-step-session")
            .await
            .expect("snapshots")
            .len(),
        checkpoint_count,
        "terminal step rewrites must not append graph checkpoints"
    );
}

#[tokio::test]
async fn update_step_keeps_checkpointed_steps_when_session_projection_is_stale() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    graph
        .run_until_gate("linear", "stale-session")
        .await
        .expect("initial gate checkpoint");

    graph
        .update_step(
            "linear",
            "stale-session",
            "claim",
            "0.1",
            &step("0.1"),
            StepStatus::Completed,
            Some("checkpointed".into()),
        )
        .await
        .expect("first step");

    let updated = graph
        .update_step(
            "linear",
            "stale-session",
            "claim",
            "0.2",
            &step("0.2"),
            StepStatus::Completed,
            Some("second".into()),
        )
        .await
        .expect("second step");

    let step_ids: Vec<_> = updated
        .step_states
        .iter()
        .map(|step| step.step_id.as_str())
        .collect();
    assert_eq!(step_ids, vec!["0.1", "0.2"]);
}

#[tokio::test]
async fn update_state_as_node_rejects_embedded_session_mismatch() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    graph
        .run_until_gate("linear", "session-match")
        .await
        .expect("initial gate checkpoint");
    let mut state = graph
        .load_latest("session-match")
        .await
        .expect("load")
        .expect("checkpoint");
    state.session_id = "other-session".to_string();

    let err = graph
        .update_state_as_node("session-match", state, START)
        .await
        .expect_err("embedded session mismatch must not be checkpointed");

    assert!(err.to_string().contains("state session mismatch"));
}

#[tokio::test]
async fn update_step_without_checkpoint_fails_closed() {
    let saver = phase_checkpointer(":memory:").await.expect("saver");
    let graph = compile_skill_graph_with_checkpointer(&fixture(), saver).expect("compile");
    let mut stale = WorkflowState::new("linear", "stale-step-no-checkpoint");
    stale.current_phase = Some(2);
    stale.completed_phases = vec!["claim".to_string(), "fetch".to_string()];
    stale.complete = true;
    stale.update_step(
        "fetch",
        "9.9",
        StepStatus::Completed,
        Some("forged cached step".into()),
    );

    let err = graph
        .update_step(
            "linear",
            "stale-step-no-checkpoint",
            "claim",
            "0.1",
            &step("0.1"),
            StepStatus::Completed,
            Some("step only".into()),
        )
        .await
        .expect_err("step updates must require durable checkpoint history");

    assert!(matches!(
        err,
        crate::GraphEngineError::MissingCheckpoint {
            skill,
            session_id,
            phase,
        } if skill == "linear" && session_id == "stale-step-no-checkpoint" && phase == "claim"
    ));
    assert!(graph
        .load_latest("stale-step-no-checkpoint")
        .await
        .expect("load")
        .is_none());
}
