// Today's public surface (`validate_all`, `validate_one`, `SkillReport`)
// is exercised exclusively from this file's test module — no runtime
// caller wires it into `federation_cmd` yet. The follow-up that does
// so (promoting the check into a blocking gate) is intentionally a
// separate task so this validator can bake as a `cargo test --
// --ignored` first.
#![allow(dead_code)]

//! Step-config schema validator (M5.0 — task #103).
//!
//! Catches typo-class bugs in `config/steps/*.toml` enrichment data
//! before any real execution runs — particularly:
//!
//! 1. **Forward-reference dangling.** When a step's `artifact_schema`
//!    declares a value like `"string (from fetch/1.1)"`, the substring
//!    `from <phase_id>/<step_id>` must point at a step that actually
//!    exists in the same TOML file. Without this check, a typo like
//!    `(from fetch/1.10)` (instead of `1.1`) silently dangles — the
//!    skills-mcp codegen accepts it, real execution can't resolve it,
//!    and we don't find out until an integration test on real Linear
//!    fails confusingly.
//!
//! 2. **Suggested-tool name shape.** `suggested_tools` strings must
//!    match `mcp__<server>__<tool>` or `Bash:<...>` or an internal
//!    marker like `EnterPlanMode` / `EnterWorktree`. Off-spec names
//!    (camelCase, typos, double underscores in odd places) are
//!    flagged as suspicious — the codegen embeds them verbatim and
//!    the runtime caller would just see "no such tool" much later.
//!
//! Both checks are **report-only** today: the loader still accepts
//! the file, and `federation compose` still proceeds. The validator
//! lives as a `cargo test` that fails when configs drift from the
//! schema's intent. Promoting failures into the `federation
//! compose` blocking gate is a follow-up — let the test bake first.
//!
//! ## Why a self-contained TOML re-parser
//!
//! The canonical `SkillSteps` deserializer in `sentinel-domain` does
//! NOT carry `artifact_schema` or `suggested_tools` fields — those
//! arrived via the M2.3 enrichment that landed only in
//! `skills-mcp-rust`'s build.rs codegen path (commit 460146e in
//! sentinel + M2.3.0 in skills-mcp-rust). On the sentinel side, those
//! fields are silently dropped by serde's `deny_unknown_fields`-off
//! default. Re-parsing the TOML with a struct that knows about them
//! is the cheapest way to validate without forcing changes to the
//! canonical loader (which would ripple through `federation_cmd`'s
//! 1200+ lines).
//!
//! Long-term, M2.10 (references/ subdirs) or a unified Apollo-style
//! schema will probably absorb this. For now: belt-and-suspenders on
//! a real bug class, with no risk to the loader.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Raw shape of a step in the validator's view — strictly the fields
/// the validator inspects. `serde(default)` everywhere so any
/// canonical field we don't model is silently allowed.
#[derive(Debug, serde::Deserialize)]
struct ValidatedStep {
    id: String,
    #[serde(default)]
    suggested_tools: Vec<String>,
    /// `artifact_schema` is an inline TOML table whose values are
    /// strings (or other tables; we recurse). We keep it as
    /// `toml::Value` and walk it ourselves.
    #[serde(default)]
    artifact_schema: Option<toml::Value>,
}

#[derive(Debug, serde::Deserialize)]
struct ValidatedPhase {
    id: String,
    #[serde(default)]
    steps: Vec<ValidatedStep>,
}

#[derive(Debug, serde::Deserialize)]
struct ValidatedSkill {
    #[serde(default)]
    phases: Vec<ValidatedPhase>,
}

/// Per-skill summary of what the validator found. Useful as
/// machine-readable output; `report()` is the human surface.
#[derive(Debug, Default)]
pub struct SkillReport {
    pub skill: String,
    pub step_count: usize,
    /// `(phase_id, step_id, dangling_ref)` triples — the schema or
    /// description references a step that doesn't exist in this file.
    pub dangling_refs: Vec<(String, String, String)>,
    /// `(phase_id, step_id, suspicious_tool)` triples — a
    /// `suggested_tools` entry doesn't match the documented shapes.
    pub suspicious_tools: Vec<(String, String, String)>,
}

impl SkillReport {
    pub const fn is_clean(&self) -> bool {
        self.dangling_refs.is_empty() && self.suspicious_tools.is_empty()
    }
}

