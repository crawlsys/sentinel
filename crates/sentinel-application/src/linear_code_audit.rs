//! Linear "done-but-no-code" false-done audit.
//!
//! Cross-checks every Completed ticket in the Linear cache against a
//! precomputed map of code evidence. A ticket marked Completed with no
//! commits and no touched files is a `done-no-evidence` flag — the classic
//! false-done where the ticket moved to Done but nothing shipped (or the
//! work landed under a different ticket ref and the link was never made).
//!
//! ## Inputs
//!
//! 1. **Linear cache** at `~/.claude/sentinel/linear-assigned.json` — the
//!    same permissive `{ "issues": [...] }`-or-bare-array file the rest of
//!    the suite reads. Only the `identifier` and `state` are used here.
//! 2. **Code-evidence map** at `~/.claude/sentinel/ticket-code-evidence.json`
//!    — a precomputed ticket → evidence map (git-grep across repos is the
//!    caller's / cron's job, not this pure-application crate's):
//!    ```json
//!    {"FPCRM-520":{"commits":3,"files":["x.tsx","y.ts"]},
//!     "FPCRM-521":{"commits":0,"files":[]}}
//!    ```
//!    A ticket is "evidenced" when it has an entry with `commits > 0` OR a
//!    non-empty `files` array. No entry at all counts as no evidence.
//!
//! Non-Completed tickets are ignored entirely — this audit is only about
//! the integrity of the *Done* column.
//!
//! Output is written to `~/.claude/sentinel/metrics/linear-code-audit.json`
//! (summary) and `…-code-audit.jsonl` (one row per flagged ticket),
//! idempotently.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

/// State `type` values (lowercased) that count as "done".
const DONE_TYPES: &[&str] = &["completed"];

/// Code evidence for one ticket, parsed out of the evidence map.
#[derive(Debug, Clone, Default)]
struct Evidence {
    commits: u64,
    files: usize,
}

impl Evidence {
    /// Real evidence = at least one commit OR at least one touched file.
    fn is_present(&self) -> bool {
        self.commits > 0 || self.files > 0
    }
}

/// A completed ticket lacking code evidence, written as a JSONL row.
#[derive(Debug, Clone, Serialize)]
pub struct CodeFlag {
    pub identifier: String,
    pub state: String,
    /// Always `done-no-evidence` today; reserved for future evidence
    /// categories (e.g. `evidence-other-ticket`).
    pub category: String,
    pub commits: u64,
    pub files: usize,
    pub detail: String,
}

/// The full code-audit summary written to `linear-code-audit.json`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CodeAuditSummary {
    /// Completed tickets considered (the audit's denominator).
    pub completed_total: usize,
    /// Completed tickets with real code evidence.
    pub with_evidence: usize,
    /// Completed tickets with NO code evidence (the flagged set).
    pub without_evidence: usize,
    /// Per-ticket flag rows (the `without_evidence` set).
    pub flags: Vec<CodeFlag>,
}

/// Run the false-done audit over `linear_cache` + `evidence_map`, write
/// `output_summary` (JSON) and its `.jsonl` sibling (flag rows), return the
/// summary.
pub fn scan_code_audit(
    linear_cache: &Path,
    evidence_map: &Path,
    output_summary: &Path,
) -> Result<CodeAuditSummary> {
    let issues = load_completed(linear_cache)
        .with_context(|| format!("load linear cache {}", linear_cache.display()))?;
    let evidence = load_evidence(evidence_map)
        .with_context(|| format!("load evidence map {}", evidence_map.display()))?;

    let mut summary = CodeAuditSummary::default();
    for (identifier, state_name) in issues {
        summary.completed_total += 1;
        let ev = evidence
            .get(&identifier.to_uppercase())
            .cloned()
            .unwrap_or_default();
        if ev.is_present() {
            summary.with_evidence += 1;
        } else {
            summary.without_evidence += 1;
            summary.flags.push(CodeFlag {
                identifier,
                state: state_name,
                category: "done-no-evidence".into(),
                commits: ev.commits,
                files: ev.files,
                detail: "marked Completed but no commits or touched files found — \
                         possible false-done (or code shipped under another ticket ref)"
                    .into(),
            });
        }
    }

    write_outputs(&summary, output_summary)?;
    Ok(summary)
}

/// Read the Linear cache and return `(identifier, state_name)` for every
/// Completed ticket. Missing cache = empty list.
fn load_completed(path: &Path) -> Result<Vec<(String, String)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse JSON {}", path.display()))?;
    let arr: &[serde_json::Value] = if let Some(a) = value.as_array() {
        a
    } else if let Some(a) = value.get("issues").and_then(serde_json::Value::as_array) {
        a
    } else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for v in arr {
        let Some(identifier) = v.get("identifier").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let (state_name, state_type) = v
            .get("state")
            .map(|s| {
                (
                    s.get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    s.get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_lowercase(),
                )
            })
            .unwrap_or_default();
        if DONE_TYPES.contains(&state_type.as_str()) {
            out.push((identifier.to_string(), state_name));
        }
    }
    Ok(out)
}

