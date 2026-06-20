//! Guard Sentinel's decision graphs against opaque LangGraph execution.
//!
//! Every infrastructure decision graph must compile with a durable checkpointer
//! and run through the shared v3 streaming authority so CLI/MCP/API evidence
//! includes typed stream parts, Sentinel custom node events, checkpoints, write
//! history, and topology. Direct `execute` calls are not an enterprise audit
//! surface.

use std::path::{Path, PathBuf};

#[test]
fn all_langgraph_decision_graphs_use_v3_streaming_authority() {
    let graph_sources = langgraph_decision_graph_sources();
    assert!(
        graph_sources.len() >= 54,
        "expected broad infrastructure LangGraph decision-graph coverage, found {} files",
        graph_sources.len()
    );

    for path in graph_sources {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));

        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unknown>");
        assert!(
            source.contains("stream_decision_run("),
            "{label} must execute through decision_graph_introspection::stream_decision_run"
        );
        assert!(
            source.contains("emit_decision_node_event("),
            "{label} must emit Sentinel custom node evidence through emit_decision_node_event"
        );
        assert!(
            source.contains("DecisionGraphStreamPart"),
            "{label} must expose typed LangGraph v3 stream evidence"
        );
        assert!(
            source.contains("stream: Vec<"),
            "{label} run report must carry LangGraph stream evidence"
        );
        assert!(
            source.contains("checkpoint_history("),
            "{label} run report must carry durable checkpoint history"
        );
        assert!(
            source.contains("terminal_decision_checkpoint_result("),
            "{label} authorization must validate the terminal checkpoint through the shared helper"
        );
        assert!(
            source.contains("validate_decision_graph_run("),
            "{label} run report must validate terminal state, checkpoint history, writes, stream, and topology"
        );
        assert!(
            source.contains("run_thread_id_for_compiled("),
            "{label} must derive thread ids from compiled graph metadata"
        );
        assert!(
            source.contains("topology("),
            "{label} run report must carry LangGraph topology evidence"
        );
        assert!(
            source.contains(".with_checkpointer("),
            "{label} must compile with a durable LangGraph checkpointer"
        );
        assert!(
            source.contains("tenant_scope_metadata_value("),
            "{label} must capture checkpointer tenant scope at graph compile time"
        );
        assert!(
            source.contains("sentinel.checkpointer_tenant_scope"),
            "{label} must attach tenant scope to LangGraph node metadata"
        );
        assert!(
            source.contains(".compile_with_config("),
            "{label} must preserve LangGraph runtime configuration during compilation"
        );
        assert!(
            source.contains(".with_context_schema("),
            "{label} must preserve LangGraph execution context schema during compilation"
        );
        assert!(
            !source.contains("get_stream_writer("),
            "{label} must use emit_decision_node_event instead of direct stream-writer calls"
        );
        assert!(
            !source.contains("decision_graph_store::run_thread_id("),
            "{label} must not derive runtime thread ids from process env"
        );
        assert!(
            !source.contains(".execute("),
            "{label} must not bypass the v3 streaming authority with direct execute"
        );
    }
}

fn langgraph_decision_graph_sources() -> Vec<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = std::fs::read_dir(&src)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", src.display()))
        .map(|entry| entry.expect("source entry should be readable").path())
        .filter_map(|path| {
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
            (source.contains("StateGraphBuilder") && source.contains("build_")).then_some(path)
        })
        .collect::<Vec<_>>();

    files.sort();
    files
}
