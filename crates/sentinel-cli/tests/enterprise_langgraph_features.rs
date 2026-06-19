//! Guard the binary feature surface for production LangGraph checkpoint stores.
//!
//! The graph crates already expose SQLite and Postgres checkpointers. The
//! `sentinel` binary must forward those features itself so a production install
//! can select `SENTINEL_*_GRAPH_CHECKPOINTER=postgres` without rebuilding
//! dependency crates by hand.

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
