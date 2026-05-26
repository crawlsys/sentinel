//! A12 Phase 2 — Filesystem-backed eval corpus loader.
//!
//! Reads `EvalCase` JSON files from
//! `~/.claude/sentinel/eval/ba-corpus/cases/{case_id}.json` per spec
//! §3.5. The future BA-Eval curation tooling writes these case
//! files; sentinel's `sentinel eval` CLI + Phase 3 benchmark runner
//! read them.
//!
//! ## Private test split
//!
//! Per spec §3.4 the corpus has a private test split that agents
//! must never see during prompt iteration. The split lives in a
//! sibling directory `private-test-split/{case_id}.json`. This
//! adapter EXPOSES both directories — operator tooling
//! (`sentinel eval list`) shows all cases for inventory purposes,
//! but the `is_private_test` flag on each `EvalCase` is the
//! source-of-truth marker the future runner uses to gate exposure.
//! The directory layout is operational; the per-case flag is the
//! security contract.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use sentinel_domain::eval::{EvalCase, EvalCaseId};

/// Filesystem-backed eval corpus reader.
///
/// Read-only by design — corpus curation lives outside sentinel.
/// Lazy-load: each `load_case` call reads the file fresh; mtime
/// caching deferred until profiling shows it matters.
pub struct FilesystemEvalCorpus {
    base_dir: PathBuf,
}

impl std::fmt::Debug for FilesystemEvalCorpus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemEvalCorpus")
            .field("base_dir", &self.base_dir)
            .finish()
    }
}

