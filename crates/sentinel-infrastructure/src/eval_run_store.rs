//! A12 Phase 3d — Filesystem-backed `EvalRunResult` store.
//!
//! Persists complete benchmark runs as one JSON file per run at
//! `~/.claude/sentinel/eval/ba-corpus/runs/{run_id}.json` per spec
//! §3.5. Runs are write-once-per-attempt; `save` overwrites if the
//! same `run_id` reappears (typical case: retry after a transient
//! backend failure — the operator re-runs the same id explicitly).
//!
//! ## Why one JSON file per run, not JSONL?
//!
//! Each run carries a complete `EvalRunResult` (every case's
//! candidate output, score, timing, and error context). Appending
//! a partial run record line-by-line would create a window where
//! readers could see a half-written run — JSON-per-file is
//! atomically consistent under the standard temp+rename pattern.
//! The save path writes to `{run_id}.json.tmp` and renames into
//! place so `load` either reads the prior version or the new one,
//! never a torn write.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use sentinel_domain::eval::{EvalRunId, EvalRunResult};
use sentinel_domain::ports::{EvalRunStoreError, EvalRunStorePort};

/// Filesystem-backed `EvalRunStorePort` implementation.
///
/// Read/write by design — the benchmark runner persists runs here
/// after producing them, the CLI (`sentinel eval show`) reads them
/// back.
pub struct FilesystemEvalRunStore {
    base_dir: PathBuf,
}

impl std::fmt::Debug for FilesystemEvalRunStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemEvalRunStore")
            .field("base_dir", &self.base_dir)
            .finish()
    }
}

impl FilesystemEvalRunStore {
    /// Construct pointed at a specific run directory. Used by tests
    /// to scope to a tempdir. The directory does NOT need to exist
    /// — `save` creates it lazily.
    #[must_use]
    pub const fn at_dir(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Construct with the default path
    /// (`~/.claude/sentinel/eval/ba-corpus/runs/`).
    pub fn with_default_path() -> Result<Self> {
        let base_dir = crate::paths::sentinel_root()
            .join("eval")
            .join("ba-corpus")
            .join("runs");
        Ok(Self::at_dir(base_dir))
    }

    /// Read-only access to the base directory.
    #[must_use]
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    fn run_path(&self, run_id: &EvalRunId) -> PathBuf {
        self.base_dir.join(format!("{}.json", run_id.as_str()))
    }
}

impl EvalRunStorePort for FilesystemEvalRunStore {
    fn save(&self, run: &EvalRunResult) -> Result<(), EvalRunStoreError> {
        if !self.base_dir.exists() {
            fs::create_dir_all(&self.base_dir).map_err(|e| {
                EvalRunStoreError::StoreUnavailable(format!(
                    "create run dir {}: {e}",
                    self.base_dir.display()
                ))
            })?;
        }
        let final_path = self.run_path(&run.run_id);
        let tmp_path = final_path.with_extension("json.tmp");

        let json = serde_json::to_string_pretty(run).map_err(|e| {
            EvalRunStoreError::Malformed(format!("serialize run {}: {e}", run.run_id.as_str()))
        })?;

        fs::write(&tmp_path, json).map_err(|e| {
            EvalRunStoreError::StoreUnavailable(format!("write {}: {e}", tmp_path.display()))
        })?;

        fs::rename(&tmp_path, &final_path).map_err(|e| {
            EvalRunStoreError::StoreUnavailable(format!(
                "rename {} -> {}: {e}",
                tmp_path.display(),
                final_path.display(),
            ))
        })?;
        Ok(())
    }

