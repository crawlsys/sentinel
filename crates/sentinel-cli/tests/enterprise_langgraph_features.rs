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