impl FilesystemEvalCorpus {
    /// Construct pointed at a specific directory. Used by tests to
    /// scope to a tempdir.
    #[must_use]
    pub const fn at_dir(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Construct with the default path
    /// (`~/.claude/sentinel/eval/ba-corpus/`).
    pub fn with_default_path() -> Result<Self> {
        let home =
            dirs::home_dir().context("home directory not resolvable from environment")?;
        let base_dir = home
            .join(".claude")
            .join("sentinel")
            .join("eval")
            .join("ba-corpus");
        Ok(Self::at_dir(base_dir))
    }

    /// Read-only access to the base directory.
    #[must_use]
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Public test split — agents may see these during prompt iteration.
    #[must_use]
    pub fn public_cases_dir(&self) -> PathBuf {
        self.base_dir.join("cases")
    }

    /// Private test split per spec §3.4 — held back from prompt
    /// iteration; only the benchmark runner accesses these on
    /// honest-measurement runs. The directory layout exists for
    /// operator tooling; the per-case `is_private_test` flag is the
    /// security contract.
    #[must_use]
    pub fn private_cases_dir(&self) -> PathBuf {
        self.base_dir.join("private-test-split")
    }

    /// Path for a single case JSON file in `cases/`. Used by tests
    /// + by curation tooling that wants to know where to write.
    #[must_use]
    pub fn case_path(&self, case_id: &EvalCaseId) -> PathBuf {
        self.public_cases_dir().join(format!("{case_id}.json"))
    }

    /// Path for a single case JSON file in `private-test-split/`.
    #[must_use]
    pub fn private_case_path(&self, case_id: &EvalCaseId) -> PathBuf {
        self.private_cases_dir().join(format!("{case_id}.json"))
    }

    /// List every case in BOTH the public + private splits. Returns
    /// each `case_id` alongside whether it lives in the private
    /// directory. Returns an empty Vec when neither dir exists
    /// (fresh install). Errors only on un-readable directory
    /// metadata (permissions / filesystem failures).
    pub fn list_case_ids(&self) -> Result<Vec<CorpusEntry>> {
        let mut out = Vec::new();
        for (dir, is_private) in [
            (self.public_cases_dir(), false),
            (self.private_cases_dir(), true),
        ] {
            let entries = match std::fs::read_dir(&dir) {
                Ok(it) => it,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("failed to read corpus dir {}", dir.display())
                    });
                }
            };
            for entry in entries {
                let entry = entry.with_context(|| {
                    format!("failed to iterate corpus dir {}", dir.display())
                })?;
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                // Match `*.json` files; everything else (README,
                // editor-temp, etc.) is silently skipped.
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(case_id) = EvalCaseId::new(stem) else {
                    tracing::warn!(path = ?path, "skipping unparseable case_id");
                    continue;
                };
                out.push(CorpusEntry {
                    case_id,
                    is_private_test: is_private,
                });
            }
        }
        // Stable ordering so `sentinel eval list` output is
        // deterministic (helps with diff-based testing).
        out.sort_by(|a, b| {
            a.is_private_test
                .cmp(&b.is_private_test)
                .then_with(|| a.case_id.as_str().cmp(b.case_id.as_str()))
        });
        Ok(out)
    }

    /// Load a single case by id. Tries the public dir first; if the
    /// file isn't there, tries `private-test-split/`. **Caller
    /// responsibility** per spec §3.4: callers exposing case content
    /// to agents during prompt iteration MUST filter on
    /// `EvalCase::provenance.is_private_test == false`. This loader
    /// returns both layouts because operator inventory tooling has
    /// a legitimate read need; the security boundary is at the
    /// consumer.
    pub fn load_case(&self, case_id: &EvalCaseId) -> Result<EvalCase> {
        let candidates = [self.case_path(case_id), self.private_case_path(case_id)];
        let mut last_io_err: Option<std::io::Error> = None;
        for path in &candidates {
            match std::fs::read_to_string(path) {
                Ok(content) => {
                    return serde_json::from_str::<EvalCase>(&content).with_context(|| {
                        format!("failed to parse EvalCase at {}", path.display())
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    last_io_err = Some(err);
                }
            }
        }
        last_io_err.map_or_else(
            || Err(anyhow::anyhow!("no case file found for {case_id}")),
            |err| Err(err).context(format!("failed to read case {case_id}")),
        )
    }
}

/// One entry returned by [`FilesystemEvalCorpus::list_case_ids`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusEntry {
    pub case_id: EvalCaseId,
    /// `true` when the case lives in `private-test-split/`. Operator
    /// tooling renders this so the human sees which cases are
    /// held-back from prompt iteration.
    pub is_private_test: bool,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::eval::{
        CaseProvenance, GoldArtifact, ScoringRubric, SourceCorpus,
    };
    use std::fs;
    use tempfile::TempDir;

    fn fixture_case(id: &str, is_private: bool) -> EvalCase {
        EvalCase {
            case_id: EvalCaseId::new(id).unwrap(),
            stakeholder_brief: "Test brief".into(),
            source_corpus: SourceCorpus::Public {
                url: "https://example.com".into(),
                license: "CC-BY-4.0".into(),
            },
            gold_artifact: Some(GoldArtifact {
                text: "gold output".into(),
                author: "test".into(),
                content_hash: None,
            }),
            gold_outcomes: None,
            scoring_rubric: ScoringRubric::ba_default(),
            provenance: CaseProvenance {
                contributor: "test".into(),
                license: "CC-BY-4.0".into(),
                is_private_test: is_private,
            },
        }
    }

    fn write_case(corpus: &FilesystemEvalCorpus, case: &EvalCase, is_private: bool) {
        let path = if is_private {
            corpus.private_case_path(&case.case_id)
        } else {
            corpus.case_path(&case.case_id)
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, serde_json::to_string(case).unwrap()).unwrap();
    }

    fn corpus(dir: &TempDir) -> FilesystemEvalCorpus {
        FilesystemEvalCorpus::at_dir(dir.path().to_path_buf())
    }

    // ---- Empty corpus ----

    #[test]
    fn empty_directory_lists_no_cases() {
        let dir = TempDir::new().unwrap();
        let c = corpus(&dir);
        let entries = c.list_case_ids().unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn missing_directory_returns_empty_not_error() {
        // Point at a nonexistent dir — list_case_ids handles
        // gracefully (fresh-install case).
        let dir = TempDir::new().unwrap();
        let c = FilesystemEvalCorpus::at_dir(dir.path().join("does-not-exist"));
        let entries = c.list_case_ids().unwrap();
        assert!(entries.is_empty());
    }

    // ---- Public split ----

    #[test]
    fn lists_public_cases() {
        let dir = TempDir::new().unwrap();
        let c = corpus(&dir);
        write_case(&c, &fixture_case("case-A", false), false);
        write_case(&c, &fixture_case("case-B", false), false);
        let entries = c.list_case_ids().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| !e.is_private_test));
    }

    #[test]
    fn load_case_reads_public_split() {
        let dir = TempDir::new().unwrap();
        let c = corpus(&dir);
        write_case(&c, &fixture_case("case-A", false), false);
        let case = c.load_case(&EvalCaseId::new("case-A").unwrap()).unwrap();
        assert_eq!(case.case_id.as_str(), "case-A");
        assert!(!case.provenance.is_private_test);
    }

    // ---- Private split ----

    #[test]
    fn lists_private_cases_with_flag() {
        let dir = TempDir::new().unwrap();
        let c = corpus(&dir);
        write_case(&c, &fixture_case("case-X", true), true);
        let entries = c.list_case_ids().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_private_test);
        assert_eq!(entries[0].case_id.as_str(), "case-X");
    }

    #[test]
    fn load_case_reads_private_split_when_public_missing() {
        let dir = TempDir::new().unwrap();
        let c = corpus(&dir);
        write_case(&c, &fixture_case("case-X", true), true);
        let case = c.load_case(&EvalCaseId::new("case-X").unwrap()).unwrap();
        assert_eq!(case.case_id.as_str(), "case-X");
        assert!(case.provenance.is_private_test);
    }

    // ---- Mixed split ordering ----

    #[test]
    fn list_returns_public_before_private() {
        let dir = TempDir::new().unwrap();
        let c = corpus(&dir);
        write_case(&c, &fixture_case("case-B-public", false), false);
        write_case(&c, &fixture_case("case-A-private", true), true);
        let entries = c.list_case_ids().unwrap();
        assert_eq!(entries.len(), 2);
        // Public first (is_private_test = false sorts before true)
        assert!(!entries[0].is_private_test);
        assert!(entries[1].is_private_test);
    }

    #[test]
    fn list_is_deterministic_alphabetical_within_each_split() {
        let dir = TempDir::new().unwrap();
        let c = corpus(&dir);
        write_case(&c, &fixture_case("case-Z", false), false);
        write_case(&c, &fixture_case("case-A", false), false);
        write_case(&c, &fixture_case("case-M", false), false);
        let entries = c.list_case_ids().unwrap();
        let ids: Vec<&str> = entries.iter().map(|e| e.case_id.as_str()).collect();
        assert_eq!(ids, vec!["case-A", "case-M", "case-Z"]);
    }

    // ---- File filtering ----

    #[test]
    fn skips_non_json_files() {
        let dir = TempDir::new().unwrap();
        let c = corpus(&dir);
        write_case(&c, &fixture_case("case-A", false), false);
        // Drop a README + an editor-temp file in cases/
        fs::write(c.public_cases_dir().join("README.md"), "docs").unwrap();
        fs::write(c.public_cases_dir().join(".case-tmp.swp"), "junk").unwrap();
        let entries = c.list_case_ids().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].case_id.as_str(), "case-A");
    }

    #[test]
    fn skips_files_with_unparseable_case_id() {
        let dir = TempDir::new().unwrap();
        let c = corpus(&dir);
        write_case(&c, &fixture_case("case-A", false), false);
        // Empty filename (after stem strip) shouldn't happen on
        // POSIX, but cover the EvalCaseId rejection path: a file
        // named `.json` has empty stem.
        fs::write(c.public_cases_dir().join(".json"), "{}").unwrap();
        let entries = c.list_case_ids().unwrap();
        // Only the well-named case shows up.
        assert_eq!(entries.len(), 1);
    }

    // ---- Malformed JSON ----

    #[test]
    fn load_case_propagates_parse_error() {
        let dir = TempDir::new().unwrap();
        let c = corpus(&dir);
        fs::create_dir_all(c.public_cases_dir()).unwrap();
        fs::write(c.case_path(&EvalCaseId::new("bad").unwrap()), "not json").unwrap();
        let err = c
            .load_case(&EvalCaseId::new("bad").unwrap())
            .unwrap_err();
        assert!(format!("{err:#}").contains("parse"));
    }

    #[test]
    fn load_case_missing_errors_with_id() {
        let dir = TempDir::new().unwrap();
        let c = corpus(&dir);
        let err = c
            .load_case(&EvalCaseId::new("ghost").unwrap())
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ghost"));
    }

    // ---- Path semantics ----

    #[test]
    fn with_default_path_resolves_under_claude_dir() {
        let c = FilesystemEvalCorpus::with_default_path().unwrap();
        let p = c.base_dir().display().to_string();
        assert!(
            p.contains(".claude")
                && p.contains("sentinel")
                && p.contains("eval")
                && p.ends_with("ba-corpus"),
            "default path: {p}"
        );
    }

    #[test]
    fn public_and_private_dirs_are_siblings_under_base() {
        let dir = TempDir::new().unwrap();
        let c = corpus(&dir);
        let public = c.public_cases_dir();
        let private = c.private_cases_dir();
        assert_eq!(public.parent(), Some(dir.path()));
        assert_eq!(private.parent(), Some(dir.path()));
        assert_ne!(public, private);
    }

    // ---- Send + Sync ----

    #[test]
    fn corpus_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FilesystemEvalCorpus>();
    }
}
