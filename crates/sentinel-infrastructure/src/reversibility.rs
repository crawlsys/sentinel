//! Layered TOML-driven `ReversibilityClassifierPort` adapter (A6 Phase 3b).
//!
//! Implements the four-layer evaluation scheme from
//! `docs/a6-reversibility-graded-tripwires.md` §3:
//!
//! 1. **Built-in tool defaults.** Hardcoded in [`builtin_class`] —
//!    `Read`/`Glob`/`Grep`/`TaskList` are `TriviallyReversible`,
//!    `Edit`/`Write`/`TaskCreate`/`TaskUpdate` are `ReversibleWithEffort`,
//!    `Bash` delegates to Layer 3, `mcp__*` delegates to Layer 2,
//!    everything else falls back conservatively to `Irreversible`.
//! 2. **Per-MCP-tool defaults** loaded from `[mcp.<server>] <tool> = "Class"`
//!    TOML.
//! 3. **Per-input Bash patterns** loaded from `[[bash.pattern]] match =
//!    "<regex>" class = "Class"` TOML arrays. First-match wins (TOML order
//!    preserved by the `toml` crate via array-of-tables).
//! 4. **Operator overrides** loaded from `[overrides] "<tool_name>" =
//!    "Class"` TOML. Exact `tool_name` match, highest priority.
//!
//! Loader merges a *defaults* TOML (shipped with sentinel) and an optional
//! *overrides* TOML (operator-managed); MCP entries union (overrides win on
//! conflict), Bash patterns concatenate (overrides patterns evaluated AFTER
//! defaults so operators can add catch-all rules without losing
//! the shipped catastrophic-pattern coverage), `[overrides]` table merges
//! (overrides file wins).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use sentinel_domain::ports::ReversibilityClassifierPort;
use sentinel_domain::ReversibilityClass;

// ---------------------------------------------------------------------------
// TOML schema
// ---------------------------------------------------------------------------

/// Top-level TOML structure. Both the defaults file and the operator
/// overrides file share this schema; the loader merges them.
#[derive(Debug, Default, Deserialize)]
pub struct ReversibilityConfigToml {
    /// `[mcp.<server>] <tool> = "Class"` — per-MCP-tool defaults.
    #[serde(default)]
    pub mcp: HashMap<String, HashMap<String, ReversibilityClass>>,

    /// `[bash] pattern = [[ ... ]]` — ordered Bash command pattern rules.
    #[serde(default)]
    pub bash: BashRulesToml,

    /// `[overrides] "<tool_name>" = "Class"` — operator overrides.
    #[serde(default)]
    pub overrides: HashMap<String, ReversibilityClass>,
}

