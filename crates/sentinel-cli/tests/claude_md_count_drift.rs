//! Guard against the repo `CLAUDE.md` accreting stale counts.
//!
//! `CLAUDE.md` claimed "27 hooks", "7 subcommands", and "11 MCP tools" long
//! after the real numbers were 79 / 34 / 15 — pure drift, because nothing
//! re-counted on change. This test pins the three drift-prone numeric claims
//! to the actual code. A claim that goes stale (or whose anchoring phrase is
//! reworded so the number disappears) fails here with an actionable message.
//!
//! Scope is deliberately the THREE high-value counts only — not every number
//! in the doc — so the guard stays robust against ordinary prose edits.

use std::path::Path;

/// Repo-root `CLAUDE.md`, embedded at compile time (3 dirs up from this test:
/// tests/ -> sentinel-cli/ -> crates/ -> repo root).
const CLAUDE_MD: &str = include_str!("../../../CLAUDE.md");
/// The hook dispatcher + MCP command source, for source-derived truth.
const HOOK_CMD_SRC: &str = include_str!("../src/hook_cmd.rs");
const MAIN_SRC: &str = include_str!("../src/main.rs");
const MCP_CMD_SRC: &str = include_str!("../src/mcp_cmd.rs");

/// Pull the integer captured by the first regex-like pattern: we avoid a regex
/// dep and just find `needle` then read the integer that immediately precedes
/// or follows per the caller's slice. Returns the parsed number.
fn claimed_number(anchor_before: &str, anchor_after: &str) -> Option<u64> {
    // Find "<anchor_before><digits><anchor_after>" and parse the digits.
    let start = CLAUDE_MD.find(anchor_before)? + anchor_before.len();
    let rest = &CLAUDE_MD[start..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    // Confirm the after-anchor follows the digits (cheap sanity that we matched
    // the right sentence, not just any number).
    let after = &rest[digits.len()..];
    if !after.starts_with(anchor_after) {
        return None;
    }
    digits.parse().ok()
}

#[test]
fn claude_md_hook_count_matches_source() {
    // Truth: number of hook .rs files (excl. mod.rs) — the doc says
    // "N hook modules (one `.rs` file per hook...)".
    let hooks_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../sentinel-application/src/hooks");
    let actual = std::fs::read_dir(&hooks_dir)
        .expect("hooks dir readable")
        .filter_map(Result::ok)
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.ends_with(".rs") && n != "mod.rs"
        })
        .count() as u64;

    let claimed = claimed_number("Use cases: engine, classifier, gate, ", " hook modules")
        .expect(
            "CLAUDE.md must contain '...gate, <N> hook modules' (the architecture table). \
             If the wording changed, update this test AND keep the count honest.",
        );

    assert_eq!(
        claimed, actual,
        "CLAUDE.md claims {claimed} hook modules but there are {actual} .rs files in \
         crates/sentinel-application/src/hooks (excl. mod.rs). Update CLAUDE.md."
    );
}

#[test]
fn claude_md_subcommand_count_matches_source() {
    // Truth: top-level variants of `enum Commands` in main.rs.
    let enum_body = MAIN_SRC
        .split_once("enum Commands")
        .and_then(|(_, rest)| rest.split_once('{'))
        .map(|(_, body)| body)
        .expect("main.rs has an `enum Commands {`");
    // Count lines that start a variant: 4-space indent + Uppercase ident.
    // Stop at the closing brace of the enum (first line that is `}` at col 0).
    let mut actual: u64 = 0;
    for line in enum_body.lines() {
        if line.starts_with('}') {
            break;
        }
        let t = line.trim_start();
        // A variant line begins with an uppercase letter (skip doc comments,
        // attributes, blank lines, and nested-field lines which are indented
        // deeper than 4 spaces).
        let four_space = line.len() - t.len() == 4;
        if four_space
            && t.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        {
            actual += 1;
        }
    }

    let claimed = claimed_number("CLI (", " top-level subcommands)").expect(
        "CLAUDE.md must contain 'CLI (<N> top-level subcommands)'. If the wording \
         changed, update this test AND keep the count honest.",
    );

    assert_eq!(
        claimed, actual,
        "CLAUDE.md claims {claimed} top-level subcommands but `enum Commands` in \
         main.rs has {actual} variants. Update CLAUDE.md."
    );
}

#[test]
fn claude_md_mcp_tool_count_matches_source() {
    // Truth: distinct `"name": "sentinel__<tool>"` entries in the MCP schema.
    let mut tools: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let needle = "\"name\": \"sentinel__";
    let mut hay = MCP_CMD_SRC;
    while let Some(i) = hay.find(needle) {
        let after = &hay[i + needle.len()..];
        let name: String = after.chars().take_while(|c| *c != '"').collect();
        // store into the set via a leaked-free approach: use the slice
        let end = name.len();
        tools.insert(&after[..end]);
        hay = &after[end..];
    }
    let actual = tools.len() as u64;

    let claimed = claimed_number("MCP host (`sentinel mcp`, defined in `crates/sentinel-cli/src/mcp_cmd.rs`) exposes ", " tools")
        .or_else(|| claimed_number("exposes ", " tools"))
        .expect(
            "CLAUDE.md must contain 'exposes <N> tools'. If the wording changed, \
             update this test AND keep the count honest.",
        );

    assert_eq!(
        claimed, actual,
        "CLAUDE.md claims {claimed} MCP tools but mcp_cmd.rs declares {actual} \
         sentinel__* tool names. Update CLAUDE.md."
    );

    // Keep this guard non-trivial: assert HOOK_CMD_SRC is the real dispatcher
    // (cheap smoke that the include paths resolved, not empty).
    assert!(HOOK_CMD_SRC.contains("fn handle_pre_tool_use"));
}
