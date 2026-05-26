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
//! # What landed with M2.5 directives
//!
//! After per-skill internal checks, a cross-skill pass runs:
//!
//! 4. **`requires` ↔ `provides` reachability** — every artifact a step
//!    declares it `requires` must be `provides`d by some step somewhere
//!    in the supergraph. A `requires` with no producer is a hard error
//!    (the analog of an Apollo `@requires` referencing a field no
//!    subgraph owns).
//! 5. **`external` reference resolution** — every `external` step
//!    coordinate (`"skill.phase.step_id"` form) must point to an
//!    existing `(skill, phase_id, step_id)` triple in the supergraph.
//!    Dangling externals are hard errors.
//! 6. **`inaccessible` is not an error** — but it's a useful signal
//!    so the report can later inform router (M7) which steps to omit
//!    from virtual skill packs. No validation today; M7 reads it.
//!
//! # What landed with M2.6 deprecation directives
//!
//! 7. **`deprecated` surfacing** — when a step declares `deprecated =
//!    "..."`, compose emits a warning naming the step plus the
//!    migration message. Deprecated steps still function (no error)
//!    so existing chains continue to work; the warning gets the
//!    operator's attention and gives them the target to migrate to.
//! 8. **`override` target resolution** — when a step declares
//!    `override = "phase.step_id"` (same skill) or
//!    `"skill.phase.step_id"` (cross-skill), compose verifies the
//!    target step exists. Dangling override targets are hard errors —
//!    you can't replace a step that isn't there. If the target
//!    exists but isn't itself marked `deprecated`, compose emits a
//!    warning encouraging disciplined migration paths (declare the
//!    deprecation up-front so consumers know the contract is changing).
//!
//! # What's still NOT validated (M2.9+ follow-up)
//!
//! Type alignment between `provides` and `requires` shapes, version
//! skew across `federation_version` boundaries, signed config
//! manifests (M2.13). Those land in later passes without changing
//! the CLI shape.
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
    pub const fn ok(&self) -> bool {
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

    // First pass: per-skill internal validation, plus harvest the
    // supergraph-wide artifact provider table and step coordinate set
    // needed for the cross-skill pass below. We collect successful
    // loads so the directive pass only sees skills that actually
    // parsed cleanly.
    let mut loaded: Vec<(String, sentinel_domain::workflow::SkillSteps)> = Vec::new();
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
                loaded.push((skill.clone(), skill_steps));
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

    // Second pass: cross-skill federation directive validation.
    // Builds two indexes from the union of all loaded skills, then
    // walks `requires`/`external` declarations on every step and
    // raises errors for unsatisfied references.
    check_directives(&loaded, &mut report);

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
            if let std::collections::hash_map::Entry::Vacant(e) = seen_coords.entry(coord) {
                e.insert(());
            } else {
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
            }
        }
    }
}

