//! Commit Message Validator — PreToolUse hook
//!
//! Validates that `git commit` commands:
//!   1. Use conventional commit format (feat:, fix:, chore:, etc.)
//!   2. When running inside a configured project (e.g. Firefly Pro),
//!      include a Linear issue reference for one of the teams.
//!
//! Fires on PreToolUse for Bash tool calls containing `git commit`.
//! Blocks on malformed messages or missing Linear refs in gated projects.

use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use std::fs;
use std::path::PathBuf;

const VALID_PREFIXES: &[&str] = &[
    "feat", "fix", "chore", "docs", "refactor", "test",
    "style", "perf", "ci", "build", "revert",
];

fn extract_commit_message(command: &str) -> Option<String> {
    let heredoc_re = Regex::new(r#"(?s)<<'?EOF'?\s*\n(.*?)\n\s*EOF"#).ok()?;
    if let Some(caps) = heredoc_re.captures(command) {
        return Some(caps[1].trim().to_string());
    }

    let quoted_re = Regex::new(r#"-m\s+["']([^"']+)["']"#).ok()?;
    if let Some(caps) = quoted_re.captures(command) {
        return Some(caps[1].trim().to_string());
    }

    let unquoted_re = Regex::new(r#"-m\s+(\S+)"#).ok()?;
    if let Some(caps) = unquoted_re.captures(command) {
        return Some(caps[1].trim().to_string());
    }

    None
}

fn is_conventional(message: &str) -> bool {
    let subject = message.lines().next().unwrap_or(message).trim();
    if subject.is_empty() {
        return false;
    }
    let prefix_re = match Regex::new(r"^(\w+)(?:\([^)]*\))?:\s*.+") {
        Ok(re) => re,
        Err(_) => return false,
    };
    let caps = match prefix_re.captures(subject) {
        Some(c) => c,
        None => return false,
    };
    let prefix = caps[1].to_lowercase();
    VALID_PREFIXES.contains(&prefix.as_str())
}

fn projects_dir() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

fn extract_frontmatter(content: &str) -> Option<&str> {
    let trimmed = content.trim_start();
    let rest = trimmed.strip_prefix("---")?;
    let rest = rest.trim_start_matches('\n').trim_start_matches('\r');
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

fn frontmatter_tokens(frontmatter: &str, file_stem: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    tokens.push(file_stem.to_lowercase());

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name:") {
            tokens.push(rest.trim().trim_matches('"').trim_matches('\'').to_lowercase());
        } else if let Some(rest) = line.strip_prefix("doppler_project:") {
            tokens.push(rest.trim().trim_matches('"').trim_matches('\'').to_lowercase());
        } else if let Some(rest) = line.strip_prefix("aliases:") {
            let rest = rest.trim();
            if rest.starts_with('[') && rest.ends_with(']') {
                let inner = &rest[1..rest.len() - 1];
                for a in inner.split(',') {
                    let clean = a.trim().trim_matches('"').trim_matches('\'').to_lowercase();
                    if !clean.is_empty() {
                        tokens.push(clean);
                    }
                }
            }
        }
    }

    tokens.into_iter().filter(|t| t.len() >= 3).collect::<Vec<_>>()
}

fn frontmatter_prefixes(frontmatter: &str) -> Vec<String> {
    let mut prefixes = Vec::new();
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("issue_prefix:") {
            let p = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !p.is_empty() {
                prefixes.push(p);
            }
        } else if let Some(rest) = line.strip_prefix("- key:") {
            let p = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !p.is_empty() {
                prefixes.push(p);
            }
        } else if let Some(rest) = line.strip_prefix("key:") {
            let p = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !p.is_empty() {
                prefixes.push(p);
            }
        }
    }
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

fn cwd_matches_tokens(cwd: &str, tokens: &[String]) -> bool {
    // Match tokens against path segments (not substrings) so that
    // e.g. token "gary" does not falsely match the "garys" home dir
    // segment on Windows/macOS. Normalize both separators, lowercase,
    // and require exact segment equality.
    let segments: Vec<String> = cwd
        .replace('\\', "/")
        .to_lowercase()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    tokens.iter().any(|t| segments.iter().any(|s| s == t))
}

fn detect_prefixes_for_cwd(cwd: &str) -> Option<(String, Vec<String>)> {
    let dir = projects_dir()?;
    let entries = fs::read_dir(&dir).ok()?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path.file_stem()?.to_str()?.to_string();
        if stem.eq_ignore_ascii_case("MEMORY") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let Some(fm) = extract_frontmatter(&content) else {
            continue;
        };
        let tokens = frontmatter_tokens(fm, &stem);
        if !cwd_matches_tokens(cwd, &tokens) {
            continue;
        }
        let prefixes = frontmatter_prefixes(fm);
        if prefixes.is_empty() {
            continue;
        }
        return Some((stem, prefixes));
    }
    None
}

fn has_linear_ref(message: &str, prefixes: &[String]) -> bool {
    for prefix in prefixes {
        let pat = format!(r"(?i)\b{}-\d+\b", regex::escape(prefix));
        if let Ok(re) = Regex::new(&pat) {
            if re.is_match(message) {
                return true;
            }
        }
    }
    false
}

pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    if input.tool_name.as_deref() != Some("Bash") {
        return HookOutput::allow();
    }

    let command = input
        .tool_input
        .as_ref()
        .and_then(|ti| ti.get("command"))
        .and_then(|c| c.as_str())
        .unwrap_or("");

    let commit_re = match Regex::new(r"\bgit\s+commit\b") {
        Ok(re) => re,
        Err(_) => return HookOutput::allow(),
    };

    if !commit_re.is_match(command) {
        return HookOutput::allow();
    }

    if command.contains("--amend") {
        return HookOutput::allow();
    }

    let message = match extract_commit_message(command) {
        Some(m) => m,
        None => return HookOutput::allow(),
    };

    if !is_conventional(&message) {
        let valid_list = VALID_PREFIXES
            .iter()
            .map(|p| format!("`{p}:`"))
            .collect::<Vec<_>>()
            .join(", ");

        let reason = format!(
            "Commit message doesn't follow conventional format.\n\
             Got: \"{message}\"\n\
             Expected: <type>(<scope>): <description>\n\
             Valid types: {valid_list}\n\
             Examples: \"feat: add user auth\", \"fix(api): handle null response\", \"chore: bump deps\""
        );
        return HookOutput::block(reason);
    }

    if let Some(cwd) = input.cwd.as_deref() {
        if let Some((project, prefixes)) = detect_prefixes_for_cwd(cwd) {
            if !has_linear_ref(&message, &prefixes) {
                let list = prefixes
                    .iter()
                    .map(|p| format!("{p}-XXX"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let example = prefixes
                    .first()
                    .map(|p| format!("{p}-123"))
                    .unwrap_or_else(|| "FPCRM-123".into());
                let reason = format!(
                    "Commit is missing a Linear issue reference.\n\
                     Project `{project}` (cwd: {cwd}) requires one of: {list}.\n\
                     Add a ref to the subject or body (e.g. \"feat: add X ({example})\" \
                     or \"Ref {example}\" in the body).\n\
                     Got: \"{message}\""
                );
                return HookOutput::block(reason);
            }
        }
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_allows_non_bash() {
        let input = HookInput {
            tool_name: Some("Read".into()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_allows_non_git_command() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "cargo test"})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_allows_conventional_feat() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit -m \"feat: add user auth\""})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_allows_conventional_fix_with_scope() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(
                json!({"command": "git commit -m \"fix(api): handle null response\""}),
            ),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_allows_conventional_chore() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit -m 'chore: bump deps'"})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_blocks_non_conventional() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit -m \"updated the thing\""})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert_eq!(output.blocked, Some(true));
        assert!(output.reason.as_ref().unwrap().contains("conventional"));
    }

    #[test]
    fn test_blocks_no_prefix() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit -m \"add new feature\""})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_blocks_invalid_prefix() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit -m \"update: changed something\""})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_allows_amend() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit --amend -m \"whatever\""})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_allows_no_message_flag() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit"})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_allows_heredoc_conventional() {
        let cmd = "git commit -m \"$(cat <<'EOF'\nfeat: add hooks engine\n\nCo-Authored-By: Claude\nEOF\n)\"";
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": cmd})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_extract_message_double_quotes() {
        assert_eq!(
            extract_commit_message("git commit -m \"feat: test\""),
            Some("feat: test".into())
        );
    }

    #[test]
    fn test_extract_message_single_quotes() {
        assert_eq!(
            extract_commit_message("git commit -m 'fix: bug'"),
            Some("fix: bug".into())
        );
    }

    #[test]
    fn test_extract_message_heredoc_body_captures_full_body() {
        let cmd = "git commit -m \"$(cat <<'EOF'\nchore: bump version\n\nRef FPCRM-123\nEOF\n)\"";
        let extracted = extract_commit_message(cmd).unwrap();
        assert!(extracted.contains("chore: bump version"));
        assert!(extracted.contains("Ref FPCRM-123"));
    }

    #[test]
    fn test_is_conventional_all_types() {
        for prefix in VALID_PREFIXES {
            let msg = format!("{prefix}: some change");
            assert!(is_conventional(&msg), "Expected '{msg}' to be conventional");
        }
    }

    #[test]
    fn test_is_conventional_with_scope() {
        assert!(is_conventional("feat(hooks): add pre-compact"));
        assert!(is_conventional("fix(dashboard): layout issue"));
    }

    #[test]
    fn test_not_conventional() {
        assert!(!is_conventional("updated stuff"));
        assert!(!is_conventional("WIP"));
        assert!(!is_conventional(""));
        assert!(!is_conventional("random: not a valid type"));
    }

    #[test]
    fn test_allows_git_push() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git push origin main"})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_allows_no_tool_input() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_has_linear_ref_matches_subject() {
        let prefixes = vec!["FPCRM".to_string(), "FPFIELD".to_string()];
        assert!(has_linear_ref("feat: add X (FPCRM-361)", &prefixes));
        assert!(has_linear_ref("fix: stuff fpcrm-42", &prefixes));
        assert!(has_linear_ref("feat: thing (FPFIELD-9)", &prefixes));
    }

    #[test]
    fn test_has_linear_ref_matches_body() {
        let prefixes = vec!["FPCRM".to_string()];
        let msg = "feat: add X\n\nRef FPCRM-123\n\nCo-Authored-By: Claude";
        assert!(has_linear_ref(msg, &prefixes));
    }

    #[test]
    fn test_has_linear_ref_rejects_missing() {
        let prefixes = vec!["FPCRM".to_string()];
        assert!(!has_linear_ref("feat: add X", &prefixes));
        assert!(!has_linear_ref("feat: add FPCRM-abc", &prefixes));
        assert!(!has_linear_ref("feat: add XFPCRM-1", &prefixes));
    }

    #[test]
    fn test_extract_frontmatter_simple() {
        let md = "---\nname: test\nissue_prefix: ABC\n---\n\nbody";
        let fm = extract_frontmatter(md).unwrap();
        assert!(fm.contains("name: test"));
        assert!(fm.contains("issue_prefix: ABC"));
    }

    #[test]
    fn test_frontmatter_tokens_collects_aliases() {
        let fm = "name: firefly-pro\naliases: [\"firefly\", \"crm\", \"fir\"]\ndoppler_project: firefly-pro-crm";
        let tokens = frontmatter_tokens(fm, "firefly-pro");
        assert!(tokens.contains(&"firefly-pro".to_string()));
        assert!(tokens.contains(&"firefly".to_string()));
        assert!(tokens.contains(&"crm".to_string()));
        assert!(tokens.contains(&"fir".to_string()));
        assert!(tokens.contains(&"firefly-pro-crm".to_string()));
    }

    #[test]
    fn test_frontmatter_prefixes_collects_all_teams() {
        let fm = "issue_prefix: FPCRM\nlinear_teams:\n  - key: FPCRM\n  - key: FPFIELD\n  - key: FPROUTE";
        let prefixes = frontmatter_prefixes(fm);
        assert!(prefixes.contains(&"FPCRM".to_string()));
        assert!(prefixes.contains(&"FPFIELD".to_string()));
        assert!(prefixes.contains(&"FPROUTE".to_string()));
    }

    #[test]
    fn test_cwd_matches_tokens_path_segment() {
        let tokens = vec!["firefly".into(), "firefly-pro-crm".into()];
        assert!(cwd_matches_tokens(
            "C:/Users/garys/Documents/GitHub/firefly-pro-crm",
            &tokens
        ));
        assert!(cwd_matches_tokens("/home/g/firefly", &tokens));
        assert!(!cwd_matches_tokens("/home/g/some-other-repo", &tokens));
    }

    #[test]
    fn test_cwd_matches_tokens_windows_backslash() {
        let tokens = vec!["hookdeck-mcp-rust".into()];
        assert!(cwd_matches_tokens(
            r"C:\Users\garys\Documents\GitHub\hookdeck-mcp-rust",
            &tokens
        ));
        assert!(cwd_matches_tokens(
            r"C:\Users\garys\Documents\GitHub\hookdeck-mcp-rust\.claude\worktrees\foo",
            &tokens
        ));
    }

    #[test]
    fn test_cwd_matches_tokens_rejects_substring_of_segment() {
        // Regression: token "gary" must NOT match segment "garys" in the
        // home directory. Previously (substring match) every repo under
        // /users/garys/ falsely matched the `personal` project which has
        // "gary" as an alias.
        let tokens = vec!["gary".into(), "personal".into()];
        assert!(!cwd_matches_tokens(
            "C:/Users/garys/Documents/GitHub/hookdeck-mcp-rust",
            &tokens
        ));
        assert!(!cwd_matches_tokens(
            "/home/garys/repos/unrelated-project",
            &tokens
        ));
        // But an exact "personal" segment still matches.
        assert!(cwd_matches_tokens(
            "/home/garys/repos/personal",
            &tokens
        ));
    }

    #[test]
    fn test_cwd_matches_tokens_case_insensitive() {
        let tokens = vec!["firefly-pro".into()];
        assert!(cwd_matches_tokens(
            "C:/Users/garys/Documents/GitHub/Firefly-Pro",
            &tokens
        ));
    }
}
