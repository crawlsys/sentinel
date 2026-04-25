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
    /// E.g., ["`mcp__cdp`__", "`mcp__edge_cdp`__"] blocks CDP tools when steel is active.
    #[serde(default)]
    blocked_tool_prefixes: Vec<String>,
    /// Bash command patterns (regex) to block when this workflow is active.
    /// E.g., ["steel-mcp", "chrome.*--remote-debugging"] blocks CLI escape attempts.
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
                    "haiku" => JudgeModel::Haiku,
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
    phases: Vec<StepsPhaseToml>,
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
