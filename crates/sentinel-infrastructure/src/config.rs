//! Config Loading
//!
//! Parses hooks.toml and workflows.toml into domain types.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use sentinel_domain::events::HookEvent;
use sentinel_domain::hooks::{HookId, HookSpec};
use sentinel_domain::judge::JudgeModel;
use sentinel_domain::workflow::{
    PhaseSteps, SkillSteps, SkillWorkflow, WorkflowPhase, WorkflowStep,
};

/// Raw TOML config for hooks
#[derive(Debug, Deserialize)]
struct HooksConfig {
    hooks: Vec<HookToml>,
}

#[derive(Debug, Deserialize)]
struct HookToml {
    id: String,
    event: String,
    #[serde(default)]
    matcher: Vec<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    has_api_call: bool,
    /// Documentation field from TOML — not mapped to domain type
    #[serde(default)]
    #[allow(dead_code)]
    description: String,
}

/// Raw TOML config for workflows
#[derive(Debug, Deserialize)]
struct WorkflowsConfig {
    workflows: Vec<WorkflowToml>,
}

#[derive(Debug, Deserialize)]
struct WorkflowToml {
    skill: String,
    phases: Vec<PhaseToml>,
    /// Tool name prefixes to block when this workflow is active.
    /// E.g., a workflow that wants local browser only could block
    /// `mcp__browserbase__` to force CDP usage.
    #[serde(default)]
    blocked_tool_prefixes: Vec<String>,
    /// Bash command patterns (regex) to block when this workflow is active.
    /// E.g., ["chrome.*--remote-debugging"] blocks CLI escape attempts that
    /// would bypass the controlled CDP MCP. Legacy `steel-mcp` patterns are
    /// still accepted here even though the steel binary is gone (defense
    /// in depth).
    #[serde(default)]
    blocked_bash_patterns: Vec<String>,
    /// Bash command allowlist (regex). When non-empty, ONLY matching commands pass.
    #[serde(default)]
    bash_allowlist: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PhaseToml {
    id: String,
    file: String,
    #[serde(default = "default_true")]
    required: bool,
    #[serde(default = "default_judge_str")]
    judge: String,
    #[serde(default)]
    description: String,
}

const fn default_true() -> bool {
    true
}

fn default_judge_str() -> String {
    "sonnet".to_string()
}

/// Default config directory.
///
/// **Attack #73 fix**: Always use `~/.claude/sentinel/config`. The previous
/// exe-relative lookup (`~/.cargo/bin/config/`) let any user-writable process
/// plant a malicious `workflows.toml` that disables all enforcement.
///
/// **Attack #84 fix**: Panic instead of falling back to `"."` when HOME is unset.
/// The `"."` fallback means CWD (attacker-controlled project dir) becomes the
/// config source — an attacker plants `workflows.toml` in the project with empty
/// enforcement, and sentinel loads it as the real config.
#[must_use]
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory. HOME/USERPROFILE must be set.")
        .join(".claude")
        .join("sentinel")
        .join("config")
}

/// Load hook specs from hooks.toml
pub fn load_hooks(config_path: &Path) -> Result<Vec<HookSpec>> {
    let toml_path = config_path.join("hooks.toml");
    warn_if_world_writable(&toml_path);
    let content = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("Failed to read {}", toml_path.display()))?;

    let config: HooksConfig = toml::from_str(&content).context("Failed to parse hooks.toml")?;

    let mut specs: Vec<HookSpec> = Vec::new();
    for h in config.hooks {
        let event = HookEvent::from_arg(&h.event).ok_or_else(|| {
            anyhow::anyhow!("Unknown hook event type '{}' for hook '{}'", h.event, h.id)
        })?;
        specs.push(HookSpec {
            id: HookId::new(&h.id),
            event,
            matcher: h.matcher,
            depends_on: h.depends_on.into_iter().map(|d| HookId::new(&d)).collect(),
            has_api_call: h.has_api_call,
        });
    }

    Ok(specs)
}

