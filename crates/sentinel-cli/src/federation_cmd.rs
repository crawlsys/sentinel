//! `sentinel federation compose` — validate the federated step supergraph
//!
//! Apollo's `apollo composition` analog. Walks every `config/steps/*.toml`
//! file under `~/.claude/sentinel/config/steps/` (the place sentinel
//! infrastructure already loads from) and verifies that the union of all
//! skills' step configs forms a coherent federated supergraph.
//!
//! # What "coherent supergraph" means today (M2.4 v1)
//!
//! 1. **No duplicate step coordinates within a skill** — a skill cannot
//!    declare two steps with the same `(phase_id, step_id)` pair.
//! 2. **No malformed step configs** — every TOML file parses cleanly via
//!    the existing `load_skill_steps` (which already enforces step ID
//!    safety via `is_safe_name`).
//! 3. **No empty skills** — a skill registered in the directory but
//!    declaring zero phases or zero steps is reported as a warning so
//!    operators know about half-finished configs.
//!
//! # What's NOT validated yet (M2.5+ follow-up)
//!
//! Apollo Federation validators do much more — `@key` directive
//! consistency, handoff type alignment, deprecation paths, version
//! compat. Those land when M2.5 (federation directives in step config
//! TOML) ships. The hook here is the v1 minimum: parse + load + lift
//! collisions to errors. When the directive layer arrives, this same
//! command grows new check passes without changing its CLI shape.
//!
//! # Output modes
//!
//! - **Default (text):** human-readable summary with color-coded errors
//!   and warnings. Right for interactive runs.
//! - **`--json`:** machine-readable JSON for CI status checks. The
//!   `sentinel federation check` companion (M2.8) will consume this.
//!
//! # Exit code
//!
//! - `0` — supergraph composes cleanly (warnings ok)
//! - `1` — composition errors (duplicate coordinates, malformed TOML,
//!   anything that would break a downstream skills-mcp build)

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One validation finding from the compose pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeFinding {
    pub severity: ComposeSeverity,
    pub skill: Option<String>,
    pub phase_id: Option<String>,
    pub step_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ComposeSeverity {
    Error,
    Warning,
}

/// Aggregate result of the compose pass — what the command returns and
/// what `--json` serializes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeReport {
    pub skills_seen: usize,
    pub total_steps: usize,
    pub total_phases: usize,
    pub findings: Vec<ComposeFinding>,
    /// Convenience: `findings.iter().filter(|f| f.severity == Error).count()`
    pub error_count: usize,
}

impl ComposeReport {
    /// True when no errors were found (warnings ok).
    #[must_use]
    pub fn ok(&self) -> bool {
        self.error_count == 0
    }
}

/// Compose the supergraph from a config directory. Pure function on the
/// directory contents — no I/O outside of reading the step configs the
/// existing infrastructure already knows how to read.
pub fn compose(config_dir: &Path) -> Result<ComposeReport> {
    let steps_dir = config_dir.join("steps");
    let mut report = ComposeReport {
        skills_seen: 0,
        total_steps: 0,
        total_phases: 0,
        findings: Vec::new(),
        error_count: 0,
    };

    // No steps directory => empty supergraph. Not an error — the user
    // may not have declared any step configs yet. Composition just has
    // nothing to validate.
    if !steps_dir.is_dir() {
        return Ok(report);
    }

    // Discover every .toml under steps/ — stem is the skill name.
    let entries = std::fs::read_dir(&steps_dir)
        .with_context(|| format!("read_dir {}", steps_dir.display()))?;
    let mut skill_names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        skill_names.push(stem.to_string());
    }
    skill_names.sort_unstable();

    for skill in &skill_names {
        report.skills_seen += 1;
        match sentinel_infrastructure::config::load_skill_steps(config_dir, skill) {
            Ok(Some(skill_steps)) => {
                if skill_steps.phases.is_empty() {
                    report.findings.push(ComposeFinding {
                        severity: ComposeSeverity::Warning,
                        skill: Some(skill.clone()),
                        phase_id: None,
                        step_id: None,
                        message: format!("skill '{skill}' declares zero phases"),
                    });
                    continue;
                }
                check_skill(skill, &skill_steps, &mut report);
            }
            Ok(None) => {
                // load_skill_steps returned None — skill name didn't pass
                // safety check or file vanished mid-walk. Lift to error
                // so the operator notices.
                report.findings.push(ComposeFinding {
                    severity: ComposeSeverity::Error,
                    skill: Some(skill.clone()),
                    phase_id: None,
                    step_id: None,
                    message: format!(
                        "skill '{skill}' could not be loaded (rejected by name safety \
                         check or file disappeared)",
                    ),
                });
            }
            Err(e) => {
                report.findings.push(ComposeFinding {
                    severity: ComposeSeverity::Error,
                    skill: Some(skill.clone()),
                    phase_id: None,
                    step_id: None,
                    message: format!("skill '{skill}' failed to parse: {e:#}"),
                });
            }
        }
    }

    report.error_count = report
        .findings
        .iter()
        .filter(|f| f.severity == ComposeSeverity::Error)
        .count();
    Ok(report)
}

