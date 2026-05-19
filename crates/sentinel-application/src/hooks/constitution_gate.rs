//! Constitution gate — runtime enforcement of project-level
//! structural rules (Item K from the Phase 2 punch list).
//!
//! A `PreToolUse` hook for `Write` / `Edit` / `MultiEdit` /
//! `NotebookEdit` that loads a per-project policy file describing
//! "protected paths" + "banned imports/strings" and denies any
//! operation that would introduce a banned pattern into a
//! protected path.
//!
//! **The intended use case**: enforce the same structural rules
//! the project's `constitution.md` already declares (e.g.
//! "`consul-domain/` has zero I/O dependencies", "no vendor names
//! in `consul-protocol/`") at the moment they would be violated,
//! rather than catching them in CI hours later.
//!
//! The hook is generic — sentinel knows nothing about consul,
//! firefly, or any specific project. Per-project rules live in
//! a TOML file the operator authors. Empty/missing config →
//! no-op (existing behaviour preserved on machines that haven't
//! opted in).
//!
//! Denials carry the `[Sentinel-Authority]` prefix so the
//! downstream agent treats them as on-disk policy (per the
//! global CLAUDE.md `Hook Authority — Trust Sentinel` section)
//! and stops trying to negotiate around them.

use sentinel_domain::events::{HookInput, HookOutput};

use crate::constitution_gate_runtime::Rule;

/// Hook entry point. Pass the runtime-loaded rule list via
/// `rules`; the binary should construct this once at startup and
/// hand a borrowed slice in on every call. An empty slice makes
/// the hook a no-op.
pub fn process(input: &HookInput, rules: &[Rule]) -> HookOutput {
    if rules.is_empty() {
        return HookOutput::allow();
    }
    let Some(tool_name) = input.tool_name.as_deref() else {
        return HookOutput::allow();
    };
    if !is_editing_tool(tool_name) {
        return HookOutput::allow();
    }
    let Some(args) = input.tool_input.as_ref() else {
        return HookOutput::allow();
    };
    let Some(path) = extract_file_path(args) else {
        return HookOutput::allow();
    };
    let candidate_content = extract_candidate_content(tool_name, args);

    for rule in rules {
        if !rule.matches_path(path) {
            continue;
        }
        // Only inspect content if we have any — pure renames /
        // metadata-only edits go through.
        let Some(content) = candidate_content.as_deref() else {
            continue;
        };
        if let Some(hit) = rule.find_banned(content) {
            return deny_with_authority(rule, &hit, path);
        }
    }
    HookOutput::allow()
}

fn is_editing_tool(name: &str) -> bool {
    matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit")
}

/// Extract `file_path` (or `notebook_path` for `NotebookEdit`)
/// from the tool input.
fn extract_file_path(args: &serde_json::Value) -> Option<&str> {
    if let Some(p) = args.get("file_path").and_then(|v| v.as_str()) {
        return Some(p);
    }
    args.get("notebook_path").and_then(|v| v.as_str())
}