/// **Attack #166 fix**: Warn if config files are world-writable (Unix).
/// A world-writable workflows.toml lets any local user inject workflows
/// with no required phases or disable enforcement entirely.
#[cfg(unix)]
fn warn_if_world_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o002 != 0 {
            eprintln!(
                "[sentinel] SECURITY WARNING: Config file '{}' is world-writable (mode {:04o}). \
                 Other users on this system can modify sentinel enforcement rules. \
                 Fix with: chmod 644 {}",
                path.display(),
                mode & 0o777,
                path.display(),
            );
        }
    }
}

#[cfg(not(unix))]
const fn warn_if_world_writable(_path: &Path) {
    // Windows ACLs are handled by icacls elsewhere; no simple mode check.
}

/// Load workflow definitions from workflows.toml
pub fn load_workflows(config_path: &Path) -> Result<Vec<SkillWorkflow>> {
    let toml_path = config_path.join("workflows.toml");
    warn_if_world_writable(&toml_path);
    let content = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("Failed to read {}", toml_path.display()))?;

    let config: WorkflowsConfig =
        toml::from_str(&content).context("Failed to parse workflows.toml")?;

    let mut workflows: Vec<SkillWorkflow> = Vec::new();
    for w in config.workflows {
        let phases: Vec<WorkflowPhase> = w
            .phases
            .into_iter()
            .map(|p| WorkflowPhase {
                id: p.id,
                file: p.file,
                required: p.required,
                judge: match p.judge.as_str() {
                    "opus" => JudgeModel::Opus,
                    "codex" => JudgeModel::Codex,
                    _ => JudgeModel::Sonnet,
                },
                description: p.description,
            })
            .collect();

        // **Attack #181 fix**: Reject empty skill names.
        // An empty string key in the workflows HashMap creates state confusion
        // and could match unintended skill routing results.
        if w.skill.is_empty() {
            anyhow::bail!("Workflow skill name cannot be empty");
        }

        // **Attack #158 fix**: Reject workflows with zero required phases.
        // A workflow with no required phases creates a zero-enforcement zone:
        // the phase gate never blocks because there are no phases to enforce,
        // but the workflow's presence prevents fallback to other enforcement.
        let required_count = phases.iter().filter(|p| p.required).count();
        if required_count == 0 {
            anyhow::bail!(
                "Workflow '{}' has no required phases. This creates a zero-enforcement zone. \
                 Every workflow must have at least one required phase.",
                w.skill
            );
        }

        // **Attack #179 fix**: Validate all regex patterns at config load time.
        // Catches syntax errors and oversized patterns early instead of at
        // enforcement time where failures could silently skip enforcement.
        for pattern in w
            .blocked_bash_patterns
            .iter()
            .chain(w.bash_allowlist.iter())
        {
            if pattern.len() > 256 {
                anyhow::bail!(
                    "Workflow '{}': regex pattern exceeds 256 chars ({} chars). \
                     This could cause performance issues during enforcement.",
                    w.skill,
                    pattern.len()
                );
            }
            if let Err(e) = regex::Regex::new(pattern) {
                anyhow::bail!(
                    "Workflow '{}': invalid regex pattern '{}': {}",
                    w.skill,
                    pattern,
                    e
                );
            }
        }

        workflows.push(SkillWorkflow {
            skill: w.skill,
            phases,
            blocked_tool_prefixes: w.blocked_tool_prefixes,
            blocked_bash_patterns: w.blocked_bash_patterns,
            bash_allowlist: w.bash_allowlist,
        });
    }

    Ok(workflows)
}

/// Raw TOML config for skill steps
#[derive(Debug, Deserialize)]
struct StepsConfig {
    /// Federation version (M2.7). Defaults to "1" for pre-M2.7 configs.
    /// Bumped on breaking changes — see SkillSteps::federation_version.
    #[serde(default = "default_federation_version_str")]
    federation_version: String,
    phases: Vec<StepsPhaseToml>,
}