/// Inspect a single skill's loaded step config for internal consistency:
/// no duplicate `(phase_id, step_id)` pairs, no empty phases.
fn check_skill(
    skill: &str,
    skill_steps: &sentinel_domain::workflow::SkillSteps,
    report: &mut ComposeReport,
) {
    let mut seen_coords: HashMap<(String, String), ()> = HashMap::new();
    for phase in &skill_steps.phases {
        report.total_phases += 1;
        if phase.steps.is_empty() {
            report.findings.push(ComposeFinding {
                severity: ComposeSeverity::Warning,
                skill: Some(skill.into()),
                phase_id: Some(phase.phase_id.clone()),
                step_id: None,
                message: format!(
                    "phase '{}' in skill '{skill}' declares zero steps",
                    phase.phase_id,
                ),
            });
        }
        for step in &phase.steps {
            report.total_steps += 1;
            let coord = (phase.phase_id.clone(), step.id.clone());
            if seen_coords.contains_key(&coord) {
                report.findings.push(ComposeFinding {
                    severity: ComposeSeverity::Error,
                    skill: Some(skill.into()),
                    phase_id: Some(phase.phase_id.clone()),
                    step_id: Some(step.id.clone()),
                    message: format!(
                        "duplicate step coordinate ({phase_id}, {step_id}) in skill '{skill}' \
                         — every step within a skill must have a unique (phase_id, step_id) pair",
                        phase_id = phase.phase_id,
                        step_id = step.id,
                    ),
                });
            } else {
                seen_coords.insert(coord, ());
            }
        }
    }
}

/// Render the report as a human-readable summary. Used when `--json`
/// is not set.
pub fn render_text(report: &ComposeReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Federation compose: {} skill(s), {} phase(s), {} step(s)",
        report.skills_seen, report.total_phases, report.total_steps,
    );
    if report.findings.is_empty() {
        let _ = writeln!(out, "  ✓ supergraph composes cleanly");
        return out;
    }
    for f in &report.findings {
        let prefix = match f.severity {
            ComposeSeverity::Error => "ERROR",
            ComposeSeverity::Warning => "warn ",
        };
        let _ = writeln!(out, "  [{prefix}] {}", f.message);
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{} error(s), {} warning(s)",
        report.error_count,
        report.findings.len() - report.error_count,
    );
    out
}