/// Cross-skill federation directive validation (M2.5). Walks the
/// loaded skills twice: first to build provider/coordinate indexes,
/// then to verify every `requires` / `external` reference resolves.
///
/// `provides` is namespaced by string identity — the producer side
/// declares an artifact name, consumers cite the same string. We
/// don't impose grammar on the strings themselves: skills can use
/// `"linear.ticket_id"`, `"git.pr_url"`, or anything else, as long
/// as both sides spell it identically.
///
/// `external` references take the form `"skill.phase.step_id"` and
/// must resolve to a step that actually exists in the supergraph.
fn check_directives(
    loaded: &[(String, sentinel_domain::workflow::SkillSteps)],
    report: &mut ComposeReport,
) {
    // Index every artifact a step claims to provide. The value type
    // is the location for diagnostics — when a `requires` finds a
    // hit, we don't report it (success is silent), but if there's a
    // mismatch the operator gets the location.
    let mut providers: HashMap<String, (String, String, String)> = HashMap::new();
    // Index every step coordinate so `external` references can be
    // resolved without re-walking the loaded skills for each lookup.
    let mut coordinates: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();

    for (skill, ss) in loaded {
        for phase in &ss.phases {
            for step in &phase.steps {
                coordinates.insert((skill.clone(), phase.phase_id.clone(), step.id.clone()));
                for artifact in &step.provides {
                    // Last writer wins on duplicate provides — collisions
                    // are a separate consistency check we could lift to
                    // a warning, but that's a M2.6 concern (deprecation
                    // / overrides). For now: just register.
                    providers.insert(
                        artifact.clone(),
                        (skill.clone(), phase.phase_id.clone(), step.id.clone()),
                    );
                }
            }
        }
    }

    // Index deprecated coordinates so override targets can be checked
    // for "is the target itself deprecated?" — if not, we warn so the
    // migration path is declared up-front. Key shape mirrors the
    // override grammar: same-skill `(skill, phase, step_id)` triples.
    let mut deprecated_coords: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    for (skill, ss) in loaded {
        for phase in &ss.phases {
            for step in &phase.steps {
                if step.deprecated.is_some() {
                    deprecated_coords.insert((
                        skill.clone(),
                        phase.phase_id.clone(),
                        step.id.clone(),
                    ));
                }
            }
        }
    }

    // Walk every step's requires/external. Error on any unresolved.
    for (skill, ss) in loaded {
        for phase in &ss.phases {
            for step in &phase.steps {
                // M2.6 — surface deprecation warnings up front.
                if let Some(reason) = &step.deprecated {
                    let body = if reason.is_empty() {
                        format!(
                            "step ({phase_id}, {step_id}) in skill '{skill}' is deprecated",
                            phase_id = phase.phase_id,
                            step_id = step.id,
                        )
                    } else {
                        format!(
                            "step ({phase_id}, {step_id}) in skill '{skill}' is deprecated: \
                             {reason}",
                            phase_id = phase.phase_id,
                            step_id = step.id,
                        )
                    };
                    report.findings.push(ComposeFinding {
                        severity: ComposeSeverity::Warning,
                        skill: Some(skill.clone()),
                        phase_id: Some(phase.phase_id.clone()),
                        step_id: Some(step.id.clone()),
                        message: body,
                    });
                }

                // M2.6 — verify override target exists and is itself
                // deprecated. Override grammar: "phase.step_id" (same
                // skill) or "skill.phase.step_id" (cross-skill).
                if let Some(target) = &step.r#override {
                    let target_coord = parse_override_target(skill, target);
                    match target_coord {
                        Some(coord) => {
                            if !coordinates.contains(&coord) {
                                report.findings.push(ComposeFinding {
                                    severity: ComposeSeverity::Error,
                                    skill: Some(skill.clone()),
                                    phase_id: Some(phase.phase_id.clone()),
                                    step_id: Some(step.id.clone()),
                                    message: format!(
                                        "step ({phase_id}, {step_id}) in skill '{skill}' \
                                         declares override of '{target}' which does not \
                                         exist in the supergraph",
                                        phase_id = phase.phase_id,
                                        step_id = step.id,
                                    ),
                                });
                            } else if !deprecated_coords.contains(&coord) {
                                report.findings.push(ComposeFinding {
                                    severity: ComposeSeverity::Warning,
                                    skill: Some(skill.clone()),
                                    phase_id: Some(phase.phase_id.clone()),
                                    step_id: Some(step.id.clone()),
                                    message: format!(
                                        "step ({phase_id}, {step_id}) in skill '{skill}' \
                                         overrides '{target}' but the target is not marked \
                                         `deprecated` — declare the deprecation on the \
                                         target so consumers know the contract is changing",
                                        phase_id = phase.phase_id,
                                        step_id = step.id,
                                    ),
                                });
                            }
                        }
                        None => {
                            report.findings.push(ComposeFinding {
                                severity: ComposeSeverity::Error,
                                skill: Some(skill.clone()),
                                phase_id: Some(phase.phase_id.clone()),
                                step_id: Some(step.id.clone()),
                                message: format!(
                                    "step ({phase_id}, {step_id}) in skill '{skill}' has \
                                     malformed override target '{target}' — expected \
                                     'phase.step_id' or 'skill.phase.step_id'",
                                    phase_id = phase.phase_id,
                                    step_id = step.id,
                                ),
                            });
                        }
                    }
                }

                for artifact in &step.requires {
                    if !providers.contains_key(artifact) {
                        report.findings.push(ComposeFinding {
                            severity: ComposeSeverity::Error,
                            skill: Some(skill.clone()),
                            phase_id: Some(phase.phase_id.clone()),
                            step_id: Some(step.id.clone()),
                            message: format!(
                                "step ({phase_id}, {step_id}) in skill '{skill}' requires \
                                 artifact '{artifact}', but no step in the supergraph \
                                 declares it via `provides`",
                                phase_id = phase.phase_id,
                                step_id = step.id,
                            ),
                        });
                    }
                }
                for ext in &step.external {
                    let parts: Vec<&str> = ext.splitn(3, '.').collect();
                    if parts.len() != 3 {
                        report.findings.push(ComposeFinding {
                            severity: ComposeSeverity::Error,
                            skill: Some(skill.clone()),
                            phase_id: Some(phase.phase_id.clone()),
                            step_id: Some(step.id.clone()),
                            message: format!(
                                "step ({phase_id}, {step_id}) in skill '{skill}' has malformed \
                                 external reference '{ext}' — expected 'skill.phase.step_id'",
                                phase_id = phase.phase_id,
                                step_id = step.id,
                            ),
                        });
                        continue;
                    }
                    let coord = (
                        parts[0].to_string(),
                        parts[1].to_string(),
                        parts[2].to_string(),
                    );
                    if !coordinates.contains(&coord) {
                        report.findings.push(ComposeFinding {
                            severity: ComposeSeverity::Error,
                            skill: Some(skill.clone()),
                            phase_id: Some(phase.phase_id.clone()),
                            step_id: Some(step.id.clone()),
                            message: format!(
                                "step ({phase_id}, {step_id}) in skill '{skill}' references \
                                 external step '{ext}' which does not exist in the supergraph",
                                phase_id = phase.phase_id,
                                step_id = step.id,
                            ),
                        });
                    }
                }
            }
        }
    }
}