/// Validate every `*.toml` under `steps_dir`. Returns one report
/// per skill, sorted by skill name. Skipping or returning early
/// is preferred over erroring — a malformed TOML in one file
/// shouldn't hide schema issues in others.
pub fn validate_all(steps_dir: &Path) -> Result<Vec<SkillReport>> {
    let mut by_skill: BTreeMap<String, SkillReport> = BTreeMap::new();
    let entries = std::fs::read_dir(steps_dir)
        .with_context(|| format!("read_dir({})", steps_dir.display()))?;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let skill = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let report = validate_one(&path, &skill).unwrap_or_else(|e| SkillReport {
            skill: skill.clone(),
            step_count: 0,
            dangling_refs: vec![(
                "<file>".to_string(),
                "<file>".to_string(),
                format!("parse error: {e}"),
            )],
            suspicious_tools: vec![],
        });
        by_skill.insert(skill, report);
    }
    Ok(by_skill.into_values().collect())
}

/// Validate one skill's TOML.
pub fn validate_one(path: &Path, skill: &str) -> Result<SkillReport> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read_to_string({})", path.display()))?;
    let parsed: ValidatedSkill =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;

    // Step universe: every `(phase_id, step_id)` pair in this file.
    let mut universe: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    let mut step_count = 0usize;
    for phase in &parsed.phases {
        for step in &phase.steps {
            universe.insert((phase.id.clone(), step.id.clone()));
            step_count += 1;
        }
    }

    let mut dangling_refs = Vec::new();
    let mut suspicious_tools = Vec::new();

    for phase in &parsed.phases {
        for step in &phase.steps {
            // Forward-ref check: walk artifact_schema values, scan
            // for `from <phase>/<step>` and verify each resolves.
            if let Some(schema) = &step.artifact_schema {
                collect_strings_recursive(schema, &mut |s: &str| {
                    for (ref_phase, ref_step) in find_forward_refs(s) {
                        if !universe.contains(&(ref_phase.clone(), ref_step.clone())) {
                            dangling_refs.push((
                                phase.id.clone(),
                                step.id.clone(),
                                format!("(from {ref_phase}/{ref_step})"),
                            ));
                        }
                    }
                });
            }

            // Suggested-tool shape check.
            for tool in &step.suggested_tools {
                if !is_well_formed_tool(tool) {
                    suspicious_tools.push((phase.id.clone(), step.id.clone(), tool.clone()));
                }
            }
        }
    }

    Ok(SkillReport {
        skill: skill.to_string(),
        step_count,
        dangling_refs,
        suspicious_tools,
    })
}

/// Walk a TOML value and call `f` on every string leaf. Used by the
/// forward-ref scanner — we don't care where in the schema the string
/// lives, only what it says.
fn collect_strings_recursive(value: &toml::Value, f: &mut impl FnMut(&str)) {
    match value {
        toml::Value::String(s) => f(s),
        toml::Value::Array(arr) => {
            for v in arr {
                collect_strings_recursive(v, f);
            }
        }
        toml::Value::Table(tbl) => {
            for v in tbl.values() {
                collect_strings_recursive(v, f);
            }
        }
        // Numbers, booleans, datetimes: irrelevant to ref scanning.
        _ => {}
    }
}

/// Extract every `from <phase>/<step>` reference from a string. The
/// shape we look for is the literal substring `(from ` followed by a
/// phase identifier, `/`, a step identifier, and either `)` or whitespace.
///
/// Returns owned strings to keep callers simple. The set is allowed to
/// contain duplicates — each occurrence is a separate dangling-ref
/// failure if the target doesn't exist.
fn find_forward_refs(s: &str) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    let needle = "(from ";
    let mut search = s;
    while let Some(idx) = search.find(needle) {
        let after = &search[idx + needle.len()..];
        // The phase id runs up to '/'; the step id runs up to ')' or
        // whitespace. We're lenient about extra annotations after the
        // step id, e.g. `(from fetch/1.1 — bla bla)`.
        if let Some(slash) = after.find('/') {
            let phase = after[..slash].trim().to_string();
            let after_slash = &after[slash + 1..];
            // Step id terminates at `)`, whitespace, or comma.
            let term = after_slash
                .find(|c: char| c == ')' || c.is_whitespace() || c == ',')
                .unwrap_or(after_slash.len());
            let step = after_slash[..term].trim().to_string();
            if !phase.is_empty() && !step.is_empty() {
                refs.push((phase, step));
            }
            search = &after_slash[term..];
        } else {
            // Malformed — no slash before end of string. Stop scanning
            // this string; the validator only catches resolvable refs.
            break;
        }
    }
    refs
}

/// Is `tool` a recognized tool-name shape?
///
/// Accepted forms:
/// - `mcp__<server>__<tool>` — peer MCP tool calls. Server and tool
///   are non-empty, lowercase letters / digits / underscores.
///   Re-checks the double-underscore separator structure.
/// - `Bash:<cmd>` — anything after `Bash:` is the suggested invocation;
///   we don't constrain the command.
/// - `EnterPlanMode` / `EnterWorktree` / `ExitPlanMode` / `ExitWorktree`
///   — internal Claude Code tools without the mcp__ prefix.
///
/// Anything else is reported as suspicious — not necessarily wrong,
/// but worth a human glance. The list of internal tools above is the
/// closed set used by the linear.toml today; extending it as more
/// internal tools are referenced is a one-line change.
fn is_well_formed_tool(tool: &str) -> bool {
    // Internal Claude Code tools.
    matches!(
        tool,
        "EnterPlanMode" | "ExitPlanMode" | "EnterWorktree" | "ExitWorktree"
    ) || tool.starts_with("Bash:")
        || is_mcp_tool_name(tool)
}