#[derive(Debug, Default, Deserialize)]
pub struct BashRulesToml {
    /// `[[bash.pattern]] match = "<regex>" class = "..."` — first-match
    /// wins. Order preserved by the `toml` crate's array-of-tables.
    #[serde(default)]
    pub pattern: Vec<BashPatternRuleToml>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BashPatternRuleToml {
    /// Regular expression matched against the Bash `command` string.
    /// Uses the `regex` crate's syntax (no look-around; PCRE-incompatible).
    #[serde(rename = "match")]
    pub pattern: String,
    /// Class assigned when `pattern` matches.
    pub class: ReversibilityClass,
}

// ---------------------------------------------------------------------------
// Compiled classifier
// ---------------------------------------------------------------------------

/// Four-layer reversibility classifier built from one or more
/// [`ReversibilityConfigToml`] sources. Patterns are pre-compiled at
/// construction so the hot path (a `classify` call) only does table
/// lookups + regex matches.
#[derive(Debug)]
pub struct LayeredReversibilityClassifier {
    /// `mcp[<server>][<tool>] = class` — Layer 2.
    mcp: HashMap<String, HashMap<String, ReversibilityClass>>,
    /// Compiled Layer-3 Bash patterns, evaluation order preserved.
    bash_patterns: Vec<(Regex, ReversibilityClass)>,
    /// `overrides[<tool_name>] = class` — Layer 4.
    overrides: HashMap<String, ReversibilityClass>,
}

impl LayeredReversibilityClassifier {
    /// Construct an empty classifier — every tool falls through Layers 2
    /// and 3 to the conservative Layer-1 fallback (`Irreversible` for
    /// unknown MCP tools, `ReversibleWithEffort` for unknown Bash
    /// commands). Useful for tests that want to exercise only Layer 1.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            mcp: HashMap::new(),
            bash_patterns: Vec::new(),
            overrides: HashMap::new(),
        }
    }

    /// Construct from the sentinel-shipped `config/reversibility-defaults.toml`
    /// (embedded at compile time via `include_str!`). The production caller
    /// uses this in the hook dispatcher; tests prefer
    /// [`StaticReversibilityClassifier`](crate) or [`Self::from_str`] with
    /// canned TOML.
    ///
    /// Phase 4a does not yet layer operator overrides from
    /// `~/.claude/sentinel/config/reversibility.toml` — that wiring lands
    /// in a follow-up phase once the operator-overrides storage location
    /// is settled.
    pub fn with_shipped_defaults() -> Result<Self> {
        const SHIPPED_DEFAULTS: &str =
            include_str!("../../../config/reversibility-defaults.toml");
        Self::from_str(SHIPPED_DEFAULTS, None)
    }

    /// Load from a defaults TOML string (shipped with sentinel) plus an
    /// optional overrides TOML string (operator-managed). The two
    /// sources are merged per the rules in the module docstring.
    pub fn from_str(defaults_toml: &str, overrides_toml: Option<&str>) -> Result<Self> {
        let defaults: ReversibilityConfigToml = toml::from_str(defaults_toml)
            .context("failed to parse reversibility defaults TOML")?;
        let overrides = overrides_toml
            .map(|s| {
                toml::from_str::<ReversibilityConfigToml>(s)
                    .context("failed to parse reversibility overrides TOML")
            })
            .transpose()?
            .unwrap_or_default();
        Self::build(defaults, overrides)
    }

    /// Load from paths. The defaults path must exist; the overrides path
    /// is optional and silently skipped if absent.
    pub fn load_from_paths(
        defaults_path: &Path,
        overrides_path: Option<&Path>,
    ) -> Result<Self> {
        let defaults_str = std::fs::read_to_string(defaults_path).with_context(|| {
            format!(
                "failed to read reversibility defaults from {}",
                defaults_path.display()
            )
        })?;
        let overrides_str = match overrides_path {
            Some(p) if p.exists() => Some(std::fs::read_to_string(p).with_context(|| {
                format!("failed to read reversibility overrides from {}", p.display())
            })?),
            _ => None,
        };
        Self::from_str(&defaults_str, overrides_str.as_deref())
    }

    fn build(
        defaults: ReversibilityConfigToml,
        overrides_config: ReversibilityConfigToml,
    ) -> Result<Self> {
        // Layer 2: MCP entries. Union; overrides win on conflict.
        let mut mcp = defaults.mcp;
        for (server, tools) in overrides_config.mcp {
            mcp.entry(server).or_default().extend(tools);
        }

        // Layer 3: Bash patterns. Concatenate; overrides AFTER defaults so
        // operators can add catch-all rules without losing shipped
        // catastrophic-pattern coverage at the front of the list.
        let mut bash_patterns = Vec::with_capacity(
            defaults.bash.pattern.len() + overrides_config.bash.pattern.len(),
        );
        for rule in defaults
            .bash
            .pattern
            .into_iter()
            .chain(overrides_config.bash.pattern.into_iter())
        {
            let regex = Regex::new(&rule.pattern).with_context(|| {
                format!("failed to compile bash pattern regex `{}`", rule.pattern)
            })?;
            bash_patterns.push((regex, rule.class));
        }

        // Layer 4: operator overrides. Defaults-table merges; overrides
        // file wins on conflict.
        let mut overrides = defaults.overrides;
        overrides.extend(overrides_config.overrides);

        Ok(Self {
            mcp,
            bash_patterns,
            overrides,
        })
    }

    fn classify_bash(&self, tool_input: &Value) -> ReversibilityClass {
        let cmd = tool_input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        for (regex, class) in &self.bash_patterns {
            if regex.is_match(cmd) {
                return *class;
            }
        }
        // Bash default when no pattern matches: ReversibleWithEffort.
        // Conservative — assume any unrecognized command is mutating.
        ReversibilityClass::ReversibleWithEffort
    }

    fn classify_mcp(&self, tool_name: &str) -> ReversibilityClass {
        // tool_name shape: "mcp__<server>__<tool...>"
        let stripped = match tool_name.strip_prefix("mcp__") {
            Some(s) => s,
            None => return ReversibilityClass::Irreversible,
        };
        let (server, tool) = match stripped.split_once("__") {
            Some(pair) => pair,
            None => return ReversibilityClass::Irreversible,
        };
        self.mcp
            .get(server)
            .and_then(|tools| tools.get(tool).copied())
            .unwrap_or(ReversibilityClass::Irreversible)
    }
}