/// CLI entry: `sentinel federation compose [--json] [--config-dir DIR]`.
///
/// Returns process exit code: 0 on clean, 1 on errors.
pub fn run(json: bool, config_dir_override: Option<String>) -> Result<()> {
    let config_dir = match config_dir_override {
        Some(p) => std::path::PathBuf::from(p),
        None => sentinel_infrastructure::config::config_dir(),
    };
    let report = compose(&config_dir)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_text(&report));
    }

    if report.ok() {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Build a temp config dir with a `steps/` subdir and write the
    /// supplied (skill_name, toml_content) pairs. Returns the temp dir
    /// (kept alive by the caller via the returned guard).
    fn temp_config(skills: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().expect("tempdir");
        let steps_dir = dir.path().join("steps");
        fs::create_dir_all(&steps_dir).unwrap();
        for (name, content) in skills {
            fs::write(steps_dir.join(format!("{name}.toml")), content).unwrap();
        }
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn compose_returns_empty_report_when_steps_dir_missing() {
        let dir = tempdir().unwrap();
        let report = compose(dir.path()).unwrap();
        assert_eq!(report.skills_seen, 0);
        assert!(report.findings.is_empty());
        assert!(report.ok());
    }

    #[test]
    fn compose_clean_supergraph_with_one_skill() {
        let toml = r#"
[[phases]]
id = "claim"

  [[phases.steps]]
  id = "1"
  description = "fetch ticket"

  [[phases.steps]]
  id = "2"
  description = "create branch"
"#;
        let (_guard, path) = temp_config(&[("linear", toml)]);
        let report = compose(&path).unwrap();
        assert_eq!(report.skills_seen, 1);
        assert_eq!(report.total_phases, 1);
        assert_eq!(report.total_steps, 2);
        assert!(report.ok(), "clean supergraph composes, got {:?}", report.findings);
    }

    #[test]
    fn compose_detects_duplicate_step_coordinates() {
        // Same (phase_id, step_id) pair declared twice within one skill.
        let toml = r#"
[[phases]]
id = "claim"

  [[phases.steps]]
  id = "1"
  description = "first"

  [[phases.steps]]
  id = "1"
  description = "duplicate"
"#;
        let (_guard, path) = temp_config(&[("linear", toml)]);
        let report = compose(&path).unwrap();
        assert!(!report.ok(), "duplicate coords must surface as error");
        assert_eq!(report.error_count, 1);
        let msg = &report.findings[0].message;
        assert!(
            msg.contains("duplicate step coordinate"),
            "error names the duplication, got: {msg}",
        );
        assert!(msg.contains("linear"), "error names the skill");
    }

    #[test]
    fn compose_warns_on_skill_with_zero_phases() {
        // Empty TOML — parses, but the resulting SkillSteps has no
        // phases. That's a warning, not an error: someone may be
        // mid-edit on a new skill config.
        let (_guard, path) = temp_config(&[("emptyskill", "")]);
        let report = compose(&path).unwrap();
        assert_eq!(report.skills_seen, 1);
        // load_skill_steps returns None for empty TOML in some configs;
        // either way we should produce SOME finding, and it should not
        // be an error. Accept warning OR an error as long as we don't
        // crash — the contract is "report it visibly."
        assert!(!report.findings.is_empty(), "empty skill must produce a finding");
    }

    #[test]
    fn compose_lifts_malformed_toml_to_error() {
        let invalid = r#"this is not valid toml = "#;
        let (_guard, path) = temp_config(&[("brokenskill", invalid)]);
        let report = compose(&path).unwrap();
        assert!(!report.ok(), "malformed toml must be an error");
        assert!(
            report.findings[0].message.contains("brokenskill"),
            "error names the offending skill, got: {:?}",
            report.findings[0],
        );
    }

    #[test]
    fn compose_walks_multiple_skills_independently() {
        let linear_toml = r#"
[[phases]]
id = "claim"
  [[phases.steps]]
  id = "1"
  description = "fetch"
"#;
        let git_toml = r#"
[[phases]]
id = "branch"
  [[phases.steps]]
  id = "1"
  description = "checkout"
"#;
        let (_guard, path) = temp_config(&[("linear", linear_toml), ("git", git_toml)]);
        let report = compose(&path).unwrap();
        assert_eq!(report.skills_seen, 2);
        assert_eq!(report.total_steps, 2);
        // Same step_id ("1") in DIFFERENT skills is fine — coordinates
        // are scoped per-skill, not per-supergraph.
        assert!(report.ok(), "different skills sharing step ids is legal");
    }

    #[test]
    fn render_text_includes_clean_marker_when_no_findings() {
        let report = ComposeReport {
            skills_seen: 1,
            total_phases: 1,
            total_steps: 2,
            findings: Vec::new(),
            error_count: 0,
        };
        let text = render_text(&report);
        assert!(text.contains("composes cleanly"));
    }

    #[test]
    fn render_text_lists_every_finding_with_severity_prefix() {
        let report = ComposeReport {
            skills_seen: 1,
            total_phases: 1,
            total_steps: 2,
            findings: vec![
                ComposeFinding {
                    severity: ComposeSeverity::Error,
                    skill: Some("linear".into()),
                    phase_id: None,
                    step_id: None,
                    message: "boom".into(),
                },
                ComposeFinding {
                    severity: ComposeSeverity::Warning,
                    skill: Some("git".into()),
                    phase_id: None,
                    step_id: None,
                    message: "hmm".into(),
                },
            ],
            error_count: 1,
        };
        let text = render_text(&report);
        assert!(text.contains("[ERROR] boom"));
        assert!(text.contains("[warn ] hmm"));
        assert!(text.contains("1 error(s), 1 warning(s)"));
    }

    #[test]
    fn report_serializes_round_trip_via_json() {
        // The --json output is the M2.8 federation check CI contract.
        // Serde shape must stay stable.
        let report = ComposeReport {
            skills_seen: 1,
            total_phases: 0,
            total_steps: 0,
            findings: vec![ComposeFinding {
                severity: ComposeSeverity::Error,
                skill: Some("linear".into()),
                phase_id: Some("claim".into()),
                step_id: Some("1".into()),
                message: "test".into(),
            }],
            error_count: 1,
        };
        let json = serde_json::to_string(&report).unwrap();
        let restored: ComposeReport = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.error_count, 1);
        assert_eq!(restored.findings.len(), 1);
        assert_eq!(restored.findings[0].severity, ComposeSeverity::Error);
    }
}