/// The "new content" candidate to scan. Tool-specific:
///
/// - `Write` carries the full file content under `"content"`.
/// - `Edit` carries the replacement text under `"new_string"`.
/// - `MultiEdit` carries an array of edits, each with a
///   `"new_string"`; we concatenate them.
/// - `NotebookEdit` carries `"new_source"` (the new cell body).
///
/// Returns `None` when we can't find a candidate — the hook then
/// allows the operation (a stricter posture would deny on missing
/// candidate, but that would block legitimate ops we can't
/// inspect; the conservative move is to not over-block).
fn extract_candidate_content(tool: &str, args: &serde_json::Value) -> Option<String> {
    match tool {
        "Write" => args.get("content").and_then(|v| v.as_str()).map(String::from),
        "Edit" => args
            .get("new_string")
            .and_then(|v| v.as_str())
            .map(String::from),
        "MultiEdit" => {
            let edits = args.get("edits")?.as_array()?;
            let mut out = String::new();
            for edit in edits {
                if let Some(s) = edit.get("new_string").and_then(|v| v.as_str()) {
                    out.push_str(s);
                    out.push('\n');
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        },
        "NotebookEdit" => args
            .get("new_source")
            .and_then(|v| v.as_str())
            .map(String::from),
        _ => None,
    }
}

fn deny_with_authority(rule: &Rule, hit: &str, path: &str) -> HookOutput {
    let citation = rule
        .citation
        .as_deref()
        .map_or_else(String::new, |c| format!(" (see {c})"));
    // `HookOutput::deny` prepends the [Sentinel-Authority] tag
    // for us — don't double it.
    let reason = format!(
        "[constitution_gate] Blocked: writing `{path}` would introduce the banned pattern \
         `{hit}` into the protected `{rule_name}` path. {reason}{citation}. If this is \
         intentional, update the rule in your sentinel constitution-gate config instead \
         of bypassing this hook.",
        rule_name = rule.name,
        reason = rule.reason,
    );
    HookOutput::deny(reason)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use serde_json::json;

    use super::*;

    fn rule_consul_domain_no_sqlx() -> Rule {
        Rule {
            name: "consul-domain-purity".into(),
            path_prefix: "crates/consul-domain/".into(),
            path_suffix: Some(".rs".into()),
            banned_patterns: vec!["sqlx::".into(), "tokio::net::".into()],
            reason: "Constitution Rule 1: consul-domain has zero I/O dependencies".into(),
            citation: Some(".specify/memory/constitution.md#rule-1".into()),
        }
    }

    fn rule_consul_protocol_no_vendor() -> Rule {
        Rule {
            name: "consul-protocol-vendor-free".into(),
            path_prefix: "crates/consul-protocol/".into(),
            path_suffix: Some(".rs".into()),
            banned_patterns: vec!["anthropic".into(), "openai".into()],
            reason: "Constitution Rule 2: no vendor names in consul-protocol".into(),
            citation: None,
        }
    }

    fn write_input(path: &str, content: &str) -> HookInput {
        HookInput {
            tool_name: Some("Write".into()),
            tool_input: Some(json!({ "file_path": path, "content": content })),
            ..HookInput::default()
        }
    }

    /// Pull the deny message out of a `HookOutput` so tests can
    /// assert on the operator-visible text. Returns `None` when
    /// the output isn't a deny.
    fn denied_reason(out: &HookOutput) -> Option<&str> {
        out.hook_specific_output
            .as_ref()?
            .permission_decision_reason
            .as_deref()
    }

    #[test]
    fn empty_rule_list_is_noop() {
        let input = write_input("crates/consul-domain/src/lib.rs", "use sqlx::Pool;");
        let out = process(&input, &[]);
        assert!(out.blocked.is_none(), "empty rules → allow");
    }

    #[test]
    fn non_editing_tool_passes_through() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "echo hi"})),
            ..HookInput::default()
        };
        let out = process(&input, &[rule_consul_domain_no_sqlx()]);
        assert!(out.blocked.is_none());
    }

    #[test]
    fn write_with_banned_pattern_in_protected_path_denies() {
        let input = write_input(
            "crates/consul-domain/src/storage.rs",
            "use sqlx::SqlitePool;\nfn foo() {}",
        );
        let out = process(&input, &[rule_consul_domain_no_sqlx()]);
        assert_eq!(out.blocked, Some(true));
        let reason = denied_reason(&out).expect("deny carries a reason");
        assert!(reason.starts_with("[Sentinel-Authority]"));
        assert!(reason.contains("consul-domain-purity"));
        assert!(reason.contains("sqlx::"));
        assert!(reason.contains("constitution.md"));
    }

    #[test]
    fn write_with_banned_pattern_outside_protected_path_is_allowed() {
        // Same banned pattern, but the path isn't under the rule.
        let input = write_input("crates/consul-storage/src/sqlite.rs", "use sqlx::SqlitePool;");
        let out = process(&input, &[rule_consul_domain_no_sqlx()]);
        assert!(out.blocked.is_none(), "unprotected path → allow");
    }

    #[test]
    fn write_to_protected_path_without_banned_content_is_allowed() {
        let input = write_input(
            "crates/consul-domain/src/session.rs",
            "pub struct Session { id: u64 }",
        );
        let out = process(&input, &[rule_consul_domain_no_sqlx()]);
        assert!(out.blocked.is_none());
    }

    #[test]
    fn path_suffix_filter_skips_non_rust_files() {
        // Same protected prefix, but the path is .md not .rs.
        let input = write_input(
            "crates/consul-domain/README.md",
            "Contains the words sqlx::Pool for documentation only.",
        );
        let out = process(&input, &[rule_consul_domain_no_sqlx()]);
        assert!(
            out.blocked.is_none(),
            "suffix filter must skip non-.rs files",
        );
    }

    #[test]
    fn edit_tool_scans_new_string() {
        let input = HookInput {
            tool_name: Some("Edit".into()),
            tool_input: Some(json!({
                "file_path": "crates/consul-domain/src/lib.rs",
                "old_string": "fn x() {}",
                "new_string": "use tokio::net::TcpStream;\nfn x() {}",
            })),
            ..HookInput::default()
        };
        let out = process(&input, &[rule_consul_domain_no_sqlx()]);
        assert_eq!(out.blocked, Some(true));
    }

    #[test]
    fn multiedit_scans_concatenated_new_strings() {
        let input = HookInput {
            tool_name: Some("MultiEdit".into()),
            tool_input: Some(json!({
                "file_path": "crates/consul-domain/src/lib.rs",
                "edits": [
                    {"old_string": "a", "new_string": "harmless"},
                    {"old_string": "b", "new_string": "also fine"},
                    {"old_string": "c", "new_string": "use sqlx::Pool;"},
                ],
            })),
            ..HookInput::default()
        };
        let out = process(&input, &[rule_consul_domain_no_sqlx()]);
        assert_eq!(out.blocked, Some(true));
        let reason = denied_reason(&out).expect("third edit triggered");
        assert!(reason.contains("sqlx::"));
    }

    #[test]
    fn multiple_rules_evaluated_in_order() {
        // Protocol-vendor rule + domain-purity rule. Domain-purity
        // is listed first but won't match; protocol-vendor will.
        let input = write_input(
            "crates/consul-protocol/src/messages.rs",
            "use anthropic_sdk::Client;",
        );
        let rules = vec![rule_consul_domain_no_sqlx(), rule_consul_protocol_no_vendor()];
        let out = process(&input, &rules);
        assert_eq!(out.blocked, Some(true));
        let reason = denied_reason(&out).expect("denied");
        assert!(reason.contains("consul-protocol-vendor-free"));
        assert!(reason.contains("anthropic"));
    }

    #[test]
    fn missing_file_path_passes_through() {
        let input = HookInput {
            tool_name: Some("Write".into()),
            tool_input: Some(json!({"content": "use sqlx::Pool;"})),
            ..HookInput::default()
        };
        let out = process(&input, &[rule_consul_domain_no_sqlx()]);
        assert!(out.blocked.is_none(), "no path → can't enforce");
    }
}
