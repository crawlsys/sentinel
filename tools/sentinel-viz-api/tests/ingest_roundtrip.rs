//! Writer↔reader round-trip: ingest synthetic sentinel metrics JSONL with the
//! Rust ingester, then read the store back through the same `db` + `graph`
//! path the API serves and assert the sessions, hooks, and `has_invocation`
//! edges materialise. This proves the ingester writes exactly the `events`
//! schema the reader expects.

use sentinel_viz_api::graph::GraphOpts;
use sentinel_viz_api::ingest::{self, MetricsPaths};
use sentinel_viz_api::{db, graph};
use tempfile::TempDir;

fn write(path: &std::path::Path, lines: &[&str]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

#[test]
fn ingest_then_read_materialises_sessions_hooks_and_edges() {
    let tmp = TempDir::new().unwrap();
    let metrics = tmp.path().join(".claude/sentinel/metrics");

    write(
        &metrics.join("sessions.jsonl"),
        &[r#"{"session_id":"sess-a","cwd":"/work","platform":"windows","ts":"2026-05-26T00:00:00Z"}"#],
    );
    write(
        &metrics.join("hook-invocations.jsonl"),
        &[
            r#"{"trace_id":"tr-1","hook":"git_hygiene","event":"PreToolUse","outcome":"allow","session_id":"sess-a","duration_us":12,"ts":"2026-05-26T00:00:01Z"}"#,
            // Same trace_id, different hook — composite dedupe must keep both.
            r#"{"trace_id":"tr-1","hook":"phase_gate","event":"PreToolUse","outcome":"deny","session_id":"sess-a","duration_us":34,"ts":"2026-05-26T00:00:01Z"}"#,
            // Unknown session_id — should get a stub session.
            r#"{"trace_id":"tr-2","hook":"commit_hygiene","event":"Stop","outcome":"allow","session_id":"ghost","duration_us":7,"ts":"2026-05-26T00:00:02Z"}"#,
        ],
    );

    let store = tmp.path().join("sentinel.db");

    // Point the ingester at the temp home via the SENTINEL_VIZ_HOME override
    // (the same pattern transcript.rs uses). This is the only test in this
    // file and it does not run concurrently with other env mutation.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("SENTINEL_VIZ_HOME", tmp.path());
    }
    let paths = MetricsPaths::from_home().unwrap();
    ingest::run_one_shot(&store, &paths).unwrap();
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("SENTINEL_VIZ_HOME");
    }

    // Read it back through the API's own path, including hook nodes.
    let conn = db::open_ro(&store).unwrap();
    let g = graph::load_graph_with(
        &conn,
        GraphOpts { limit: 100, since_secs: None, include_hooks: true, focused_session: None },
    )
    .unwrap();

    assert!(g.error.is_none(), "unexpected error: {:?}", g.error);

    // Sessions: the real one plus the stub for "ghost".
    let session_ids: std::collections::BTreeSet<String> = g
        .nodes
        .iter()
        .filter(|n| n.kind == "SentinelSession")
        .filter_map(|n| n.data.get("session_id").and_then(|v| v.as_str()).map(String::from))
        .collect();
    assert!(session_ids.contains("sess-a"), "real session missing: {session_ids:?}");
    assert!(session_ids.contains("ghost"), "stub session missing: {session_ids:?}");

    // All three hook invocations survived composite dedupe.
    let hook_count = g.nodes.iter().filter(|n| n.kind == "SentinelHookInvocation").count();
    assert_eq!(hook_count, 3, "expected 3 hook invocations, got {hook_count}");

    // has_invocation edges link sessions to their hooks.
    let has_inv = g.edges.iter().filter(|e| e.kind == "has_invocation").count();
    assert_eq!(has_inv, 3, "expected 3 has_invocation edges, got {has_inv}");
    assert!(
        g.edges
            .iter()
            .any(|e| e.kind == "has_invocation" && e.source == "SentinelSession#sess-a"),
        "expected an edge from SentinelSession#sess-a"
    );

    // Domain events surface in the ticker (session_started + hook_ingested
    // + one hook_denied for the deny outcome).
    let kinds: std::collections::BTreeSet<String> =
        g.events.iter().map(|e| e.kind.clone()).collect();
    assert!(kinds.contains("sentinel.session_started"));
    assert!(kinds.contains("sentinel.hook_ingested"));
    assert!(kinds.contains("sentinel.hook_denied"));
}