fn is_mcp_tool_name(tool: &str) -> bool {
    // Must be `mcp__<server>__<tool>` with both server and tool
    // non-empty and matching [a-z0-9_-]+ (server names like
    // `claude-accounts` exist in the wild; tool names use _).
    let Some(rest) = tool.strip_prefix("mcp__") else {
        return false;
    };
    let parts: Vec<&str> = rest.splitn(2, "__").collect();
    if parts.len() != 2 {
        return false;
    }
    let server = parts[0];
    let tool = parts[1];
    if server.is_empty() || tool.is_empty() {
        return false;
    }
    let server_ok = server
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    let tool_ok = tool
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    server_ok && tool_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_forward_refs_extracts_phase_and_step() {
        let s = "string (from fetch/1.1)";
        assert_eq!(
            find_forward_refs(s),
            vec![("fetch".to_string(), "1.1".to_string())],
        );
    }

    #[test]
    fn find_forward_refs_handles_annotations_after_step() {
        let s = "string (from fetch/1.1 — bla bla bla)";
        assert_eq!(
            find_forward_refs(s),
            vec![("fetch".to_string(), "1.1".to_string())],
        );
    }

    #[test]
    fn find_forward_refs_handles_multiple_refs() {
        let s = "value (from claim/0.2) plus (from fetch/1.1)";
        let refs = find_forward_refs(s);
        assert_eq!(refs.len(), 2);
        assert!(refs.contains(&("claim".to_string(), "0.2".to_string())));
        assert!(refs.contains(&("fetch".to_string(), "1.1".to_string())));
    }

    #[test]
    fn find_forward_refs_handles_negative_step_ids() {
        // Linear's claim phase uses "-0.1", "-0.2" etc. These must
        // round-trip cleanly.
        let s = "(from claim/-0.1)";
        assert_eq!(
            find_forward_refs(s),
            vec![("claim".to_string(), "-0.1".to_string())],
        );
    }

    #[test]
    fn find_forward_refs_ignores_non_ref_strings() {
        assert!(find_forward_refs("just a plain string").is_empty());
        assert!(find_forward_refs("(but not from anything)").is_empty());
        assert!(find_forward_refs("(from )").is_empty()); // empty phase
    }

    #[test]
    fn is_mcp_tool_name_accepts_real_examples() {
        assert!(is_mcp_tool_name("mcp__linear__get_issue"));
        assert!(is_mcp_tool_name("mcp__browserbase__create_session"));
        assert!(is_mcp_tool_name("mcp__github__pr_create"));
        // Server with hyphen (e.g. claude-accounts).
        assert!(is_mcp_tool_name("mcp__claude-accounts__account_list"));
        assert!(is_mcp_tool_name(
            "mcp__sequential-thinking__sequentialthinking"
        ));
    }

    #[test]
    fn is_mcp_tool_name_rejects_off_spec() {
        assert!(!is_mcp_tool_name("mcp__linear__GetIssue")); // camelCase tool
        assert!(!is_mcp_tool_name("mcp_linear_get_issue")); // single _ separator
        assert!(!is_mcp_tool_name("mcp__linear")); // no tool
        assert!(!is_mcp_tool_name("mcp____get_issue")); // empty server
        assert!(!is_mcp_tool_name("get_issue")); // no prefix
    }

    #[test]
    fn is_well_formed_tool_accepts_internal_and_bash() {
        assert!(is_well_formed_tool("EnterPlanMode"));
        assert!(is_well_formed_tool("EnterWorktree"));
        assert!(is_well_formed_tool("Bash:git push -u origin"));
        assert!(is_well_formed_tool("Bash:cargo test --workspace"));
    }

    #[test]
    fn validator_catches_dangling_forward_ref() {
        // Synthetic TOML: one step references a phase/step that doesn't exist.
        let toml_src = r#"
federation_version = "1"

[[phases]]
id = "claim"

[[phases.steps]]
id = "0.1"
description = "the only step that exists"
artifact_schema = { issue_id = "string (from fetch/9.9)" }
"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("synthetic.toml");
        std::fs::write(&path, toml_src).unwrap();
        let report = validate_one(&path, "synthetic").unwrap();
        assert_eq!(report.dangling_refs.len(), 1);
        let (phase, step, dangling) = &report.dangling_refs[0];
        assert_eq!(phase, "claim");
        assert_eq!(step, "0.1");
        assert!(dangling.contains("fetch/9.9"));
    }

    #[test]
    fn validator_accepts_valid_forward_ref_within_skill() {
        let toml_src = r#"
federation_version = "1"

[[phases]]
id = "fetch"

[[phases.steps]]
id = "1.1"
description = "first"

[[phases]]
id = "intelligence"

[[phases.steps]]
id = "1.5.1"
description = "uses 1.1"
artifact_schema = { issue_id = "string (from fetch/1.1)" }
"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("synthetic.toml");
        std::fs::write(&path, toml_src).unwrap();
        let report = validate_one(&path, "synthetic").unwrap();
        assert!(
            report.dangling_refs.is_empty(),
            "got: {:?}",
            report.dangling_refs
        );
        assert_eq!(report.step_count, 2);
    }

    #[test]
    fn validator_catches_suspicious_tool_name() {
        let toml_src = r#"
federation_version = "1"

[[phases]]
id = "claim"

[[phases.steps]]
id = "0.1"
description = "calls a bad-shaped tool"
suggested_tools = ["mcp__linear__GetIssue", "mcp_linear_old_style"]
"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("synthetic.toml");
        std::fs::write(&path, toml_src).unwrap();
        let report = validate_one(&path, "synthetic").unwrap();
        assert_eq!(report.suspicious_tools.len(), 2);
    }

    #[test]
    fn validator_accepts_well_formed_tools() {
        let toml_src = r#"
federation_version = "1"

[[phases]]
id = "claim"

[[phases.steps]]
id = "0.1"
description = "well-formed"
suggested_tools = [
    "mcp__linear__get_issue",
    "Bash:cargo test --workspace",
    "EnterPlanMode",
    "EnterWorktree",
    "mcp__sequential-thinking__sequentialthinking",
]
"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("synthetic.toml");
        std::fs::write(&path, toml_src).unwrap();
        let report = validate_one(&path, "synthetic").unwrap();
        assert!(
            report.suspicious_tools.is_empty(),
            "got: {:?}",
            report.suspicious_tools
        );
    }

    #[test]
    fn validator_handles_negative_step_id_in_forward_ref() {
        // claim/-0.1 is real in linear.toml. Validator must resolve it.
        let toml_src = r#"
federation_version = "1"

[[phases]]
id = "claim"

[[phases.steps]]
id = "-0.1"
description = "first guard"

[[phases.steps]]
id = "0.4"
description = "uses -0.1"
artifact_schema = { issue_id = "string (from claim/-0.1)" }
"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("synthetic.toml");
        std::fs::write(&path, toml_src).unwrap();
        let report = validate_one(&path, "synthetic").unwrap();
        assert!(
            report.dangling_refs.is_empty(),
            "got: {:?}",
            report.dangling_refs
        );
    }

    /// Run the validator against the real `config/steps/` directory.
    /// This is the production check — if linear.toml or any other
    /// shipped config has a dangling ref, this catches it.
    ///
    /// `#[ignore]`d by default because it depends on the repo
    /// checkout layout (locates `config/steps/` relative to
    /// `CARGO_MANIFEST_DIR`). Run via:
    ///
    /// ```sh
    /// cargo test --bin sentinel-engine --
    ///   schema_validator::tests::production_step_configs_are_clean --ignored
    /// ```
    ///
    /// Once the test bakes (a few iterations of CI passing it without
    /// human intervention), drop the `#[ignore]` to make it a hard CI
    /// gate.
    #[test]
    #[ignore = "depends on repo layout; run manually via --ignored"]
    fn production_step_configs_are_clean() {
        use std::fmt::Write as _;
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let steps_dir = std::path::Path::new(manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("config")
            .join("steps");
        assert!(
            steps_dir.is_dir(),
            "expected config/steps/ at {}",
            steps_dir.display()
        );
        let reports = validate_all(&steps_dir).unwrap();
        assert!(
            !reports.is_empty(),
            "no skills found in {}",
            steps_dir.display()
        );

        let mut dirty: Vec<&SkillReport> = reports.iter().filter(|r| !r.is_clean()).collect();
        if !dirty.is_empty() {
            dirty.sort_by(|a, b| a.skill.cmp(&b.skill));
            let mut msg = String::from("step-config schema issues found:\n");
            for r in &dirty {
                let _ = write!(msg, "\n  {} ({} steps):\n", r.skill, r.step_count);
                for (p, s, d) in &r.dangling_refs {
                    let _ = writeln!(msg, "    DANGLING REF {p}/{s}: {d}");
                }
                for (p, s, t) in &r.suspicious_tools {
                    let _ = writeln!(msg, "    SUSPICIOUS TOOL {p}/{s}: {t}");
                }
            }
            panic!("{msg}");
        }
    }
}