fn default_federation_version_str() -> String {
    "1".to_string()
}

#[derive(Debug, Deserialize)]
struct StepsPhaseToml {
    id: String,
    #[serde(default)]
    steps: Vec<StepToml>,
}

#[derive(Debug, Deserialize)]
struct StepToml {
    id: String,
    description: String,
    #[serde(default)]
    blocker: bool,
    /// Cold-start baseline threshold (M1.8). See `WorkflowStep::baseline_threshold`.
    /// Default 0 — enforce immediately. Existing TOML configs without this field
    /// continue to load unchanged.
    #[serde(default)]
    baseline_threshold: u64,
    /// Per-step judge tier (#73). See `WorkflowStep::judge`. None = use the
    /// default tier. TOML: `judge = "codex" | "kimi" | "sonnet" | "opus"`.
    #[serde(default)]
    judge: Option<sentinel_domain::judge::JudgeModel>,
    /// Per-step timeout (M4.4). See `WorkflowStep::timeout_ms`. None = no timeout.
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Retry policy (M4.4). See `WorkflowStep::retry_policy`. Default = no retries.
    #[serde(default)]
    retry_policy: sentinel_domain::workflow::RetryPolicy,
    /// Circuit breaker (M4.4). See `WorkflowStep::circuit_breaker`. Default = disabled.
    #[serde(default)]
    circuit_breaker: sentinel_domain::workflow::CircuitBreaker,
    /// Federation directives (M2.5). See `WorkflowStep::provides/requires/external/inaccessible`.
    #[serde(default)]
    provides: Vec<String>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    external: Vec<String>,
    #[serde(default)]
    inaccessible: bool,
    /// Federation deprecation (M2.6). See `WorkflowStep::deprecated/override`.
    #[serde(default)]
    deprecated: Option<String>,
    #[serde(default, rename = "override")]
    r#override: Option<String>,
    /// Plugin extensibility (M2.9). See `WorkflowStep::extra`. Captured as
    /// `Option<toml::Value>` because TOML and JSON have different native
    /// types (TOML has `Datetime`, JSON has `null`); the loader converts
    /// `None` to `Value::Null` and `Some(v)` via `toml_to_json`.
    #[serde(default)]
    extra: Option<toml::Value>,
}

/// Convert a `toml::Value` into a `serde_json::Value` for the M2.9
/// `extra` plugin metadata field.
///
/// TOML and JSON have overlapping but not identical native types:
/// - TOML `Datetime` → JSON string (ISO-8601 representation, what
///   downstream JSON consumers expect anyway).
/// - TOML doesn't have `null` so `serde_json::Value::Null` only
///   appears via the `Option<toml::Value>` wrapper at the field
///   boundary, not from this function.
/// - All other types (String, Integer, Float, Boolean, Array, Table)
///   map cleanly.
///
/// We do this explicitly rather than via serde round-trip so future
/// edits to the field's TOML/JSON shape are deliberate, not
/// accidentally mediated by some intermediate representation.
fn toml_to_json(v: toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::Value::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        toml::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(tbl) => {
            let map: serde_json::Map<String, serde_json::Value> = tbl
                .into_iter()
                .map(|(k, v)| (k, toml_to_json(v)))
                .collect();
            serde_json::Value::Object(map)
        }
    }
}