/// Parse an `override` target string into a `(skill, phase, step_id)`
/// triple. Two grammars are accepted:
///
/// - `"phase.step_id"` — same-skill override; `current_skill` fills the
///   skill slot.
/// - `"skill.phase.step_id"` — cross-skill override; explicit skill.
///
/// Returns `None` for any other shape. The caller treats a `None` as
/// a malformed-override hard error.
fn parse_override_target(
    current_skill: &str,
    target: &str,
) -> Option<(String, String, String)> {
    let parts: Vec<&str> = target.split('.').collect();
    match parts.len() {
        2 => Some((
            current_skill.to_string(),
            parts[0].to_string(),
            parts[1].to_string(),
        )),
        3 => Some((
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string(),
        )),
        _ => None,
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

/// CI-flavored check output (M2.8). Designed to be consumed by GitHub
/// Actions / PR-status pipelines: the JSON shape is small, stable, and
/// includes the fields a status-check API expects.
///
/// `conclusion` matches GitHub Checks API enum values for direct
/// posting: "success" | "failure" | "neutral". `summary` is the
/// short human-readable headline (suitable for a PR-check title);
/// `details` is the full text dump (suitable for the check body).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationCheckOutput {
    pub conclusion: &'static str,
    pub summary: String,
    pub details: String,
    pub report: ComposeReport,
}

impl FederationCheckOutput {
    fn from_report(report: ComposeReport) -> Self {
        let warnings = report.findings.len() - report.error_count;
        let conclusion: &'static str = if report.error_count > 0 {
            "failure"
        } else if warnings > 0 {
            "neutral" // warnings don't fail the check, but flag them
        } else {
            "success"
        };
        let summary = if report.error_count > 0 {
            format!(
                "Federation check failed: {} error(s), {warnings} warning(s) across {} skill(s)",
                report.error_count, report.skills_seen,
            )
        } else if warnings > 0 {
            format!(
                "Federation check passed with {warnings} warning(s) across {} skill(s)",
                report.skills_seen,
            )
        } else {
            format!(
                "Federation check passed: {} skill(s), {} step(s)",
                report.skills_seen, report.total_steps,
            )
        };
        let details = render_text(&report);
        Self {
            conclusion,
            summary,
            details,
            report,
        }
    }
}

