//! Build a synthetic Claude transcript JSONL and verify
//! `session_activity` rolls it up into segments correctly.

// This integration test mutates `SENTINEL_VIZ_HOME` to point the
// transcript finder at a temp tree; `std::env::set_var` is `unsafe`
// on current Rust. Scoped to the test target only.
#![allow(unsafe_code)]

use std::io::Write;

use sentinel_viz_api::activity;
use tempfile::TempDir;

const fn fixture_jsonl() -> &'static str {
    // Minimal Claude transcript: user input → assistant turn with one
    // Bash tool_use → user message containing the matching tool_result.
    r#"{"type":"user","timestamp":"2026-05-25T00:00:00Z","message":{"content":"hello"}}
{"type":"assistant","timestamp":"2026-05-25T00:00:01Z","message":{"content":[{"type":"text","text":"running ls"},{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"ls -la /tmp"}}]}}
{"type":"user","timestamp":"2026-05-25T00:00:02Z","message":{"content":[{"type":"tool_result","tool_use_id":"tu_1","content":[{"type":"text","text":"total 0\ndrwx... .\n"}],"is_error":false}]}}
"#
}

fn write_transcript(dir: &std::path::Path, sid: &str) -> std::path::PathBuf {
    // Mirror ~/.claude/projects/<some-cwd>/<sid>.jsonl layout: roots are
    // scanned, so the transcript must live one level under a root.
    let sub = dir.join("proj-x");
    std::fs::create_dir_all(&sub).unwrap();
    let path = sub.join(format!("{sid}.jsonl"));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(fixture_jsonl().as_bytes()).unwrap();
    path
}

#[test]
fn activity_rolls_user_input_and_assistant_turn() {
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path();
    let projects = fake_home.join(".claude/projects");
    std::fs::create_dir_all(&projects).unwrap();
    write_transcript(&projects, "sess-a");

    // Point the transcript finder at the temp tree via SENTINEL_VIZ_HOME.
    // We use this override (not HOME) because dirs::home_dir() ignores
    // $HOME on Windows, so a HOME-only override is a silent no-op there.
    let orig_home = std::env::var("SENTINEL_VIZ_HOME").ok();
    // SAFETY: tests run single-threaded by default in cargo test
    // for this binary; not setting RUST_TEST_THREADS=1 is fine here
    // because we have no other tests in this file.
    unsafe {
        std::env::set_var("SENTINEL_VIZ_HOME", fake_home);
    }

    let r = activity::session_activity("sess-a", 80, None, 30);
    match &orig_home {
        Some(orig) => unsafe { std::env::set_var("SENTINEL_VIZ_HOME", orig) },
        None => unsafe { std::env::remove_var("SENTINEL_VIZ_HOME") },
    }

    assert_eq!(r.session_id, "sess-a");
    assert!(r.transcript.is_some(), "transcript path should be set");
    assert_eq!(r.total, Some(4), "user + assistant text + assistant tool_use + tool_result = 4 atomic events");
    assert_eq!(r.total_segments, Some(2), "user_input + assistant_turn");

    let seg = r.segments.iter().find(|s| s.kind == "assistant_turn").unwrap();
    assert_eq!(seg.tool_count, 1);
    assert_eq!(seg.tools, vec!["Bash"]);
    assert_eq!(seg.label, "Bash");
    let tc = seg.tool_calls.first().unwrap();
    assert_eq!(tc.tool, "Bash");
    assert_eq!(tc.summary, "ls -la /tmp");
    assert_eq!(tc.result_preview.as_deref(), Some("total 0 drwx... ."));
    assert_eq!(tc.error, Some(false));
}

#[test]
fn activity_returns_empty_for_missing_session() {
    let tmp = TempDir::new().unwrap();
    let orig_home = std::env::var("SENTINEL_VIZ_HOME").ok();
    unsafe { std::env::set_var("SENTINEL_VIZ_HOME", tmp.path()) };

    let r = activity::session_activity("does-not-exist", 80, None, 30);
    match &orig_home {
        Some(orig) => unsafe { std::env::set_var("SENTINEL_VIZ_HOME", orig) },
        None => unsafe { std::env::remove_var("SENTINEL_VIZ_HOME") },
    }
    assert_eq!(r.session_id, "does-not-exist");
    assert!(r.transcript.is_none());
    assert!(r.segments.is_empty());
}

#[test]
fn tool_summary_known_tools() {
    let bash = activity::tool_summary("Bash", &serde_json::json!({"command": "echo hi"}));
    assert_eq!(bash, "echo hi");

    let read = activity::tool_summary("Read", &serde_json::json!({"file_path": "/tmp/x"}));
    assert_eq!(read, "/tmp/x");

    let edit = activity::tool_summary("Edit", &serde_json::json!({
        "file_path": "/tmp/x",
        "new_string": "abc",
    }));
    assert!(edit.starts_with("/tmp/x  →  abc"), "got: {edit}");

    let agent = activity::tool_summary("Agent", &serde_json::json!({
        "description": "find files",
        "subagent_type": "Explore",
    }));
    assert_eq!(agent, "find files · Explore");
}