/// Built-in (Layer 1) class dispatch for non-MCP, non-Bash tools.
/// Returns `None` when the tool name should be handled by a deeper
/// layer (MCP via Layer 2, Bash via Layer 3) or by the unknown-tool
/// fallback.
///
/// Coverage is exhaustive for the harness tools shipped by Claude Code
/// and the Vulcan/FleetView runtime — anything not listed here falls
/// through to the conservative Irreversible default, which strands the
/// agent at A3 because `dry_run_then_commit` demands
/// `_intent`/`_reasoning`/`_expected_effect` on tool calls that
/// pure-read or only mutate in-conversation state. Observed deadlock
/// pre-fix: skill_gate requires `Skill(...)` before `Bash`, but
/// `dry_run_then_commit` refused `Skill(...)` (and `ToolSearch`,
/// `AskUserQuestion`) as Irreversible, blocking the agent from
/// loading the skill the gate demanded. Keep this list comprehensive;
/// mirror new entries in
/// `crates/sentinel-application/src/hooks/skill_gate.rs` if they
/// should also bypass that gate's load requirement.
fn builtin_class(tool_name: &str) -> Option<ReversibilityClass> {
    match tool_name {
        // --- TriviallyReversible: pure reads, UI prompts, schema
        // lookups, idempotent harness queries. Zero state change
        // observable outside the agent's own conversation buffer. ---
        "Read"
        | "Glob"
        | "Grep"
        | "WebFetch"
        | "WebSearch"
        // Task introspection (writes covered below)
        | "TaskList"
        | "TaskGet"
        | "TaskOutput"
        // UI-only prompts — the agent asks the operator a question;
        // no tool action commits until the operator answers.
        | "AskUserQuestion"
        // Skill / tool-schema loaders — load markdown / JSON Schema
        // into the agent's context, no external side effect.
        | "Skill"
        | "ToolSearch"
        // Onboarding share-link check — read-only in `check` mode
        // (the default); operator-initiated create/update/delete
        // modes re-classify at the explicit call site if needed.
        | "ShareOnboardingGuide"
        // Process introspection — Monitor streams stdout lines from
        // an already-spawned background command, doesn't start work.
        | "Monitor"
        // Cron introspection (writes covered below)
        | "CronList"
        // LSP query surface — reads symbol tables / hovers /
        // definitions. LSP-driven edits go through Edit/Write.
        | "LSP" => Some(ReversibilityClass::TriviallyReversible),

        // --- ReversibleWithEffort: in-conversation or local-tree
        // mutations the operator can undo with a known recovery path
        // (delete the task, kill the agent, delete the worktree, etc.).
        // None of these reach external services or shared infrastructure. ---
        "Edit"
        | "Write"
        | "NotebookEdit"
        // Task lifecycle writes — confined to the harness task store.
        | "TaskCreate"
        | "TaskUpdate"
        | "TaskStop"
        // Mode transitions — change the agent's permission state for
        // the rest of the session. Revertible by entering/exiting the
        // opposite mode.
        | "EnterPlanMode"
        | "ExitPlanMode"
        // Worktree lifecycle — local-disk only; ExitWorktree(remove)
        // is the recovery for EnterWorktree.
        | "EnterWorktree"
        | "ExitWorktree"
        // Agent / team orchestration — spawned work lives in the
        // local task graph; recovery is TaskStop / TeamDelete.
        | "Agent"
        | "TeamCreate"
        | "TeamDelete"
        // Inter-agent messaging — stays inside the FleetView fleet;
        // recovery is "agent ignores it" or restart the recipient.
        | "SendMessage"
        // Cron management — adding/removing scheduled work. Side
        // effects of FIRING a cron go through their own tool calls
        // and re-classify at that point.
        | "CronCreate"
        | "CronDelete"
        // Wake-up scheduler (Loop dynamic mode) — schedules the next
        // re-entry, doesn't act externally.
        | "ScheduleWakeup"
        // Local notification surfaces — push to the operator's own
        // device / channel. No external commitment to anyone else.
        | "PushNotification"
        | "RemoteTrigger" => Some(ReversibilityClass::ReversibleWithEffort),

        _ => None,
    }
}