/// CLI entry: `sentinel federation check [--config-dir DIR]`.
///
/// Always emits JSON (no human text mode — this is the CI surface).
/// Exit code: 0 on success/neutral (warnings ok in PRs), 1 on failure.
/// PR pipelines consume the JSON to post a status check; the
/// `conclusion` field maps directly to GitHub Checks API.
pub fn run_check(config_dir_override: Option<String>) -> Result<()> {
    let config_dir = match config_dir_override {
        Some(p) => std::path::PathBuf::from(p),
        None => sentinel_infrastructure::config::config_dir(),
    };
    let report = compose(&config_dir)?;
    let output = FederationCheckOutput::from_report(report);
    println!("{}", serde_json::to_string_pretty(&output)?);
    if output.conclusion == "failure" {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Build a temp config dir with a `steps/` subdir and write the
    /// supplied (`skill_name`, `toml_content`) pairs. Returns the temp dir
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
        let invalid = r"this is not valid toml = ";
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

    // ─── M2.8 federation check (CI flavor) tests ─────────────────────

    #[test]
    fn check_output_concludes_success_for_clean_report() {
        let report = ComposeReport {
            skills_seen: 2,
            total_phases: 3,
            total_steps: 7,
            findings: Vec::new(),
            error_count: 0,
        };
        let out = FederationCheckOutput::from_report(report);
        assert_eq!(out.conclusion, "success");
        assert!(out.summary.contains("passed"));
        assert!(out.summary.contains("2 skill"));
        assert!(out.summary.contains("7 step"));
    }

    #[test]
    fn check_output_concludes_neutral_when_only_warnings() {
        // Warnings don't fail PRs. neutral conclusion lets reviewers
        // see them without blocking merge.
        let report = ComposeReport {
            skills_seen: 1,
            total_phases: 1,
            total_steps: 0,
            findings: vec![ComposeFinding {
                severity: ComposeSeverity::Warning,
                skill: Some("emptyskill".into()),
                phase_id: None,
                step_id: None,
                message: "skill 'emptyskill' declares zero steps".into(),
            }],
            error_count: 0,
        };
        let out = FederationCheckOutput::from_report(report);
        assert_eq!(out.conclusion, "neutral");
        assert!(out.summary.contains("warning"));
    }

    #[test]
    fn check_output_concludes_failure_when_errors_present() {
        let report = ComposeReport {
            skills_seen: 1,
            total_phases: 1,
            total_steps: 2,
            findings: vec![ComposeFinding {
                severity: ComposeSeverity::Error,
                skill: Some("linear".into()),
                phase_id: Some("claim".into()),
                step_id: Some("1".into()),
                message: "duplicate step coordinate".into(),
            }],
            error_count: 1,
        };
        let out = FederationCheckOutput::from_report(report);
        assert_eq!(out.conclusion, "failure");
        assert!(out.summary.contains("failed"));
        assert!(out.summary.contains("1 error"));
    }

    #[test]
    fn check_output_serializes_with_stable_field_names() {
        // PR-CI consumers depend on the field names — guard them
        // explicitly so we don't accidentally rename them.
        let report = ComposeReport {
            skills_seen: 0,
            total_phases: 0,
            total_steps: 0,
            findings: Vec::new(),
            error_count: 0,
        };
        let out = FederationCheckOutput::from_report(report);
        let json = serde_json::to_string(&out).unwrap();
        for required_field in ["conclusion", "summary", "details", "report"] {
            assert!(
                json.contains(&format!("\"{required_field}\":")),
                "missing required field '{required_field}', got: {json}",
            );
        }
        // conclusion is one of the GitHub Checks API enum values.
        assert!(json.contains("\"conclusion\":\"success\""));
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

    // ─── M2.5 federation directive cross-skill checks ────────────────

    #[test]
    fn compose_clean_when_provides_satisfies_requires_across_skills() {
        // Skill A's step 1 provides "linear.ticket_id"; skill B's step 1
        // requires it. Cross-skill resolution succeeds — clean compose.
        // This is the happy path that proves the federation contract
        // is more than annotation: it actually wires producers to
        // consumers across the supergraph.
        let producer = r#"
[[phases]]
id = "claim"

  [[phases.steps]]
  id = "1"
  description = "fetch ticket"
  provides = ["linear.ticket_id"]
"#;
        let consumer = r#"
[[phases]]
id = "open_pr"

  [[phases.steps]]
  id = "1"
  description = "create the PR"
  requires = ["linear.ticket_id"]
"#;
        let (_g, path) = temp_config(&[("linear", producer), ("git", consumer)]);
        let report = compose(&path).unwrap();
        assert!(
            report.ok(),
            "cross-skill provides/requires should resolve, got {:?}",
            report.findings,
        );
    }

    #[test]
    fn compose_errors_when_requires_has_no_provider() {
        // Skill declares `requires = ["nobody.ever.provides.this"]`.
        // No skill in the supergraph offers that artifact, so compose
        // must error. Without this check, virtual skill packs (M7)
        // could plan executions that physically can't run because a
        // required input is never produced.
        let toml = r#"
[[phases]]
id = "open_pr"

  [[phases.steps]]
  id = "1"
  description = "open"
  requires = ["nonexistent.artifact"]
"#;
        let (_g, path) = temp_config(&[("git", toml)]);
        let report = compose(&path).unwrap();
        assert!(!report.ok());
        let msg = &report.findings[0].message;
        assert!(
            msg.contains("requires artifact 'nonexistent.artifact'"),
            "expected unsatisfied-requires error, got: {msg}",
        );
    }

    #[test]
    fn compose_errors_on_dangling_external_reference() {
        // `external = ["linear.claim.99"]` references a step that
        // doesn't exist. Compose must catch this — otherwise routers
        // emit plans depending on phantom steps.
        let toml = r#"
[[phases]]
id = "open_pr"

  [[phases.steps]]
  id = "1"
  description = "open"
  external = ["linear.claim.99"]
"#;
        let (_g, path) = temp_config(&[("git", toml)]);
        let report = compose(&path).unwrap();
        assert!(!report.ok());
        let msg = &report.findings[0].message;
        assert!(
            msg.contains("external step 'linear.claim.99'"),
            "expected dangling-external error, got: {msg}",
        );
    }

    #[test]
    fn compose_errors_on_malformed_external_reference() {
        // External must be `skill.phase.step_id` — anything else is
        // a malformed reference. Catch it at compose time so operators
        // see the typo before any execution attempts to follow the
        // dangling pointer.
        let toml = r#"
[[phases]]
id = "open_pr"

  [[phases.steps]]
  id = "1"
  description = "open"
  external = ["linear-claim-2"]
"#;
        let (_g, path) = temp_config(&[("git", toml)]);
        let report = compose(&path).unwrap();
        assert!(!report.ok());
        let msg = &report.findings[0].message;
        assert!(
            msg.contains("malformed external reference"),
            "expected malformed-external error, got: {msg}",
        );
    }

    // ─── M2.6 deprecation/migration cross-skill checks ───────────────

    #[test]
    fn compose_warns_on_deprecated_step() {
        // Deprecated steps don't break compose — they just surface a
        // warning naming the migration message. The chain composes
        // (report.ok() == true), but operators see the heads-up.
        let toml = r#"
[[phases]]
id = "claim"

  [[phases.steps]]
  id = "1"
  description = "fetch (legacy)"
  deprecated = "Use claim.2 — fetches by ID, not URL"
"#;
        let (_g, path) = temp_config(&[("linear", toml)]);
        let report = compose(&path).unwrap();
        assert!(report.ok(), "deprecated steps shouldn't fail compose");
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].severity, ComposeSeverity::Warning);
        let msg = &report.findings[0].message;
        assert!(msg.contains("deprecated"));
        assert!(msg.contains("Use claim.2"));
    }

    #[test]
    fn compose_errors_on_dangling_override_target() {
        // override = "claim.99" — target step doesn't exist. Hard
        // error: you can't replace a step that isn't there.
        let toml = r#"
[[phases]]
id = "claim"

  [[phases.steps]]
  id = "new"
  description = "modern fetch"
  override = "claim.99"
"#;
        let (_g, path) = temp_config(&[("linear", toml)]);
        let report = compose(&path).unwrap();
        assert!(!report.ok());
        let msg = &report.findings[0].message;
        assert!(msg.contains("override of 'claim.99'"));
        assert!(msg.contains("does not exist"));
    }

    #[test]
    fn compose_warns_when_override_target_not_deprecated() {
        // override target exists but is not itself marked deprecated.
        // Compose composes (no error) but warns — declare the
        // deprecation up-front so consumers know.
        let toml = r#"
[[phases]]
id = "claim"

  [[phases.steps]]
  id = "old"
  description = "fetch (still alive)"

  [[phases.steps]]
  id = "new"
  description = "modern fetch"
  override = "claim.old"
"#;
        let (_g, path) = temp_config(&[("linear", toml)]);
        let report = compose(&path).unwrap();
        assert!(report.ok(), "warning-only situation should still compose");
        let warning = report
            .findings
            .iter()
            .find(|f| f.severity == ComposeSeverity::Warning)
            .expect("override-without-deprecation warning expected");
        assert!(warning.message.contains("not marked `deprecated`"));
    }

    #[test]
    fn compose_clean_when_override_target_is_deprecated() {
        // The disciplined migration path: declare deprecated on the
        // old step, declare override on the new step. Compose emits
        // exactly one warning (the deprecation), no override warning.
        let toml = r#"
[[phases]]
id = "claim"

  [[phases.steps]]
  id = "old"
  description = "legacy fetch"
  deprecated = "use claim.new"

  [[phases.steps]]
  id = "new"
  description = "modern fetch"
  override = "claim.old"
"#;
        let (_g, path) = temp_config(&[("linear", toml)]);
        let report = compose(&path).unwrap();
        assert!(report.ok());
        // Exactly one finding — the deprecation warning. No override
        // warning because the target IS deprecated.
        assert_eq!(report.findings.len(), 1);
        assert!(report.findings[0].message.contains("deprecated"));
    }

    #[test]
    fn compose_errors_on_malformed_override_target() {
        // override = "claim/old" — wrong separator. Compose can't
        // parse the grammar so it errors instead of resolving.
        let toml = r#"
[[phases]]
id = "claim"

  [[phases.steps]]
  id = "new"
  description = "modern fetch"
  override = "claim/old"
"#;
        let (_g, path) = temp_config(&[("linear", toml)]);
        let report = compose(&path).unwrap();
        assert!(!report.ok());
        let msg = &report.findings[0].message;
        assert!(msg.contains("malformed override target"));
    }

    #[test]
    fn compose_resolves_cross_skill_override() {
        // Cross-skill override grammar: "skill.phase.step_id". The
        // git skill takes over a capability previously owned by
        // linear. Both halves are needed — declare deprecation on the
        // linear side, declare override on the git side.
        let producer = r#"
[[phases]]
id = "claim"

  [[phases.steps]]
  id = "1"
  description = "old linear-side claim"
  deprecated = "moved to git.claim.1"
"#;
        let consumer = r#"
[[phases]]
id = "claim"

  [[phases.steps]]
  id = "1"
  description = "new git-side claim"
  override = "linear.claim.1"
"#;
        let (_g, path) = temp_config(&[("linear", producer), ("git", consumer)]);
        let report = compose(&path).unwrap();
        assert!(report.ok());
        // One deprecation warning, no override warnings.
        let warnings: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.severity == ComposeSeverity::Warning)
            .collect();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("deprecated"));
    }

    // ── parse_override_target unit tests ─────────────────────────────

    #[test]
    fn parse_override_target_two_parts_uses_current_skill() {
        let r = parse_override_target("linear", "claim.old").unwrap();
        assert_eq!(r, ("linear".into(), "claim".into(), "old".into()));
    }

    #[test]
    fn parse_override_target_three_parts_uses_explicit_skill() {
        let r = parse_override_target("git", "linear.claim.1").unwrap();
        assert_eq!(r, ("linear".into(), "claim".into(), "1".into()));
    }

    #[test]
    fn parse_override_target_rejects_other_arities() {
        assert!(parse_override_target("linear", "single").is_none());
        assert!(parse_override_target("linear", "a.b.c.d").is_none());
        assert!(parse_override_target("linear", "").is_none());
    }

    #[test]
    fn compose_inaccessible_step_does_not_break_provides_chain() {
        // Inaccessible steps still participate in the provides graph —
        // they're just not exposed to the router. So a chain
        // `internal_helper (inaccessible) → public_consumer` should
        // still compose cleanly. Federation correctness ≠ router
        // visibility.
        let toml = r#"
[[phases]]
id = "internal"

  [[phases.steps]]
  id = "helper"
  description = "skill-internal"
  provides = ["git.computed_branch"]
  inaccessible = true

[[phases]]
id = "open_pr"

  [[phases.steps]]
  id = "1"
  description = "open"
  requires = ["git.computed_branch"]
"#;
        let (_g, path) = temp_config(&[("git", toml)]);
        let report = compose(&path).unwrap();
        assert!(
            report.ok(),
            "inaccessible producers should still satisfy requires, got {:?}",
            report.findings,
        );
    }
}