/// Parse the ticket → evidence map (keyed upper-case for case-insensitive
/// lookup). Missing file = empty map.
fn load_evidence(path: &Path) -> Result<HashMap<String, Evidence>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse JSON {}", path.display()))?;
    let Some(obj) = value.as_object() else {
        return Ok(HashMap::new());
    };

    let mut out = HashMap::with_capacity(obj.len());
    for (k, v) in obj {
        let commits = v
            .get("commits")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let files = v
            .get("files")
            .and_then(serde_json::Value::as_array)
            .map_or(0, std::vec::Vec::len);
        out.insert(k.to_uppercase(), Evidence { commits, files });
    }
    Ok(out)
}

fn write_outputs(summary: &CodeAuditSummary, output_summary: &Path) -> Result<()> {
    if let Some(parent) = output_summary.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create metrics dir {}", parent.display()))?;
    }
    let jsonl = output_summary.with_extension("jsonl");
    let mut f = File::create(&jsonl).with_context(|| format!("create {}", jsonl.display()))?;
    for flag in &summary.flags {
        f.write_all(serde_json::to_string(flag)?.as_bytes())?;
        f.write_all(b"\n")?;
    }
    fs::write(output_summary, serde_json::to_string_pretty(summary)?)
        .with_context(|| format!("write {}", output_summary.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(json: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn completed_with_evidence_passes() {
        let cache = tmp(r#"{"issues":[
                {"identifier":"FPCRM-520","state":{"name":"Completed","type":"completed"}}
            ]}"#);
        let ev = tmp(r#"{"FPCRM-520":{"commits":3,"files":["x.tsx"]}}"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_code_audit(cache.path(), ev.path(), out.path()).unwrap();
        assert_eq!(s.completed_total, 1);
        assert_eq!(s.with_evidence, 1);
        assert_eq!(s.without_evidence, 0);
        assert!(s.flags.is_empty());
    }

    #[test]
    fn completed_without_evidence_is_flagged() {
        let cache = tmp(r#"{"issues":[
                {"identifier":"FPCRM-521","state":{"name":"Completed","type":"completed"}}
            ]}"#);
        // No entry at all for FPCRM-521.
        let ev = tmp(r#"{"FPCRM-999":{"commits":1,"files":["a.ts"]}}"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_code_audit(cache.path(), ev.path(), out.path()).unwrap();
        assert_eq!(s.without_evidence, 1);
        assert_eq!(s.flags.len(), 1);
        assert_eq!(s.flags[0].identifier, "FPCRM-521");
        assert_eq!(s.flags[0].category, "done-no-evidence");
    }

    #[test]
    fn zero_commits_and_zero_files_is_flagged() {
        // An entry that exists but is empty counts as no evidence.
        let cache =
            tmp(r#"[{"identifier":"FPCRM-522","state":{"name":"Completed","type":"completed"}}]"#);
        let ev = tmp(r#"{"FPCRM-522":{"commits":0,"files":[]}}"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_code_audit(cache.path(), ev.path(), out.path()).unwrap();
        assert_eq!(s.without_evidence, 1);
        assert_eq!(s.flags[0].commits, 0);
        assert_eq!(s.flags[0].files, 0);
    }

    #[test]
    fn non_completed_tickets_are_ignored() {
        let cache = tmp(r#"{"issues":[
                {"identifier":"FPCRM-600","state":{"name":"In Progress","type":"started"}},
                {"identifier":"FPCRM-601","state":{"name":"Backlog","type":"backlog"}},
                {"identifier":"FPCRM-602","state":{"name":"Completed","type":"completed"}}
            ]}"#);
        let ev = tmp(r#"{"FPCRM-602":{"commits":2,"files":["z.ts"]}}"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_code_audit(cache.path(), ev.path(), out.path()).unwrap();
        // Only the one Completed ticket is in the denominator.
        assert_eq!(s.completed_total, 1);
        assert_eq!(s.with_evidence, 1);
        assert_eq!(s.without_evidence, 0);
    }

    #[test]
    fn files_only_counts_as_evidence() {
        // commits=0 but a touched file present → evidenced.
        let cache =
            tmp(r#"[{"identifier":"FPCRM-523","state":{"name":"Completed","type":"completed"}}]"#);
        let ev = tmp(r#"{"FPCRM-523":{"commits":0,"files":["only.tsx"]}}"#);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_code_audit(cache.path(), ev.path(), out.path()).unwrap();
        assert_eq!(s.with_evidence, 1);
        assert_eq!(s.without_evidence, 0);
    }

    #[test]
    fn missing_inputs_are_empty_not_error() {
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_code_audit(
            Path::new("/nonexistent/cache.json"),
            Path::new("/nonexistent/evidence.json"),
            out.path(),
        )
        .unwrap();
        assert_eq!(s.completed_total, 0);
        assert_eq!(s.without_evidence, 0);
    }
}