impl ReversibilityClassifierPort for LayeredReversibilityClassifier {
    fn classify(&self, tool_name: &str, tool_input: &Value) -> ReversibilityClass {
        // Layer 4 — operator overrides win above all (highest priority).
        if let Some(class) = self.overrides.get(tool_name) {
            return *class;
        }

        // Layer 3 (command patterns) and Layer 2 (MCP) are dispatched from
        // Layer 1. `PowerShell` is the Windows-native sibling of `Bash` â its
        // tool_input carries the same `command` field, so it shares the exact
        // same Layer-3 pattern list. Without this arm PowerShell falls through
        // to the conservative Irreversible default, which strands every
        // PowerShell call at the A3 dry-run gate: `dry_run_then_commit`
        // demands `_intent`/`_reasoning`/`_expected_effect` keys, but the
        // PowerShell tool schema rejects unknown params (`additionalProperties:
        // false`) â an unbreakable deadlock. Same failure class the
        // `builtin_class` doc comment describes for Skill/ToolSearch/AskUserQuestion.
        match tool_name {
            "Bash" | "PowerShell" => self.classify_bash(tool_input),
            t if t.starts_with("mcp__") => self.classify_mcp(t),
            other => builtin_class(other).unwrap_or(ReversibilityClass::Irreversible),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn no_input() -> Value {
        json!({})
    }

    fn bash_cmd(s: &str) -> Value {
        json!({ "command": s })
    }

    // ---- Layer 1: built-in tool defaults ----

    #[test]
    fn layer1_read_glob_grep_are_trivially_reversible() {
        let c = LayeredReversibilityClassifier::empty();
        for t in ["Read", "Glob", "Grep", "TaskList", "WebFetch", "WebSearch"] {
            assert_eq!(
                c.classify(t, &no_input()),
                ReversibilityClass::TriviallyReversible,
                "{t} should be TriviallyReversible"
            );
        }
    }

    #[test]
    fn layer1_edit_write_task_mutations_are_reversible_with_effort() {
        let c = LayeredReversibilityClassifier::empty();
        for t in ["Edit", "Write", "TaskCreate", "TaskUpdate"] {
            assert_eq!(
                c.classify(t, &no_input()),
                ReversibilityClass::ReversibleWithEffort,
                "{t} should be ReversibleWithEffort"
            );
        }
    }

    #[test]
    fn layer1_unknown_tool_falls_back_to_irreversible() {
        let c = LayeredReversibilityClassifier::empty();
        assert_eq!(
            c.classify("UnknownTool", &no_input()),
            ReversibilityClass::Irreversible
        );
    }

    // ---- Layer 2: per-MCP-tool defaults ----

    #[test]
    fn layer2_known_mcp_tool_lookup() {
        let toml_src = r#"
            [mcp.linear]
            list_issues = "TriviallyReversible"
            create_issue = "ReversibleWithEffort"
            delete_issue = "Irreversible"
        "#;
        let c = LayeredReversibilityClassifier::from_str(toml_src, None).unwrap();
        assert_eq!(
            c.classify("mcp__linear__list_issues", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
        assert_eq!(
            c.classify("mcp__linear__create_issue", &no_input()),
            ReversibilityClass::ReversibleWithEffort
        );
        assert_eq!(
            c.classify("mcp__linear__delete_issue", &no_input()),
            ReversibilityClass::Irreversible
        );
    }

    #[test]
    fn layer2_unknown_mcp_tool_defaults_to_irreversible() {
        let toml_src = r#"
            [mcp.linear]
            list_issues = "TriviallyReversible"
        "#;
        let c = LayeredReversibilityClassifier::from_str(toml_src, None).unwrap();
        // Unknown tool on known server
        assert_eq!(
            c.classify("mcp__linear__unknown_tool", &no_input()),
            ReversibilityClass::Irreversible
        );
        // Unknown server entirely
        assert_eq!(
            c.classify("mcp__unknown__anything", &no_input()),
            ReversibilityClass::Irreversible
        );
    }

    #[test]
    fn layer2_mcp_tool_name_with_underscores_preserved() {
        // tool name "send_message" contains underscore but is one token
        // after the first split on `__`.
        let toml_src = r#"
            [mcp.gmail]
            send_message = "Catastrophic"
        "#;
        let c = LayeredReversibilityClassifier::from_str(toml_src, None).unwrap();
        assert_eq!(
            c.classify("mcp__gmail__send_message", &no_input()),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn layer2_malformed_mcp_name_falls_back_to_irreversible() {
        let c = LayeredReversibilityClassifier::empty();
        // Missing __<tool> suffix
        assert_eq!(
            c.classify("mcp__linear", &no_input()),
            ReversibilityClass::Irreversible
        );
        // Just "mcp__"
        assert_eq!(
            c.classify("mcp__", &no_input()),
            ReversibilityClass::Irreversible
        );
    }

    // ---- Layer 3: Bash patterns ----

    #[test]
    fn layer3_bash_catastrophic_pattern_matches() {
        let toml_src = r#"
            [[bash.pattern]]
            match = "rm -rf /"
            class = "Catastrophic"
        "#;
        let c = LayeredReversibilityClassifier::from_str(toml_src, None).unwrap();
        assert_eq!(
            c.classify("Bash", &bash_cmd("rm -rf /")),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn layer3_bash_pattern_order_first_match_wins() {
        let toml_src = r#"
            [[bash.pattern]]
            match = "^git push"
            class = "Irreversible"

            [[bash.pattern]]
            match = "--force"
            class = "Catastrophic"
        "#;
        let c = LayeredReversibilityClassifier::from_str(toml_src, None).unwrap();
        // First match wins: `git push --force` matches `^git push` first,
        // so classifies as Irreversible. Operators wanting catastrophic
        // classification for force-push should put the more specific
        // pattern FIRST.
        assert_eq!(
            c.classify("Bash", &bash_cmd("git push --force")),
            ReversibilityClass::Irreversible
        );
    }

    #[test]
    fn layer3_bash_no_match_falls_back_to_reversible_with_effort() {
        let c = LayeredReversibilityClassifier::empty();
        assert_eq!(
            c.classify("Bash", &bash_cmd("ls -la")),
            ReversibilityClass::ReversibleWithEffort
        );
    }

    #[test]
    fn layer3_bash_missing_command_field_treated_as_empty() {
        let c = LayeredReversibilityClassifier::empty();
        // No `command` key at all → empty string → falls through to default.
        assert_eq!(
            c.classify("Bash", &json!({ "other": "field" })),
            ReversibilityClass::ReversibleWithEffort
        );
        // `command` is non-string → also treated as empty.
        assert_eq!(
            c.classify("Bash", &json!({ "command": 42 })),
            ReversibilityClass::ReversibleWithEffort
        );
    }

    #[test]
    fn layer3_invalid_regex_surfaces_at_load_not_classify() {
        let toml_src = r#"
            [[bash.pattern]]
            match = "(?P<unclosed"
            class = "Catastrophic"
        "#;
        let err = LayeredReversibilityClassifier::from_str(toml_src, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bash pattern regex"),
            "error should name the failing pattern context: {msg}"
        );
    }

    // ---- Layer 4: operator overrides ----

    #[test]
    fn layer4_override_beats_builtin() {
        let toml_src = r#"
            [overrides]
            Read = "Catastrophic"
        "#;
        let c = LayeredReversibilityClassifier::from_str(toml_src, None).unwrap();
        // Override flips Read from the built-in TriviallyReversible.
        assert_eq!(
            c.classify("Read", &no_input()),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn layer4_override_beats_mcp() {
        let toml_src = r#"
            [mcp.linear]
            list_issues = "TriviallyReversible"

            [overrides]
            "mcp__linear__list_issues" = "Catastrophic"
        "#;
        let c = LayeredReversibilityClassifier::from_str(toml_src, None).unwrap();
        assert_eq!(
            c.classify("mcp__linear__list_issues", &no_input()),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn layer4_override_beats_bash_pattern() {
        let toml_src = r#"
            [[bash.pattern]]
            match = "rm"
            class = "Catastrophic"

            [overrides]
            Bash = "TriviallyReversible"
        "#;
        let c = LayeredReversibilityClassifier::from_str(toml_src, None).unwrap();
        // Override of the Bash tool name short-circuits before pattern eval.
        assert_eq!(
            c.classify("Bash", &bash_cmd("rm -rf /")),
            ReversibilityClass::TriviallyReversible
        );
    }

    // ---- Merge semantics across defaults + overrides files ----

    #[test]
    fn merge_mcp_overrides_file_wins() {
        let defaults = r#"
            [mcp.linear]
            list_issues = "TriviallyReversible"
        "#;
        let overrides = r#"
            [mcp.linear]
            list_issues = "Catastrophic"
        "#;
        let c = LayeredReversibilityClassifier::from_str(defaults, Some(overrides)).unwrap();
        assert_eq!(
            c.classify("mcp__linear__list_issues", &no_input()),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn merge_mcp_overrides_file_adds_to_known_server() {
        let defaults = r#"
            [mcp.linear]
            list_issues = "TriviallyReversible"
        "#;
        let overrides = r#"
            [mcp.linear]
            create_issue = "ReversibleWithEffort"
        "#;
        let c = LayeredReversibilityClassifier::from_str(defaults, Some(overrides)).unwrap();
        // Original entry preserved.
        assert_eq!(
            c.classify("mcp__linear__list_issues", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
        // New entry visible.
        assert_eq!(
            c.classify("mcp__linear__create_issue", &no_input()),
            ReversibilityClass::ReversibleWithEffort
        );
    }

    #[test]
    fn merge_bash_patterns_concatenate_defaults_first() {
        let defaults = r#"
            [[bash.pattern]]
            match = "^cargo build"
            class = "ReversibleWithEffort"
        "#;
        let overrides = r#"
            [[bash.pattern]]
            match = "cargo"
            class = "Catastrophic"
        "#;
        let c = LayeredReversibilityClassifier::from_str(defaults, Some(overrides)).unwrap();
        // Defaults patterns evaluated FIRST → cargo build matches the
        // ReversibleWithEffort rule before reaching the overrides
        // catch-all. Lets sentinel ship safe defaults that operators
        // refine without losing them.
        assert_eq!(
            c.classify("Bash", &bash_cmd("cargo build --release")),
            ReversibilityClass::ReversibleWithEffort
        );
    }

    #[test]
    fn merge_overrides_table_overrides_file_wins() {
        let defaults = r#"
            [overrides]
            Read = "Irreversible"
        "#;
        let overrides = r#"
            [overrides]
            Read = "Catastrophic"
        "#;
        let c = LayeredReversibilityClassifier::from_str(defaults, Some(overrides)).unwrap();
        assert_eq!(
            c.classify("Read", &no_input()),
            ReversibilityClass::Catastrophic
        );
    }

    // ---- TOML parsing edge cases ----

    #[test]
    fn empty_toml_yields_empty_classifier() {
        let c = LayeredReversibilityClassifier::from_str("", None).unwrap();
        assert_eq!(
            c.classify("Read", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
        assert_eq!(
            c.classify("Bash", &bash_cmd("anything")),
            ReversibilityClass::ReversibleWithEffort
        );
    }

    #[test]
    fn malformed_toml_surfaces_clear_error() {
        let err = LayeredReversibilityClassifier::from_str("not [valid] toml=", None)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("reversibility defaults TOML"),
            "error should name the source TOML context: {msg}"
        );
    }

    #[test]
    fn unknown_class_in_toml_surfaces_clear_error() {
        let toml_src = r#"
            [overrides]
            Read = "NotARealClass"
        "#;
        let err = LayeredReversibilityClassifier::from_str(toml_src, None).unwrap_err();
        // Just confirm the load failed with a parse error mentioning TOML.
        let msg = format!("{err:#}");
        assert!(
            msg.contains("TOML") || msg.contains("toml"),
            "error should mention TOML parsing: {msg}"
        );
    }

    // ---- Path-based loader ----

    #[test]
    fn load_from_paths_reads_defaults_and_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let defaults_path = tmp.path().join("defaults.toml");
        let overrides_path = tmp.path().join("overrides.toml");
        std::fs::write(
            &defaults_path,
            r#"
            [mcp.linear]
            list_issues = "TriviallyReversible"
        "#,
        )
        .unwrap();
        std::fs::write(
            &overrides_path,
            r#"
            [overrides]
            Read = "Catastrophic"
        "#,
        )
        .unwrap();
        let c = LayeredReversibilityClassifier::load_from_paths(
            &defaults_path,
            Some(&overrides_path),
        )
        .unwrap();
        assert_eq!(
            c.classify("mcp__linear__list_issues", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
        assert_eq!(
            c.classify("Read", &no_input()),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn load_from_paths_skips_missing_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let defaults_path = tmp.path().join("defaults.toml");
        let overrides_path = tmp.path().join("nonexistent.toml");
        std::fs::write(&defaults_path, "").unwrap();
        // overrides_path is some, but the file does not exist → silently skip.
        let c = LayeredReversibilityClassifier::load_from_paths(
            &defaults_path,
            Some(&overrides_path),
        )
        .unwrap();
        assert_eq!(
            c.classify("Read", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
    }

    #[test]
    fn load_from_paths_errors_on_missing_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let defaults_path = tmp.path().join("nonexistent.toml");
        let err = LayeredReversibilityClassifier::load_from_paths(&defaults_path, None)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("reversibility defaults"),
            "error should name defaults-path context: {msg}"
        );
    }

    // ---- Trait-object usability + Send/Sync ----

    #[test]
    fn usable_through_port_trait_object() {
        let c = LayeredReversibilityClassifier::empty();
        let port: &dyn ReversibilityClassifierPort = &c;
        assert_eq!(
            port.classify("Read", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
    }

    #[test]
    fn classifier_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LayeredReversibilityClassifier>();
    }

    // ---- Conservative-default contract preserved ----

    #[test]
    fn full_unknown_tool_falls_back_to_irreversible() {
        let c = LayeredReversibilityClassifier::empty();
        assert_eq!(
            c.classify("CompletelyUnknown", &no_input()),
            ReversibilityClass::Irreversible
        );
    }

    // ---- Shipped defaults smoke tests ----
    //
    // These tests load the `config/reversibility-defaults.toml` shipped with
    // sentinel via `include_str!`. The point is to catch (a) syntax breaks
    // in the shipped TOML and (b) classification regressions on
    // well-known tools. If you intentionally change a classification in
    // the shipped defaults, update the corresponding assertion here.

    const SHIPPED_DEFAULTS: &str =
        include_str!("../../../config/reversibility-defaults.toml");

    fn shipped() -> LayeredReversibilityClassifier {
        LayeredReversibilityClassifier::from_str(SHIPPED_DEFAULTS, None)
            .expect("shipped reversibility-defaults.toml should parse cleanly")
    }

    #[test]
    fn shipped_defaults_parse() {
        // Just constructing exercises the TOML parser + regex compiler.
        let _ = shipped();
    }

    #[test]
    fn shipped_defaults_layer1_builtins_still_apply() {
        let c = shipped();
        assert_eq!(
            c.classify("Read", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
        assert_eq!(
            c.classify("Edit", &no_input()),
            ReversibilityClass::ReversibleWithEffort
        );
    }

    #[test]
    fn shipped_defaults_linear_mcp_classifications() {
        let c = shipped();
        assert_eq!(
            c.classify("mcp__linear__list_issues", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
        assert_eq!(
            c.classify("mcp__linear__create_issue", &no_input()),
            ReversibilityClass::ReversibleWithEffort
        );
        assert_eq!(
            c.classify("mcp__linear__delete_issue", &no_input()),
            ReversibilityClass::Irreversible
        );
    }

    #[test]
    fn shipped_defaults_gmail_send_is_catastrophic() {
        let c = shipped();
        assert_eq!(
            c.classify("mcp__gmail__send_message", &no_input()),
            ReversibilityClass::Catastrophic
        );
        assert_eq!(
            c.classify("mcp__gmail__list_messages", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
    }

    #[test]
    fn shipped_defaults_slack_post_is_irreversible() {
        let c = shipped();
        assert_eq!(
            c.classify("mcp__slack__post_message", &no_input()),
            ReversibilityClass::Irreversible
        );
        assert_eq!(
            c.classify("mcp__slack__list_channels", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
    }

    #[test]
    fn shipped_defaults_doc_systems_create_is_irreversible() {
        let c = shipped();
        assert_eq!(
            c.classify("mcp__confluence__create_page", &no_input()),
            ReversibilityClass::Irreversible
        );
        assert_eq!(
            c.classify("mcp__notion__create_page", &no_input()),
            ReversibilityClass::Irreversible
        );
        assert_eq!(
            c.classify("mcp__drive__upload_file", &no_input()),
            ReversibilityClass::Irreversible
        );
    }

    #[test]
    fn shipped_defaults_unknown_mcp_falls_back_to_irreversible() {
        let c = shipped();
        // Conservative default holds even with full defaults loaded.
        assert_eq!(
            c.classify("mcp__unknown_server__anything", &no_input()),
            ReversibilityClass::Irreversible
        );
    }

    #[test]
    fn shipped_defaults_bash_rm_rf_root_is_catastrophic() {
        let c = shipped();
        assert_eq!(
            c.classify("Bash", &bash_cmd("rm -rf /")),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn shipped_defaults_bash_drop_table_is_catastrophic() {
        let c = shipped();
        assert_eq!(
            c.classify("Bash", &bash_cmd("psql -c 'DROP TABLE users'")),
            ReversibilityClass::Catastrophic
        );
        // case-insensitive match
        assert_eq!(
            c.classify("Bash", &bash_cmd("psql -c 'drop database production'")),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn shipped_defaults_bash_force_push_to_main_is_catastrophic() {
        let c = shipped();
        assert_eq!(
            c.classify("Bash", &bash_cmd("git push --force origin main")),
            ReversibilityClass::Catastrophic
        );
        assert_eq!(
            c.classify("Bash", &bash_cmd("git push -f origin master")),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn shipped_defaults_bash_force_push_to_release_or_prod_branch_is_catastrophic() {
        let c = shipped();
        for cmd in [
            "git push --force origin release",
            "git push -f origin prod",
            "git push --force origin production",
        ] {
            assert_eq!(
                c.classify("Bash", &bash_cmd(cmd)),
                ReversibilityClass::Catastrophic,
                "expected Catastrophic for: {cmd}"
            );
        }
    }

    #[test]
    fn shipped_defaults_bash_aws_destructive_patterns_are_catastrophic() {
        let c = shipped();
        for cmd in [
            "aws s3 rb s3://prod-uploads --force",
            "aws rds delete-db-instance --db-instance-identifier prod-db --skip-final-snapshot",
            "aws iam delete-user --user-name svc-account",
            "aws iam delete-role --role-name prod-deploy",
            "aws iam delete-policy --policy-arn arn:aws:iam::123:policy/x",
        ] {
            assert_eq!(
                c.classify("Bash", &bash_cmd(cmd)),
                ReversibilityClass::Catastrophic,
                "expected Catastrophic for: {cmd}"
            );
        }
    }

    #[test]
    fn shipped_defaults_bash_gcloud_project_delete_is_catastrophic() {
        let c = shipped();
        assert_eq!(
            c.classify("Bash", &bash_cmd("gcloud projects delete my-prod-proj")),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn shipped_defaults_bash_kubectl_namespace_delete_is_catastrophic() {
        let c = shipped();
        for cmd in [
            "kubectl delete namespace billing",
            "kubectl delete ns experiments",
        ] {
            assert_eq!(
                c.classify("Bash", &bash_cmd(cmd)),
                ReversibilityClass::Catastrophic,
                "expected Catastrophic for: {cmd}"
            );
        }
    }

    #[test]
    fn shipped_defaults_bash_helm_uninstall_prod_is_catastrophic() {
        let c = shipped();
        assert_eq!(
            c.classify(
                "Bash",
                &bash_cmd("helm uninstall my-app --namespace production")
            ),
            ReversibilityClass::Catastrophic
        );
        assert_eq!(
            c.classify("Bash", &bash_cmd("helm delete my-app -n prod")),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn shipped_defaults_bash_gh_repo_delete_is_catastrophic() {
        let c = shipped();
        assert_eq!(
            c.classify("Bash", &bash_cmd("gh repo delete acme/internal-tools")),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn shipped_defaults_bash_dropdb_cli_is_catastrophic() {
        let c = shipped();
        assert_eq!(
            c.classify("Bash", &bash_cmd("dropdb prod_archive")),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn shipped_defaults_bash_chmod_777_on_system_dirs_is_catastrophic() {
        let c = shipped();
        for cmd in [
            "chmod 777 /etc/passwd",
            "chmod -R 777 /usr/local/bin",
            "chmod -R 777 /var/log",
        ] {
            assert_eq!(
                c.classify("Bash", &bash_cmd(cmd)),
                ReversibilityClass::Catastrophic,
                "expected Catastrophic for: {cmd}"
            );
        }
    }

    #[test]
    fn shipped_defaults_bash_force_with_lease_is_demoted() {
        let c = shipped();
        // --force-with-lease is matched by its specific rule BEFORE the
        // plain `git push` rule.
        assert_eq!(
            c.classify(
                "Bash",
                &bash_cmd("git push --force-with-lease origin feat/x")
            ),
            ReversibilityClass::ReversibleWithEffort
        );
    }

    #[test]
    fn shipped_defaults_bash_plain_push_is_irreversible() {
        let c = shipped();
        assert_eq!(
            c.classify("Bash", &bash_cmd("git push origin main")),
            ReversibilityClass::Irreversible
        );
    }

    #[test]
    fn shipped_defaults_bash_publish_commands_are_irreversible() {
        let c = shipped();
        for cmd in [
            "npm publish",
            "pnpm publish --access public",
            "cargo publish",
        ] {
            assert_eq!(
                c.classify("Bash", &bash_cmd(cmd)),
                ReversibilityClass::Irreversible,
                "{cmd} should be Irreversible"
            );
        }
    }

    #[test]
    fn shipped_defaults_bash_local_git_ops_are_reversible_with_effort() {
        let c = shipped();
        for cmd in [
            "git commit -m foo",
            "git reset --hard HEAD~1",
            "git checkout feat/x",
            "git rebase main",
        ] {
            assert_eq!(
                c.classify("Bash", &bash_cmd(cmd)),
                ReversibilityClass::ReversibleWithEffort,
                "{cmd} should be ReversibleWithEffort"
            );
        }
    }

    #[test]
    fn shipped_defaults_bash_read_only_ops_are_trivially_reversible() {
        let c = shipped();
        for cmd in [
            "ls -la",
            "cat README.md",
            "grep -r foo src/",
            "git status",
            "git log --oneline -10",
            "echo hello",
        ] {
            assert_eq!(
                c.classify("Bash", &bash_cmd(cmd)),
                ReversibilityClass::TriviallyReversible,
                "{cmd} should be TriviallyReversible"
            );
        }
    }

    #[test]
    fn shipped_defaults_bash_unmatched_command_falls_back_to_reversible_with_effort() {
        let c = shipped();
        // Conservative default for Bash when nothing matches.
        assert_eq!(
            c.classify("Bash", &bash_cmd("./some_custom_script.sh")),
            ReversibilityClass::ReversibleWithEffort
        );
    }

    #[test]
    fn shipped_defaults_compose_with_operator_overrides() {
        // Confirm operator overrides win even with the full shipped
        // defaults loaded — the Layer 4 short-circuit holds.
        let operator_overrides = r#"
            [overrides]
            "mcp__linear__list_issues" = "Catastrophic"
        "#;
        let c =
            LayeredReversibilityClassifier::from_str(SHIPPED_DEFAULTS, Some(operator_overrides))
                .unwrap();
        assert_eq!(
            c.classify("mcp__linear__list_issues", &no_input()),
            ReversibilityClass::Catastrophic
        );
    }

    // ---- PowerShell dispatch (Windows sibling of Bash) ----

    fn ps_cmd(s: &str) -> Value {
        json!({ "command": s })
    }

    #[test]
    fn powershell_unmatched_command_is_reversible_with_effort_not_irreversible() {
        // THE deadlock-fix regression: before the dispatch arm existed,
        // PowerShell fell through builtin_class to Irreversible, stranding
        // every PowerShell call at the A3 dry-run gate. It must now share
        // Bash's ReversibleWithEffort default for unrecognized commands.
        let c = shipped();
        assert_eq!(
            c.classify("PowerShell", &ps_cmd("Get-Process | Select-Object -First 5")),
            ReversibilityClass::ReversibleWithEffort,
            "unmatched PowerShell must NOT be Irreversible"
        );
    }

    #[test]
    fn powershell_read_only_invoke_restmethod_is_not_irreversible() {
        let c = shipped();
        let class = c.classify("PowerShell", &ps_cmd("Invoke-RestMethod -Uri https://openrouter.ai/api/v1/models"));
        assert_ne!(
            class,
            ReversibilityClass::Irreversible,
            "read-only PowerShell GET must not strand at the dry-run gate"
        );
    }

    #[test]
    fn powershell_remove_item_recurse_force_is_catastrophic() {
        let c = shipped();
        for cmd in [
            concat!("Remove-Item ", "-Recurse ", "-Force C:/data"),
            concat!("Remove-Item ", "-Force ", "-Recurse C:/data"),
        ] {
            assert_eq!(
                c.classify("PowerShell", &ps_cmd(cmd)),
                ReversibilityClass::Catastrophic,
                "expected Catastrophic for: {cmd}"
            );
        }
    }

    #[test]
    fn powershell_disk_cmdlets_are_catastrophic() {
        let c = shipped();
        for cmd in ["Format-Volume -DriveLetter D", "Clear-Disk -Number 1 -RemoveData"] {
            assert_eq!(
                c.classify("PowerShell", &ps_cmd(cmd)),
                ReversibilityClass::Catastrophic,
                "expected Catastrophic for: {cmd}"
            );
        }
    }

    #[test]
    fn powershell_shares_bash_catastrophic_patterns() {
        let c = shipped();
        assert_eq!(
            c.classify("PowerShell", &ps_cmd("git push --force origin main")),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn powershell_registry_delete_is_catastrophic() {
        let c = shipped();
        assert_eq!(
            c.classify("PowerShell", &ps_cmd("Remove-Item -Path HKLM:\\SOFTWARE\\Foo -Recurse")),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn sequential_thinking_mcp_is_trivially_reversible() {
        let c = shipped();
        assert_eq!(
            c.classify("mcp__sequential-thinking__sequentialthinking", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
    }
}
