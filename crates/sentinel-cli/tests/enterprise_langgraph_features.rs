//! Guard the binary feature surface for production LangGraph checkpoint stores.
//!
//! The graph crates already expose SQLite, Postgres, and Redis checkpointers.
//! The `sentinel` binary must forward those features itself so a production
//! install can select an enterprise `SENTINEL_*_GRAPH_CHECKPOINTER` backend
//! without rebuilding dependency crates by hand.

#[test]
fn sentinel_binary_defaults_include_enterprise_langgraph_checkpointers() {
    let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml"))
        .expect("sentinel-cli Cargo.toml should parse");
    let features = manifest["features"]
        .as_table()
        .expect("sentinel-cli Cargo.toml should declare features");

    let default = feature_values(features, "default");
    assert!(
        default.contains(&"sqlite"),
        "default sentinel binary must keep the local durable SQLite checkpointer"
    );
    assert!(
        default.contains(&"postgres"),
        "default sentinel binary must include enterprise Postgres LangGraph support"
    );
    assert!(
        default.contains(&"redis"),
        "default sentinel binary must include enterprise Redis LangGraph support"
    );

    assert_eq!(
        feature_values(features, "sqlite"),
        vec!["sentinel-graph/sqlite", "sentinel-infrastructure/sqlite"],
        "sqlite feature must forward to both phase and decision graph crates"
    );
    assert_eq!(
        feature_values(features, "postgres"),
        vec![
            "sentinel-graph/postgres",
            "sentinel-infrastructure/postgres"
        ],
        "postgres feature must forward to both phase and decision graph crates"
    );
    assert_eq!(
        feature_values(features, "redis"),
        vec!["sentinel-graph/redis", "sentinel-infrastructure/redis"],
        "redis feature must forward to both phase and decision graph crates"
    );
}

#[test]
fn graph_crate_defaults_include_enterprise_langgraph_checkpointers() {
    for (crate_name, manifest_source) in [
        (
            "sentinel-graph",
            include_str!("../../sentinel-graph/Cargo.toml"),
        ),
        (
            "sentinel-infrastructure",
            include_str!("../../sentinel-infrastructure/Cargo.toml"),
        ),
    ] {
        let manifest: toml::Value = toml::from_str(manifest_source)
            .unwrap_or_else(|err| panic!("{crate_name} Cargo.toml should parse: {err}"));
        let features = manifest["features"]
            .as_table()
            .unwrap_or_else(|| panic!("{crate_name} Cargo.toml should declare features"));

        let default = feature_values(features, "default");
        assert!(
            default.contains(&"sqlite"),
            "{crate_name} default features must keep the local durable SQLite checkpointer"
        );
        assert!(
            default.contains(&"postgres"),
            "{crate_name} default features must include enterprise Postgres LangGraph support"
        );
        assert!(
            default.contains(&"redis"),
            "{crate_name} default features must include enterprise Redis LangGraph support"
        );
        assert_eq!(
            feature_values(features, "sqlite"),
            vec!["langgraph-core/sqlite"],
            "{crate_name} sqlite feature must forward to langgraph-core"
        );
        assert_eq!(
            feature_values(features, "postgres"),
            vec!["langgraph-core/postgres"],
            "{crate_name} postgres feature must forward to langgraph-core"
        );
        assert_eq!(
            feature_values(features, "redis"),
            vec!["langgraph-core/redis-checkpoint"],
            "{crate_name} redis feature must forward to langgraph-core"
        );
    }
}

#[test]
fn langgraph_dependencies_do_not_enable_backend_defaults_implicitly() {
    let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml"))
        .expect("sentinel-cli Cargo.toml should parse");
    let dependencies = manifest["dependencies"]
        .as_table()
        .expect("sentinel-cli Cargo.toml should declare dependencies");

    for dependency in ["sentinel-graph", "sentinel-infrastructure"] {
        let spec = dependencies[dependency]
            .as_table()
            .unwrap_or_else(|| panic!("{dependency} dependency should be a table"));
        assert_eq!(
            spec.get("default-features").and_then(toml::Value::as_bool),
            Some(false),
            "{dependency} backend features must be selected by sentinel-cli features"
        );
    }

    let workspace_manifest: toml::Value = toml::from_str(include_str!("../../../Cargo.toml"))
        .expect("workspace Cargo.toml should parse");
    let workspace_dependencies = workspace_manifest["workspace"]["dependencies"]
        .as_table()
        .expect("workspace Cargo.toml should declare dependencies");
    let infrastructure = workspace_dependencies["sentinel-infrastructure"]
        .as_table()
        .expect("workspace sentinel-infrastructure dependency should be a table");
    assert_eq!(
        infrastructure
            .get("default-features")
            .and_then(toml::Value::as_bool),
        Some(false),
        "workspace sentinel-infrastructure dependency must not re-enable LangGraph backend defaults"
    );

    for (crate_name, manifest_source) in [
        (
            "sentinel-git-interceptor",
            include_str!("../../sentinel-git-interceptor/Cargo.toml"),
        ),
        (
            "sentinel-npx-interceptor",
            include_str!("../../sentinel-npx-interceptor/Cargo.toml"),
        ),
    ] {
        let manifest: toml::Value = toml::from_str(manifest_source)
            .unwrap_or_else(|err| panic!("{crate_name} Cargo.toml should parse: {err}"));
        let dependencies = manifest["dependencies"]
            .as_table()
            .unwrap_or_else(|| panic!("{crate_name} Cargo.toml should declare dependencies"));
        let infrastructure = dependencies["sentinel-infrastructure"]
            .as_table()
            .unwrap_or_else(|| {
                panic!("{crate_name} sentinel-infrastructure dependency should be a table")
            });
        assert_eq!(
            infrastructure
                .get("default-features")
                .and_then(toml::Value::as_bool),
            Some(false),
            "{crate_name} must not pull LangGraph backend features into non-graph interceptor binaries"
        );
    }
}

