//! Read the real bridge `SQLite` store and assert each known event type
//! appears at least once. Skipped (with a printed note) if the store
//! is not present on this machine — keeps the suite green on a fresh
//! checkout without a live bridge.

use std::collections::HashMap;

use sentinel_viz_api::db;
use sentinel_viz_api::model::kind;

#[test]
fn reads_events_of_each_known_kind() {
    let path = db::default_db_path().expect("resolve default db path");
    if !path.exists() {
        eprintln!(
            "skipping: no sentinel db at {} (bridge not running on this host)",
            path.display()
        );
        return;
    }

    let conn = db::open_ro(&path).expect("open ro");
    let events = db::read_events(&conn).expect("read events");
    assert!(!events.is_empty(), "expected at least one event");

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for e in &events {
        if let Some(k) = KNOWN_KINDS.iter().find(|k| **k == e.kind.as_str()) {
            *counts.entry(*k).or_insert(0) += 1;
        }
    }

    for k in KNOWN_KINDS {
        assert!(
            counts.get(k).copied().unwrap_or(0) > 0,
            "expected at least one event of kind `{k}`, got counts: {counts:?}"
        );
    }
}

const KNOWN_KINDS: &[&str] = &[
    kind::OBJECT_CREATED,
    kind::RELATION_CREATED,
    kind::SESSION_STARTED,
    kind::HOOK_INGESTED,
    kind::TOOL_CALL_OBSERVED,
];