/// Load step definitions for a skill from `config/steps/<skill>.toml`
///
/// Returns `None` if the file doesn't exist (steps are optional).
pub fn load_skill_steps(config_path: &Path, skill: &str) -> Result<Option<SkillSteps>> {
    // **Attack #95 fix**: Sanitize skill name before using as path component.
    // Without this, skill="../../etc" reads `config/steps/../../etc.toml` (path traversal).
    if skill.contains('.')
        || skill.contains('/')
        || skill.contains('\\')
        || skill.is_empty()
        || skill.len() > 64
    {
        anyhow::bail!("Invalid skill name for step loading: '{skill}'");
    }
    let toml_path = config_path.join("steps").join(format!("{skill}.toml"));
    if !toml_path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("Failed to read {}", toml_path.display()))?;

    let config: StepsConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", toml_path.display()))?;

    let skill_steps = SkillSteps {
        skill: skill.to_string(),
        federation_version: config.federation_version,
        phases: config
            .phases
            .into_iter()
            .map(|p| PhaseSteps {
                phase_id: p.id,
                steps: p
                    .steps
                    .into_iter()
                    .map(|s| WorkflowStep {
                        id: s.id,
                        description: s.description,
                        blocker: s.blocker,
                        baseline_threshold: s.baseline_threshold,
                        judge: s.judge,
                        timeout_ms: s.timeout_ms,
                        retry_policy: s.retry_policy,
                        circuit_breaker: s.circuit_breaker,
                        provides: s.provides,
                        requires: s.requires,
                        external: s.external,
                        inaccessible: s.inaccessible,
                        deprecated: s.deprecated,
                        r#override: s.r#override,
                        extra: s.extra.map_or(serde_json::Value::Null, toml_to_json),
                    })
                    .collect(),
            })
            .collect(),
    };

    Ok(Some(skill_steps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_load_hooks_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_toml = r#"
[[hooks]]
id = "test-hook"
event = "Stop"
description = "A test hook"
depends_on = []
has_api_call = false

[[hooks]]
id = "api-hook"
event = "UserPromptSubmit"
description = "Hook with API call"
depends_on = ["test-hook"]
has_api_call = true
matcher = ["Edit"]
"#;
        let path = dir.path().join("hooks.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(hooks_toml.as_bytes()).unwrap();

        let specs = load_hooks(dir.path()).unwrap();
        assert_eq!(specs.len(), 2);

        assert_eq!(specs[0].id.as_str(), "test-hook");
        assert_eq!(specs[0].event, HookEvent::Stop);
        assert!(!specs[0].has_api_call);
        assert!(specs[0].depends_on.is_empty());

        assert_eq!(specs[1].id.as_str(), "api-hook");
        assert_eq!(specs[1].event, HookEvent::UserPromptSubmit);
        assert!(specs[1].has_api_call);
        assert_eq!(specs[1].depends_on.len(), 1);
        assert_eq!(specs[1].depends_on[0].as_str(), "test-hook");
        assert_eq!(specs[1].matcher, vec!["Edit"]);
    }

    #[test]
    fn test_load_workflows_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let workflows_toml = r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim the issue"

[[workflows.phases]]
id = "review"
file = "review.md"
required = true
judge = "opus"
description = "Code review"

[[workflows.phases]]
id = "cleanup"
file = "cleanup.md"
required = false
judge = "sonnet"
description = "Cleanup"
"#;
        let path = dir.path().join("workflows.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(workflows_toml.as_bytes()).unwrap();

        let workflows = load_workflows(dir.path()).unwrap();
        assert_eq!(workflows.len(), 1);

        let wf = &workflows[0];
        assert_eq!(wf.skill, "linear");
        assert_eq!(wf.phases.len(), 3);

        assert_eq!(wf.phases[0].id, "claim");
        assert!(wf.phases[0].required);
        assert_eq!(wf.phases[0].judge, JudgeModel::Sonnet);

        assert_eq!(wf.phases[1].id, "review");
        assert_eq!(wf.phases[1].judge, JudgeModel::Opus);

        assert_eq!(wf.phases[2].id, "cleanup");
        assert!(!wf.phases[2].required);
    }

    #[test]
    fn test_load_workflows_rejects_zero_required_phases() {
        let dir = tempfile::tempdir().unwrap();
        let workflows_toml = r#"
[[workflows]]
skill = "sneaky"

[[workflows.phases]]
id = "optional-only"
file = "optional.md"
required = false
judge = "sonnet"
description = "No required phases — zero enforcement"
"#;
        let path = dir.path().join("workflows.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(workflows_toml.as_bytes()).unwrap();

        let result = load_workflows(dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no required phases"),
            "Expected 'no required phases' error, got: {err}"
        );
        assert!(
            err.contains("sneaky"),
            "Expected skill name in error, got: {err}"
        );
    }

    #[test]
    fn test_load_workflows_rejects_empty_skill_name() {
        let dir = tempfile::tempdir().unwrap();
        let workflows_toml = r#"
[[workflows]]
skill = ""

[[workflows.phases]]
id = "phase1"
file = "phase1.md"
required = true
judge = "sonnet"
description = "A phase"
"#;
        let path = dir.path().join("workflows.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(workflows_toml.as_bytes()).unwrap();

        let result = load_workflows(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn test_load_workflows_rejects_invalid_regex() {
        let dir = tempfile::tempdir().unwrap();
        let workflows_toml = r#"
[[workflows]]
skill = "test"
blocked_bash_patterns = ["(unclosed"]

[[workflows.phases]]
id = "phase1"
file = "phase1.md"
required = true
judge = "sonnet"
description = "A phase"
"#;
        let path = dir.path().join("workflows.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(workflows_toml.as_bytes()).unwrap();

        let result = load_workflows(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid regex"));
    }

    #[test]
    fn test_load_hooks_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = load_hooks(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_load_skill_steps_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steps")).unwrap();

        let steps_toml = r#"
[[phases]]
id = "claim"

[[phases.steps]]
id = "-0.1"
description = "Verify issue exists"
blocker = true

[[phases.steps]]
id = "0.1"
description = "Look up started state"

[[phases.steps]]
id = "0.2"
description = "Get current user"

[[phases]]
id = "fetch"

[[phases.steps]]
id = "1.1"
description = "Get issue"

[[phases.steps]]
id = "1.2"
description = "Get comments"
"#;
        let path = dir.path().join("steps").join("linear.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(steps_toml.as_bytes()).unwrap();

        let result = load_skill_steps(dir.path(), "linear").unwrap();
        assert!(result.is_some());

        let steps = result.unwrap();
        assert_eq!(steps.skill, "linear");
        assert_eq!(steps.phases.len(), 2);
        assert_eq!(steps.phases[0].phase_id, "claim");
        assert_eq!(steps.phases[0].steps.len(), 3);
        assert!(steps.phases[0].steps[0].blocker);
        assert!(!steps.phases[0].steps[1].blocker);
        assert_eq!(steps.phases[1].phase_id, "fetch");
        assert_eq!(steps.phases[1].steps.len(), 2);
        assert_eq!(steps.total_steps(), 5);
    }

    #[test]
    fn test_load_skill_steps_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = load_skill_steps(dir.path(), "nonexistent").unwrap();
        assert!(result.is_none());
    }

    // ─── M2.7 federation_version tests ─────────────────────────────────

    #[test]
    fn federation_version_defaults_to_one_when_omitted() {
        // Pre-M2.7 configs (no federation_version field) must continue
        // to load. Serde's #[serde(default)] fills in "1" so downstream
        // code can always assume the field is populated.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steps")).unwrap();
        let toml = r#"
[[phases]]
id = "claim"

[[phases.steps]]
id = "1"
description = "fetch"
"#;
        let path = dir.path().join("steps").join("legacy.toml");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(toml.as_bytes())
            .unwrap();
        let result = load_skill_steps(dir.path(), "legacy").unwrap().unwrap();
        assert_eq!(result.federation_version, "1");
    }

    #[test]
    fn federation_version_round_trips_when_set_explicitly() {
        // Configs that DO declare federation_version preserve it through
        // the loader. New skills that opt into M2.7 explicitly will see
        // the version they wrote, not the default.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steps")).unwrap();
        let toml = r#"
federation_version = "2"

[[phases]]
id = "claim"

[[phases.steps]]
id = "1"
description = "fetch"
"#;
        let path = dir.path().join("steps").join("modern.toml");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(toml.as_bytes())
            .unwrap();
        let result = load_skill_steps(dir.path(), "modern").unwrap().unwrap();
        assert_eq!(result.federation_version, "2");
    }

    #[test]
    fn federation_version_accepts_arbitrary_string_values() {
        // The field is `String`, not an enum — bumps can be "2",
        // "2025-05-06", or anything operators want as long as it's
        // unique per breaking change. The loader doesn't impose a
        // grammar; M2.8 federation check compares versions for
        // inequality, not for ordering.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steps")).unwrap();
        let toml = r#"
federation_version = "2026-05-06-pre-deploy-cutover"

[[phases]]
id = "claim"

[[phases.steps]]
id = "1"
description = "fetch"
"#;
        let path = dir.path().join("steps").join("dated.toml");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(toml.as_bytes())
            .unwrap();
        let result = load_skill_steps(dir.path(), "dated").unwrap().unwrap();
        assert_eq!(
            result.federation_version,
            "2026-05-06-pre-deploy-cutover",
        );
    }

    // ─── M2.5 federation directives tests ─────────────────────────────
    //
    // These cover the four directive fields added in M2.5:
    // `provides`, `requires`, `external`, and `inaccessible`. The loader
    // must (1) accept legacy configs that don't declare them (empty
    // defaults), (2) preserve declared values verbatim, and (3) round-
    // trip the boolean flag. Federation compose (M2.4) consumes these
    // values to validate cross-skill contracts.

    #[test]
    fn federation_directives_default_to_empty_when_omitted() {
        // Pre-M2.5 configs without any directive fields load with empty
        // Vec defaults and inaccessible=false. This is the backwards-
        // compat safety net — every existing skill TOML in the repo
        // must continue to load unchanged.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steps")).unwrap();
        let toml = r#"
[[phases]]
id = "claim"

[[phases.steps]]
id = "1"
description = "fetch"
"#;
        let path = dir.path().join("steps").join("nodirectives.toml");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(toml.as_bytes())
            .unwrap();
        let result = load_skill_steps(dir.path(), "nodirectives")
            .unwrap()
            .unwrap();
        let step = &result.phases[0].steps[0];
        assert!(step.provides.is_empty());
        assert!(step.requires.is_empty());
        assert!(step.external.is_empty());
        assert!(!step.inaccessible);
    }

    #[test]
    fn federation_directives_round_trip_when_declared() {
        // Configs that DO declare directive arrays preserve every
        // element verbatim through the loader. This is the contract
        // that M2.4 federation compose depends on — if a step's
        // declared `provides` got stripped, compose couldn't validate
        // that downstream `requires` are satisfiable.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steps")).unwrap();
        let toml = r#"
[[phases]]
id = "open_pr"

[[phases.steps]]
id = "1"
description = "create the PR"
provides = ["git.pr_url", "git.pr_number"]
requires = ["linear.ticket_id", "git.branch_name"]
external = ["linear.claim.3"]
"#;
        let path = dir.path().join("steps").join("withdirectives.toml");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(toml.as_bytes())
            .unwrap();
        let result = load_skill_steps(dir.path(), "withdirectives")
            .unwrap()
            .unwrap();
        let step = &result.phases[0].steps[0];
        assert_eq!(step.provides, vec!["git.pr_url", "git.pr_number"]);
        assert_eq!(
            step.requires,
            vec!["linear.ticket_id", "git.branch_name"]
        );
        assert_eq!(step.external, vec!["linear.claim.3"]);
        assert!(!step.inaccessible);
    }

    #[test]
    fn federation_inaccessible_flag_round_trips() {
        // Internal-only steps mark themselves inaccessible=true. The
        // future router (M7) must not include these in virtual skill
        // packs — they're skill-internal helpers other steps in the
        // same skill chain into. The flag has to survive the loader
        // so the router can filter on it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steps")).unwrap();
        let toml = r#"
[[phases]]
id = "internal"

[[phases.steps]]
id = "helper"
description = "skill-internal helper"
inaccessible = true
"#;
        let path = dir.path().join("steps").join("internalonly.toml");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(toml.as_bytes())
            .unwrap();
        let result = load_skill_steps(dir.path(), "internalonly")
            .unwrap()
            .unwrap();
        let step = &result.phases[0].steps[0];
        assert!(step.inaccessible);
        // Other directives still default cleanly.
        assert!(step.provides.is_empty());
        assert!(step.requires.is_empty());
        assert!(step.external.is_empty());
    }

    // ─── M2.6 deprecation/migration directives tests ──────────────────
    //
    // Cover `deprecated: Option<String>` and `override: Option<String>`
    // round-trip plus default behavior. The compose-side validation
    // (warn on deprecated usage, error on dangling override targets)
    // is tested in `federation_cmd.rs`.

    #[test]
    fn deprecation_fields_default_to_none_when_omitted() {
        // Pre-M2.6 configs that don't declare the fields load with
        // `None` for both. This is the backwards-compat invariant —
        // adding M2.6 must not break any existing skill TOML.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steps")).unwrap();
        let toml = r#"
[[phases]]
id = "claim"

[[phases.steps]]
id = "1"
description = "fetch"
"#;
        let path = dir.path().join("steps").join("nodepr.toml");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(toml.as_bytes())
            .unwrap();
        let result = load_skill_steps(dir.path(), "nodepr").unwrap().unwrap();
        let step = &result.phases[0].steps[0];
        assert!(step.deprecated.is_none());
        assert!(step.r#override.is_none());
    }

    #[test]
    fn deprecated_string_round_trips() {
        // Configs that DO declare `deprecated = "..."` preserve the
        // exact migration message. This is what compose surfaces in
        // its warning, so it has to round-trip verbatim.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steps")).unwrap();
        let toml = r#"
[[phases]]
id = "claim"

[[phases.steps]]
id = "1"
description = "fetch ticket (legacy)"
deprecated = "Use claim.2 — fetches by ID, not URL"
"#;
        let path = dir.path().join("steps").join("legacy.toml");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(toml.as_bytes())
            .unwrap();
        let result = load_skill_steps(dir.path(), "legacy").unwrap().unwrap();
        let step = &result.phases[0].steps[0];
        assert_eq!(
            step.deprecated.as_deref(),
            Some("Use claim.2 — fetches by ID, not URL"),
        );
    }

    #[test]
    fn override_field_round_trips() {
        // Configs declaring `override = "phase.step_id"` preserve
        // the exact target reference. Compose uses this to validate
        // the target exists and is itself deprecated.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steps")).unwrap();
        let toml = r#"
[[phases]]
id = "claim"

[[phases.steps]]
id = "old"
description = "fetch (legacy)"
deprecated = "Use claim.new"

[[phases.steps]]
id = "new"
description = "fetch (modern)"
override = "claim.old"
"#;
        let path = dir.path().join("steps").join("withoverride.toml");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(toml.as_bytes())
            .unwrap();
        let result = load_skill_steps(dir.path(), "withoverride")
            .unwrap()
            .unwrap();
        let new_step = &result.phases[0].steps[1];
        assert_eq!(new_step.id, "new");
        assert_eq!(new_step.r#override.as_deref(), Some("claim.old"));
    }

    // ─── M2.9 plugin extensibility tests ─────────────────────────────
    //
    // Cover the `extra: serde_json::Value` field round-trip and the
    // `toml_to_json` conversion. Plugins (lenses, custom routers,
    // telemetry adapters) read their own keys out of this opaque
    // value — the core only guarantees round-trip preservation, not
    // schema validation. M4.8 brings Pydantic-style validation in.

    #[test]
    fn extra_field_defaults_to_null_when_omitted() {
        // Pre-M2.9 configs that don't declare `extra` load with
        // `serde_json::Value::Null`. Backwards-compat invariant.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steps")).unwrap();
        let toml = r#"
[[phases]]
id = "claim"

[[phases.steps]]
id = "1"
description = "fetch"
"#;
        let path = dir.path().join("steps").join("noextra.toml");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(toml.as_bytes())
            .unwrap();
        let result = load_skill_steps(dir.path(), "noextra").unwrap().unwrap();
        let step = &result.phases[0].steps[0];
        assert!(step.extra.is_null());
    }

    #[test]
    fn extra_field_round_trips_nested_table() {
        // The realistic plugin shape: a nested table where each plugin
        // owns a top-level key. This is what enables the M2.9 contract
        // — multiple plugins coexist on one step without colliding.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steps")).unwrap();
        let toml = r#"
[[phases]]
id = "review"

[[phases.steps]]
id = "1"
description = "code review"

[phases.steps.extra.lens.code_review]
rubric = "owasp"
weight = 0.7

[phases.steps.extra.telemetry]
skip = true
"#;
        let path = dir.path().join("steps").join("withextra.toml");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(toml.as_bytes())
            .unwrap();
        let result = load_skill_steps(dir.path(), "withextra").unwrap().unwrap();
        let step = &result.phases[0].steps[0];
        // Drill into the nested structure plugins would consume.
        assert_eq!(
            step.extra
                .pointer("/lens/code_review/rubric")
                .and_then(|v| v.as_str()),
            Some("owasp"),
        );
        assert_eq!(
            step.extra
                .pointer("/lens/code_review/weight")
                .and_then(serde_json::Value::as_f64),
            Some(0.7),
        );
        assert_eq!(
            step.extra
                .pointer("/telemetry/skip")
                .and_then(serde_json::Value::as_bool),
            Some(true),
        );
    }

    #[test]
    fn toml_to_json_converts_primitive_types() {
        // Direct unit tests on the conversion helper. Each TOML
        // primitive maps to the expected JSON shape.
        assert_eq!(
            toml_to_json(toml::Value::String("hi".into())),
            serde_json::Value::String("hi".into()),
        );
        assert_eq!(
            toml_to_json(toml::Value::Integer(42)),
            serde_json::json!(42),
        );
        assert_eq!(
            toml_to_json(toml::Value::Boolean(true)),
            serde_json::Value::Bool(true),
        );
        let f = toml_to_json(toml::Value::Float(0.5));
        assert_eq!(f.as_f64(), Some(0.5));
    }

    #[test]
    fn toml_to_json_converts_nested_array_and_table() {
        // Compound types: array of tables, the shape plugins use.
        let inner = toml::value::Table::from_iter([
            ("name".to_string(), toml::Value::String("a".into())),
            ("score".to_string(), toml::Value::Integer(10)),
        ]);
        let arr = toml::Value::Array(vec![toml::Value::Table(inner)]);
        let json = toml_to_json(arr);
        assert_eq!(
            json.pointer("/0/name").and_then(|v| v.as_str()),
            Some("a"),
        );
        assert_eq!(
            json.pointer("/0/score").and_then(serde_json::Value::as_i64),
            Some(10),
        );
    }

    #[test]
    fn toml_to_json_converts_nan_to_null() {
        // NaN and Infinity have no JSON representation. Rather than
        // panic or write invalid JSON, we coerce to `null`. Plugins
        // requiring strict numeric values catch this at their
        // boundary (M4.8 fail-fast validation).
        let nan = toml_to_json(toml::Value::Float(f64::NAN));
        assert!(nan.is_null());
    }

    #[test]
    fn test_load_workflows_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflows.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"this is not valid toml {{{").unwrap();

        let result = load_workflows(dir.path());
        assert!(result.is_err());
    }
}