#[test]
fn phase_graph_runtime_snapshots_tenant_scope_from_compiled_graph() {
    let graph_source = include_str!("../../sentinel-graph/src/lib.rs");
    assert!(
        graph_source.contains("sentinel.checkpointer_tenant_scope"),
        "phase graph nodes must stamp compiled checkpointer tenant scope metadata"
    );
    assert!(
        graph_source.contains("checkpointer_tenant_scope: Option<String>"),
        "phase graph topology must expose hosted checkpointer tenant scope"
    );
    assert!(
        graph_source.contains("pub fn thread_id_for_session"),
        "compiled phase graphs must expose env-free thread id derivation"
    );
    assert!(
        !graph_source.contains("pub fn phase_thread_id("),
        "sentinel-graph must not expose env-derived phase thread id helpers"
    );
    assert!(
        !graph_source.contains("tenant_scope_for_checkpointer_backend(&checkpointer_backend)?"),
        "phase graph compilation must not re-read process env for tenant scope"
    );

    let projection_source = include_str!("../src/phase_graph_projection.rs");
    assert!(
        projection_source.contains(".thread_id_for_session("),
        "CLI/API phase graph projection must derive thread ids from the compiled graph"
    );
    assert!(
        !projection_source.contains("sentinel_graph::phase_thread_id("),
        "CLI/API phase graph projection must not re-read env for compiled graph evidence"
    );

    let mcp_source = include_str!("../src/mcp_cmd.rs");
    assert!(
        mcp_source.contains(".thread_id_for_session("),
        "MCP phase graph evidence must derive thread ids from the compiled graph"
    );
    assert!(
        !mcp_source.contains("sentinel_graph::phase_thread_id("),
        "MCP phase graph evidence must not re-read env for compiled graph evidence"
    );
}

#[test]
fn mcp_startup_wires_full_enterprise_langgraph_runtime() {
    let handler_source = include_str!("../../sentinel-application/src/mcp_handler.rs");
    let mcp_source = include_str!("../src/mcp_cmd.rs");

    for required in [
        "proof_engine.phase_graph_authority",
        "proof_engine.step_graph_authority",
        "workflow_catalog",
        "proof_archive_backing",
        "llm_port",
        "severity_graph_auditor",
        "pm_audit_graph_auditor",
        "linear_health_graph_auditor",
        "dev_scorecard_graph_auditor",
        "token_cost_graph_auditor",
        "token_usage_graph_auditor",
        "cache_efficiency_graph_auditor",
        "cost_per_point_graph_auditor",
        "deploy_frequency_graph_auditor",
        "pr_review_graph_auditor",
        "roi_graph_auditor",
        "sla_graph_auditor",
        "code_reconciliation_auditor",
        "mcp_proof_read_graph_auditor",
        "eval_runner",
        "ba_draft_runner",
    ] {
        assert!(
            handler_source.contains(&format!("missing.push(\"{required}\")")),
            "MCP enterprise LangGraph runtime validator must require {required}"
        );
    }

    for wiring in [
        ".with_phase_graph_authority(",
        ".with_step_graph_authority(",
        ".with_workflows(",
        ".with_archive(",
        ".with_llm(",
        ".with_severity_graph_auditor(",
        ".with_pm_audit_graph_auditor(",
        ".with_linear_health_graph_auditor(",
        ".with_dev_scorecard_graph_auditor(",
        ".with_token_cost_graph_auditor(",
        ".with_token_usage_graph_auditor(",
        ".with_cache_efficiency_graph_auditor(",
        ".with_cost_per_point_graph_auditor(",
        ".with_deploy_frequency_graph_auditor(",
        ".with_pr_review_graph_auditor(",
        ".with_roi_graph_auditor(",
        ".with_sla_graph_auditor(",
        ".with_code_reconciliation_auditor(",
        ".with_mcp_proof_read_graph_auditor(",
        ".with_eval_runner(",
        ".with_ba_draft_runner(",
    ] {
        assert!(
            mcp_source.contains(wiring),
            "sentinel mcp startup must wire {wiring} into the enterprise LangGraph runtime"
        );
    }

    assert!(
        mcp_source.contains(".validate_enterprise_langgraph_runtime()"),
        "sentinel mcp startup must fail closed if any enterprise LangGraph authority is missing"
    );
}

fn feature_values<'a>(
    features: &'a toml::map::Map<String, toml::Value>,
    name: &str,
) -> Vec<&'a str> {
    features[name]
        .as_array()
        .unwrap_or_else(|| panic!("{name} feature should be an array"))
        .iter()
        .map(|value| {
            value
                .as_str()
                .unwrap_or_else(|| panic!("{name} feature entries should be strings"))
        })
        .collect()
}
