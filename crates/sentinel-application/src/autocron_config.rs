//! Declarative autocron rule config.
//!
//! Loads the shipped ruleset (`config/autocron-defaults.toml`, baked in via
//! `include_str!`) plus an optional operator overlay at
//! `~/.claude/sentinel/config/autocron.toml`. The autocron hook
//! ([`crate`]'s sibling in sentinel-application) matches the current tool call
//! against these rules and injects a `CronCreate`/loop suggestion.
//!
//! Design notes:
//! - **Fail-open for overlays only**: the shipped defaults are compiled in and
//!   always parse; a missing, unreadable, or corrupt operator overlay is ignored
//!   (defaults are kept), never a panic. Path resolution itself fails closed so
//!   Sentinel never treats the current working directory as a fake home.
//! - **Merge-by-id**: an overlay rule whose `id` matches a shipped rule fully
//!   replaces it (whole-rule replace, no partial-field inheritance — keeps the
//!   merge trivial and predictable). A new `id` is appended. `enabled = false`
//!   kills a rule.
//! - Mirrors the `capability_router.rs` precedent (`include_str!` default +
//!   `config_dir()` overlay).

use serde::Deserialize;

/// The shipped autocron ruleset, compiled into the engine.
pub const SHIPPED_AUTOCRON_DEFAULTS: &str = include_str!("../../../config/autocron-defaults.toml");

/// `~/.claude/sentinel/config` — the operator config dir. Resolved directly via
/// the application-layer Claude dir resolver so missing home / empty override
/// fails closed instead of falling back to CWD.
fn config_dir() -> std::path::PathBuf {
    crate::paths::claude_dir().join("sentinel").join("config")
}

/// One declarative event→cron/loop rule. See `config/autocron-defaults.toml`
/// for the field reference; defaults here mirror that doc.
#[derive(Debug, Clone, Deserialize)]
pub struct AutocronRule {
    /// Stable id — dedupe-ledger prefix + operator override key.
    pub id: String,
    /// Exact tool name this rule gates on (`Bash`, `TaskUpdate`, an MCP name…).
    pub tool: String,
    /// Regex matched against the haystack (Bash→command; else flattened
    /// `tool_input` JSON). Named captures are usable in `prompt_template`.
    #[serde(rename = "match")]
    pub match_re: String,
    /// Optional classifier hint: `"regex"` (default) or `"git_push_branch"`.
    #[serde(default)]
    pub match_kind: Option<String>,
    /// If this regex matches the haystack, the rule is skipped.
    #[serde(default)]
    pub exclude: Option<String>,
    /// Literal substrings that suppress the rule (cheap pre-regex guard).
    #[serde(default)]
    pub skip_tokens: Vec<String>,
    /// `"cron"` (default) or `"loop"`.
    #[serde(default = "default_action")]
    pub action: String,
    /// Cron expression (cron action only).
    #[serde(default)]
    pub interval: Option<String>,
    /// When true (default), the rendered prompt must self-delete on terminal.
    #[serde(default = "default_true")]
    pub terminal: bool,
    /// Stall backstop tick count rendered into the prompt; 0 = none.
    #[serde(default = "default_safety_cap")]
    pub safety_cap_ticks: u32,
    /// Named capture used as the dedupe value; falls back to `id`.
    #[serde(default)]
    pub dedupe_key: Option<String>,
    /// When true, prefix the injected text `[Sentinel-Authority]`.
    #[serde(default)]
    pub authority: bool,
    /// When false, the rule is dropped at load time.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The CronCreate/loop body. Supports `{capture}` + builtins.
    pub prompt_template: String,
}

fn default_action() -> String {
    "cron".to_string()
}
const fn default_true() -> bool {
    true
}
const fn default_safety_cap() -> u32 {
    20
}

/// Top-level TOML shape: `[[autocron]]` array of rules.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AutocronConfig {
    #[serde(default)]
    pub autocron: Vec<AutocronRule>,
}

/// Parse a TOML string into rules. Returns an empty vec on parse failure so a
/// corrupt overlay is a no-op (the shipped defaults always parse — a failure
/// there is a build-time error caught by the test below).
fn parse(s: &str) -> Vec<AutocronRule> {
    toml::from_str::<AutocronConfig>(s)
        .map(|c| c.autocron)
        .unwrap_or_default()
}

/// Merge `overlay` rules into `base` by id: a matching id replaces the base rule
/// in place (preserving order); a new id is appended.
fn merge_by_id(base: &mut Vec<AutocronRule>, overlay: Vec<AutocronRule>) {
    for rule in overlay {
        if let Some(existing) = base.iter_mut().find(|r| r.id == rule.id) {
            *existing = rule;
        } else {
            base.push(rule);
        }
    }
}