    fn load(&self, run_id: &EvalRunId) -> Result<Option<EvalRunResult>, EvalRunStoreError> {
        let path = self.run_path(run_id);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(EvalRunStoreError::StoreUnavailable(format!(
                    "read {}: {e}",
                    path.display()
                )));
            }
        };
        let run: EvalRunResult = serde_json::from_slice(&bytes)
            .map_err(|e| EvalRunStoreError::Malformed(format!("parse {}: {e}", path.display())))?;
        Ok(Some(run))
    }

    fn list_run_ids(&self) -> Result<Vec<EvalRunId>, EvalRunStoreError> {
        if !self.base_dir.exists() {
            return Ok(Vec::new());
        }
        let entries = fs::read_dir(&self.base_dir).map_err(|e| {
            EvalRunStoreError::StoreUnavailable(format!(
                "read dir {}: {e}",
                self.base_dir.display()
            ))
        })?;
        let mut out: Vec<EvalRunId> = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            // Skip directories, dotfiles, and `.tmp` partial writes.
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            // We only enumerate `.json` files (skip stray files of any other
            // extension including `.tmp` partial writes — case-insensitive
            // because the filesystem may normalize case on macOS).
            let ext_is_json = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("json"));
            if !ext_is_json {
                continue;
            }
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            // EvalRunId::new validates the id shape; silently skip files whose
            // name doesn't parse so a stray legacy file doesn't poison the
            // whole listing.
            if let Ok(id) = EvalRunId::new(stem) {
                out.push(id);
            }
        }
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use sentinel_domain::eval::{
        EvalAxis, EvalAxisScore, EvalCaseId, EvalCaseResult, EvalRunId, EvalRunResult, EvalScore,
        ScoringRubric,
    };
    use tempfile::TempDir;

    fn make_run(run_id: &str) -> EvalRunResult {
        let rubric = ScoringRubric::ba_default();
        let axis_scores = vec![
            EvalAxisScore::new(EvalAxis::CitationDensityAccuracy, 0.8, 1.0),
            EvalAxisScore::new(EvalAxis::RequirementsCoverage, 0.7, 1.0),
            EvalAxisScore::new(EvalAxis::AlternativesSeriousness, 0.6, 1.0),
            EvalAxisScore::new(EvalAxis::TonalCalibration, 0.85, 1.0),
            EvalAxisScore::new(EvalAxis::OutcomeRealism, 0.5, 2.0),
            EvalAxisScore::new(EvalAxis::StakeholderFit, 0.9, 1.0),
        ];
        let case_id = EvalCaseId::new("c1").unwrap();
        let run = EvalRunId::new(run_id).unwrap();
        let score = EvalScore::new(case_id.clone(), run.clone(), axis_scores, &rubric);
        EvalRunResult {
            run_id: run.clone(),
            started_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            completed_at: Utc.timestamp_opt(1_700_001_000, 0).unwrap(),
            case_results: vec![EvalCaseResult {
                case_id,
                run_id: run,
                candidate_output: "candidate text".to_string(),
                score: Some(score),
                timing_ms: 1234,
                completed_at: Utc.timestamp_opt(1_700_000_500, 0).unwrap(),
                error: None,
            }],
        }
    }

    #[test]
    fn save_creates_runs_directory_if_missing() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("runs");
        assert!(!base.exists());
        let store = FilesystemEvalRunStore::at_dir(base.clone());
        store.save(&make_run("r1")).unwrap();
        assert!(base.exists());
        assert!(base.join("r1.json").exists());
    }

    #[test]
    fn save_then_load_roundtrips_full_run() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemEvalRunStore::at_dir(dir.path().to_path_buf());
        let original = make_run("r1");
        store.save(&original).unwrap();
        let loaded = store.load(&original.run_id).unwrap().unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn load_returns_ok_none_when_run_missing() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemEvalRunStore::at_dir(dir.path().to_path_buf());
        let ghost = EvalRunId::new("nonexistent").unwrap();
        let result = store.load(&ghost).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_returns_malformed_when_file_corrupt() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemEvalRunStore::at_dir(dir.path().to_path_buf());
        let run_id = EvalRunId::new("bad").unwrap();
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(dir.path().join("bad.json"), b"{ not valid json").unwrap();
        let err = store.load(&run_id).unwrap_err();
        assert!(matches!(err, EvalRunStoreError::Malformed(_)));
    }

    #[test]
    fn save_overwrites_existing_run() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemEvalRunStore::at_dir(dir.path().to_path_buf());
        let mut original = make_run("r1");
        store.save(&original).unwrap();

        // Simulate a retry — change the timing and re-save under the same id.
        original.case_results[0].timing_ms = 9999;
        store.save(&original).unwrap();

        let loaded = store.load(&original.run_id).unwrap().unwrap();
        assert_eq!(loaded.case_results[0].timing_ms, 9999);
    }

    #[test]
    fn list_run_ids_returns_empty_for_nonexistent_dir() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemEvalRunStore::at_dir(dir.path().join("runs"));
        let ids = store.list_run_ids().unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn list_run_ids_returns_empty_for_empty_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path()).unwrap();
        let store = FilesystemEvalRunStore::at_dir(dir.path().to_path_buf());
        let ids = store.list_run_ids().unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn list_run_ids_returns_saved_runs_sorted() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemEvalRunStore::at_dir(dir.path().to_path_buf());
        store.save(&make_run("zeta")).unwrap();
        store.save(&make_run("alpha")).unwrap();
        store.save(&make_run("middle")).unwrap();
        let ids = store.list_run_ids().unwrap();
        let id_strs: Vec<&str> = ids.iter().map(EvalRunId::as_str).collect();
        assert_eq!(id_strs, vec!["alpha", "middle", "zeta"]);
    }

    #[test]
    fn list_run_ids_ignores_tmp_partial_writes() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemEvalRunStore::at_dir(dir.path().to_path_buf());
        store.save(&make_run("real")).unwrap();
        // Drop a stray `.tmp` file as if a previous save was interrupted.
        fs::write(dir.path().join("ghost.json.tmp"), b"{}").unwrap();
        let ids = store.list_run_ids().unwrap();
        let id_strs: Vec<&str> = ids.iter().map(EvalRunId::as_str).collect();
        assert_eq!(id_strs, vec!["real"]);
    }

    #[test]
    fn list_run_ids_ignores_non_json_files() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemEvalRunStore::at_dir(dir.path().to_path_buf());
        store.save(&make_run("good")).unwrap();
        fs::write(dir.path().join("readme.txt"), b"hello").unwrap();
        fs::write(dir.path().join(".hidden.json"), b"{}").unwrap();
        let ids = store.list_run_ids().unwrap();
        let id_strs: Vec<&str> = ids.iter().map(EvalRunId::as_str).collect();
        assert_eq!(id_strs, vec!["good"]);
    }

    #[test]
    fn list_run_ids_skips_files_with_unparseable_run_id() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemEvalRunStore::at_dir(dir.path().to_path_buf());
        store.save(&make_run("good")).unwrap();
        // EvalRunId::new rejects empty strings — drop an empty-name file to verify.
        fs::write(dir.path().join(".json"), b"{}").unwrap();
        let ids = store.list_run_ids().unwrap();
        let id_strs: Vec<&str> = ids.iter().map(EvalRunId::as_str).collect();
        assert_eq!(id_strs, vec!["good"]);
    }

    #[test]
    fn with_default_path_resolves_under_claude_dir() {
        let store = FilesystemEvalRunStore::with_default_path().unwrap();
        let p = store.base_dir().display().to_string();
        assert!(p.contains(".claude"));
        assert!(p.contains("ba-corpus"));
        assert!(p.contains("runs"));
    }

    #[test]
    fn save_writes_no_dangling_tmp_file() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemEvalRunStore::at_dir(dir.path().to_path_buf());
        store.save(&make_run("r1")).unwrap();
        // No `.tmp` file should be left behind after a clean save.
        let tmp_path = dir.path().join("r1.json.tmp");
        assert!(!tmp_path.exists());
    }
}
