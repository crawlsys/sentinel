//! MCP-tool classification.
//!
//! Decides whether an MCP tool name (`mcp__<server>__<method>`) represents
//! a write/exec capability that should be gated by the phase gate, or a
//! read-only operation that's always safe.
//!
//! **Fail-closed**: an unknown method suffix is classified as DANGEROUS.
//! New MCP servers don't get an automatic bypass — adding a server that
//! exposes a write tool requires either an explicit safe-list entry here
//! or a workflow-level allowance.

/// Method-suffix tokens that always indicate a read-only operation.
///
/// Match is exact (case-insensitive). The list intentionally includes a
/// few specific multi-word suffixes (e.g. `wait_for_selector`) that are
/// common Browserbase/CDP read paths — generic prefix matching alone would
/// over-match and a dedicated entry is more readable.
pub const SAFE_METHOD_SUFFIXES: &[&str] = &[
    "get",
    "list",
    "search",
    "read",
    "view",
    "show",
    "status",
    "info",
    "check",
    "verify",
    "validate",
    "count",
    "stats",
    "whoami",
    "viewer",
    "describe",
    "current_account",
    "list_accounts",
    "switch_account",
    "add_account",
    "remove_account",
    "download",
    "fetch",
    "discover",
    "screenshot",
    "pdf",
    "get_text",
    "is_visible",
    "wait",
    "wait_for_selector",
    "wait_for_navigation",
    "get_tabs",
    "list_instances",
    "mcp_restart_server",
    "sequentialthinking",
];

/// Method-suffix prefixes that always indicate a read-only operation.
/// E.g. `get_workflow_progress`, `list_skills`, `search_memory`, etc.
pub const SAFE_METHOD_PREFIXES: &[&str] = &[
    "get_", "list_", "search_", "read_", "check_", "resolve_", "verify_",
];

/// Classify an MCP tool as dangerous (write/exec capability).
///
/// MCP tool names follow `mcp__<server>__<method>`. The method suffix
/// (text after the final `__`) is lowercased and checked against:
///
/// 1. `SAFE_METHOD_SUFFIXES` (exact match) — if present, the tool is
///    READ-ONLY and `is_dangerous_mcp_tool` returns `false`.
/// 2. `SAFE_METHOD_PREFIXES` (prefix match) — same.
/// 3. Otherwise, `true` (DANGEROUS).
///
/// The fall-through is the security boundary: unknown suffixes are
/// gated, not allowed. See module docs.
#[must_use]
pub fn is_dangerous_mcp_tool(tool_name: &str) -> bool {
    let suffix = tool_name
        .rsplit("__")
        .next()
        .unwrap_or(tool_name)
        .to_lowercase();

    if SAFE_METHOD_SUFFIXES.contains(&suffix.as_str()) {
        return false;
    }
    if SAFE_METHOD_PREFIXES.iter().any(|p| suffix.starts_with(p)) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_exact_suffixes() {
        for s in SAFE_METHOD_SUFFIXES {
            let tool = format!("mcp__example__{s}");
            assert!(
                !is_dangerous_mcp_tool(&tool),
                "expected mcp__example__{s} to be read-only",
            );
        }
    }

    #[test]
    fn read_only_prefixed_suffixes() {
        // Test each prefix against an arbitrary suffix.
        for p in SAFE_METHOD_PREFIXES {
            let tool = format!("mcp__example__{p}whatever_thing");
            assert!(
                !is_dangerous_mcp_tool(&tool),
                "expected mcp__example__{p}whatever_thing to be read-only",
            );
        }
    }

    #[test]
    fn case_insensitive_suffix_match() {
        // Original implementation lowercases before comparing.
        assert!(!is_dangerous_mcp_tool("mcp__example__GET"));
        assert!(!is_dangerous_mcp_tool("mcp__example__List"));
    }

    #[test]
    fn unknown_suffix_is_dangerous_fail_closed() {
        assert!(is_dangerous_mcp_tool("mcp__example__create"));
        assert!(is_dangerous_mcp_tool("mcp__example__delete"));
        assert!(is_dangerous_mcp_tool("mcp__example__send"));
        assert!(is_dangerous_mcp_tool("mcp__example__execute"));
        assert!(is_dangerous_mcp_tool("mcp__example__novel_method"));
    }

    #[test]
    fn handles_tool_name_without_double_underscore() {
        // Defensive: a non-MCP tool name without `__` falls through to the
        // whole-string check. The string is treated as the suffix.
        assert!(!is_dangerous_mcp_tool("get"));
        assert!(is_dangerous_mcp_tool("write"));
    }

    #[test]
    fn handles_only_mcp_prefix() {
        // `mcp__server` with no method — suffix becomes "server".
        // "server" isn't in either list → dangerous (fail-closed).
        assert!(is_dangerous_mcp_tool("mcp__server"));
    }
}
