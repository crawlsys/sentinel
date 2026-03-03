//! Config Loading
//!
//! Parses hooks.toml and workflows.toml into domain types.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use sentinel_domain::events::HookEvent;
use sentinel_domain::hooks::{HookId, HookSpec};
use sentinel_domain::judge::JudgeModel;
use sentinel_domain::workflow::{SkillWorkflow, WorkflowPhase};

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

fn default_true() -> bool {
    true
}

fn default_judge_str() -> String {
    "sonnet".to_string()
}

/// Default config directory.
///
/// Searches relative to the executable first, then falls back to
/// `~/.claude/sentinel/config`.
#[must_use]
pub fn config_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));

    if let Some(dir) = exe_dir {
        let config = dir.join("config");
        if config.exists() {
            return config;
        }
    }

    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("sentinel")
        .join("config")
}

/// Load hook specs from hooks.toml
pub fn load_hooks(config_path: &Path) -> Result<Vec<HookSpec>> {
    let toml_path = config_path.join("hooks.toml");
    let content = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("Failed to read {}", toml_path.display()))?;

    let config: HooksConfig =
        toml::from_str(&content).context("Failed to parse hooks.toml")?;

    let specs: Vec<HookSpec> = config
        .hooks
        .into_iter()
        .map(|h| HookSpec {
            id: HookId::new(&h.id),
            event: HookEvent::from_arg(&h.event).unwrap_or(HookEvent::Stop),
            matcher: h.matcher,
            depends_on: h.depends_on
                .into_iter()
                .map(|d| HookId::new(&d))
                .collect(),
            has_api_call: h.has_api_call,
        })
        .collect();

    Ok(specs)
}

/// Load workflow definitions from workflows.toml
pub fn load_workflows(config_path: &Path) -> Result<Vec<SkillWorkflow>> {
    let toml_path = config_path.join("workflows.toml");
    let content = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("Failed to read {}", toml_path.display()))?;

    let config: WorkflowsConfig =
        toml::from_str(&content).context("Failed to parse workflows.toml")?;

    let workflows: Vec<SkillWorkflow> = config
        .workflows
        .into_iter()
        .map(|w| SkillWorkflow {
            skill: w.skill,
            phases: w
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
                .collect(),
        })
        .collect();

    Ok(workflows)
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
    fn test_load_hooks_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = load_hooks(dir.path());
        assert!(result.is_err());
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