/// Load the effective ruleset: shipped defaults + operator overlay
/// (`~/.claude/sentinel/config/autocron.toml`), merged by id, `enabled` filtered.
/// Fail-open: a missing/unreadable/corrupt overlay leaves the defaults intact.
#[must_use]
pub fn load() -> Vec<AutocronRule> {
    let mut rules = parse(SHIPPED_AUTOCRON_DEFAULTS);
    let path = config_dir().join("autocron.toml");
    if let Ok(content) = std::fs::read_to_string(&path) {
        let overlay = parse(&content);
        if !overlay.is_empty() {
            merge_by_id(&mut rules, overlay);
        }
    }
    rules.into_iter().filter(|r| r.enabled).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_defaults_parse_and_are_nonempty() {
        let rules = parse(SHIPPED_AUTOCRON_DEFAULTS);
        assert!(
            !rules.is_empty(),
            "shipped autocron-defaults.toml must parse into rules"
        );
        // The six designed rules must all be present.
        for id in [
            "pr_created",
            "linear_state_change",
            "deploy_started",
            "pr_branch_push",
            "task_started_stale_watch",
            "ci_run_watch",
        ] {
            assert!(
                rules.iter().any(|r| r.id == id),
                "missing shipped rule {id}"
            );
        }
    }

    #[test]
    fn defaults_have_expected_fields() {
        let rules = parse(SHIPPED_AUTOCRON_DEFAULTS);
        let deploy = rules.iter().find(|r| r.id == "deploy_started").unwrap();
        assert!(deploy.authority, "deploy rule must be authoritative");
        assert_eq!(deploy.tool, "Bash");
        assert_eq!(deploy.safety_cap_ticks, 20);

        let push = rules.iter().find(|r| r.id == "pr_branch_push").unwrap();
        assert_eq!(push.match_kind.as_deref(), Some("git_push_branch"));

        let linear = rules
            .iter()
            .find(|r| r.id == "linear_state_change")
            .unwrap();
        assert!(!linear.terminal, "linear lifecycle rule is perpetual");
        assert_eq!(linear.safety_cap_ticks, 0);
    }

    #[test]
    fn shipped_prompts_include_cron_failure_circuit_breaker() {
        let rules = parse(SHIPPED_AUTOCRON_DEFAULTS);
        for rule in rules {
            assert!(
                rule.prompt_template.contains("CRON CIRCUIT BREAKER"),
                "{} is missing the cron circuit-breaker preamble",
                rule.id
            );
            assert!(
                rule.prompt_template.contains("3 consecutive failures"),
                "{} is missing the failure budget",
                rule.id
            );
            assert!(
                rule.prompt_template.contains("CronDelete"),
                "{} must be able to self-delete after repeated failures",
                rule.id
            );
        }
    }

    #[test]
    fn overlay_merge_by_id_replaces_and_appends() {
        let mut base = parse(SHIPPED_AUTOCRON_DEFAULTS);
        let base_count = base.len();
        let overlay = parse(
            r#"
            [[autocron]]
            id = "pr_created"
            tool = "Bash"
            match = 'gh pr create'
            interval = "*/9 * * * *"
            prompt_template = "replaced"

            [[autocron]]
            id = "my_custom_rule"
            tool = "Bash"
            match = 'my-deploy'
            interval = "*/4 * * * *"
            prompt_template = "custom"
            "#,
        );
        merge_by_id(&mut base, overlay);
        // Same id replaced (count grows by exactly the one new rule).
        assert_eq!(base.len(), base_count + 1);
        let pr = base.iter().find(|r| r.id == "pr_created").unwrap();
        assert_eq!(pr.interval.as_deref(), Some("*/9 * * * *"));
        assert_eq!(pr.prompt_template, "replaced");
        assert!(base.iter().any(|r| r.id == "my_custom_rule"));
    }

    #[test]
    fn corrupt_overlay_is_ignored() {
        // parse() of garbage yields empty → defaults survive (fail-open).
        assert!(parse("this is not valid toml {{{").is_empty());
    }

    #[test]
    fn disabled_rule_filtered() {
        let mut rules = parse(SHIPPED_AUTOCRON_DEFAULTS);
        let overlay = parse(
            r#"
            [[autocron]]
            id = "deploy_started"
            tool = "Bash"
            match = 'wrangler deploy'
            enabled = false
            prompt_template = "x"
            "#,
        );
        merge_by_id(&mut rules, overlay);
        let effective: Vec<_> = rules.into_iter().filter(|r| r.enabled).collect();
        assert!(
            !effective.iter().any(|r| r.id == "deploy_started"),
            "enabled=false overlay must remove the rule"
        );
    }
}
