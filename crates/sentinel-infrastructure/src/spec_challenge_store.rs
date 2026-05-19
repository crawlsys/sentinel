//! A13 Phase 4a — Filesystem-backed [`SpecChallenge`] store.
//!
//! Persists each emitted challenge as one JSON file per
//! [`WorkId`](sentinel_domain::spec_challenge::WorkId) at
//! `~/.claude/sentinel/state/spec-challenges/{work_id}.json`.
//! Re-emissions for the same `work_id` overwrite — per spec §6,
//! the typical case is "agent re-attempted after the first
//! challenge was rejected." Mirrors [`crate::eval_run_store`]'s
//! shape (temp+rename atomic writes, lazy directory creation,
//! `Ok(None)` vs `Err(Malformed)` discrimination on load).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use sentinel_domain::ports::{SpecChallengeStoreError, SpecChallengeStorePort};
use sentinel_domain::spec_challenge::{SpecChallenge, WorkId};

/// Filesystem-backed `SpecChallengeStorePort` implementation.
pub struct FilesystemSpecChallengeStore {
    base_dir: PathBuf,
}

impl std::fmt::Debug for FilesystemSpecChallengeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemSpecChallengeStore")
            .field("base_dir", &self.base_dir)
            .finish()
    }
}

impl FilesystemSpecChallengeStore {
    /// Construct pointed at a specific directory. Used by tests to
    /// scope to a tempdir. The directory does NOT need to exist —
    /// `save` creates it lazily.
    #[must_use]
    pub const fn at_dir(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Construct with the default path
    /// (`~/.claude/sentinel/state/spec-challenges/`).
    pub fn with_default_path() -> Result<Self> {
        let home =
            dirs::home_dir().context("home directory not resolvable from environment")?;
        let base_dir = home
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join("spec-challenges");
        Ok(Self::at_dir(base_dir))
    }

    /// Read-only access to the base directory.
    #[must_use]
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    fn challenge_path(&self, work_id: &WorkId) -> PathBuf {
        self.base_dir.join(format!("{}.json", work_id.as_str()))
    }
}

impl SpecChallengeStorePort for FilesystemSpecChallengeStore {
    fn save(&self, challenge: &SpecChallenge) -> Result<(), SpecChallengeStoreError> {
        if !self.base_dir.exists() {
            fs::create_dir_all(&self.base_dir).map_err(|e| {
                SpecChallengeStoreError::StoreUnavailable(format!(
                    "create challenges dir {}: {e}",
                    self.base_dir.display()
                ))
            })?;
        }
        let final_path = self.challenge_path(&challenge.work_id);
        let tmp_path = final_path.with_extension("json.tmp");

        let json = serde_json::to_string_pretty(challenge).map_err(|e| {
            SpecChallengeStoreError::Malformed(format!(
                "serialize challenge {}: {e}",
                challenge.work_id.as_str()
            ))
        })?;

        fs::write(&tmp_path, json).map_err(|e| {
            SpecChallengeStoreError::StoreUnavailable(format!(
                "write {}: {e}",
                tmp_path.display()
            ))
        })?;

        fs::rename(&tmp_path, &final_path).map_err(|e| {
            SpecChallengeStoreError::StoreUnavailable(format!(
                "rename {} -> {}: {e}",
                tmp_path.display(),
                final_path.display(),
            ))
        })?;
        Ok(())
    }

    fn load(
        &self,
        work_id: &WorkId,
    ) -> Result<Option<SpecChallenge>, SpecChallengeStoreError> {
        let path = self.challenge_path(work_id);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(SpecChallengeStoreError::StoreUnavailable(format!(
                    "read {}: {e}",
                    path.display()
                )));
            }
        };
        let challenge: SpecChallenge = serde_json::from_slice(&bytes).map_err(|e| {
            SpecChallengeStoreError::Malformed(format!("parse {}: {e}", path.display()))
        })?;
        Ok(Some(challenge))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use sentinel_domain::reversibility::ReversibilityClass;
    use sentinel_domain::spec_challenge::{
        Alternative, Ambiguity, Assumption, AssumptionConfidence, ChallengeCategory,
        GapResolution, SpecChallenge, SpecGap, SpecReference, WorkId,
    };
    use tempfile::TempDir;

    fn make_challenge(work_id: &str) -> SpecChallenge {
        SpecChallenge {
            work_id: WorkId::new(work_id).unwrap(),
            agent_id: "agent-x".to_string(),
            challenged_spec: SpecReference {
                hash: "abc".to_string(),
                source: "issue X".to_string(),
            },
            reversibility_class: ReversibilityClass::Irreversible,
            assumptions: ChallengeCategory::new(vec![Assumption {
                statement: "postgres".to_string(),
                confidence: AssumptionConfidence::High,
                blast_if_wrong: ReversibilityClass::Irreversible,
            }]),
            gaps: ChallengeCategory::new(vec![SpecGap {
                topic: "auth".to_string(),
                how_resolved: GapResolution::OperatorClarified,
                inference_source: None,
            }]),
            ambiguities: ChallengeCategory::new(vec![Ambiguity {
                spec_excerpt: "ship fast".to_string(),
                interpretations: vec!["p99".to_string(), "throughput".to_string()],
                chosen: "p99".to_string(),
                rationale: "user-visible".to_string(),
            }]),
            alternatives_considered: ChallengeCategory::new(vec![Alternative {
                description: "redis".to_string(),
                why_rejected: "durability".to_string(),
            }]),
            constraints_not_satisfied: ChallengeCategory::none("all met"),
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn save_creates_challenges_directory_if_missing() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("spec-challenges");
        assert!(!base.exists());
        let store = FilesystemSpecChallengeStore::at_dir(base.clone());
        store.save(&make_challenge("w1")).unwrap();
        assert!(base.exists());
        assert!(base.join("w1.json").exists());
    }

    #[test]
    fn save_then_load_roundtrips_full_challenge() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemSpecChallengeStore::at_dir(dir.path().to_path_buf());
        let original = make_challenge("w1");
        store.save(&original).unwrap();
        let loaded = store.load(&original.work_id).unwrap().unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn load_returns_ok_none_when_challenge_missing() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemSpecChallengeStore::at_dir(dir.path().to_path_buf());
        let ghost = WorkId::new("nonexistent").unwrap();
        let result = store.load(&ghost).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_returns_malformed_when_file_corrupt() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemSpecChallengeStore::at_dir(dir.path().to_path_buf());
        let work_id = WorkId::new("bad").unwrap();
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(dir.path().join("bad.json"), b"{ not valid json").unwrap();
        let err = store.load(&work_id).unwrap_err();
        assert!(matches!(err, SpecChallengeStoreError::Malformed(_)));
    }

    #[test]
    fn save_overwrites_existing_challenge() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemSpecChallengeStore::at_dir(dir.path().to_path_buf());
        let mut original = make_challenge("w1");
        store.save(&original).unwrap();

        // Simulate a retry — change the agent_id and re-save.
        original.agent_id = "agent-retry".to_string();
        store.save(&original).unwrap();

        let loaded = store.load(&original.work_id).unwrap().unwrap();
        assert_eq!(loaded.agent_id, "agent-retry");
    }

    #[test]
    fn save_writes_no_dangling_tmp_file() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemSpecChallengeStore::at_dir(dir.path().to_path_buf());
        store.save(&make_challenge("w1")).unwrap();
        let tmp_path = dir.path().join("w1.json.tmp");
        assert!(!tmp_path.exists());
    }

    #[test]
    fn with_default_path_resolves_under_claude_dir() {
        let store = FilesystemSpecChallengeStore::with_default_path().unwrap();
        let p = store.base_dir().display().to_string();
        assert!(p.contains(".claude"));
        assert!(p.contains("spec-challenges"));
    }

    #[test]
    fn different_work_ids_get_different_files() {
        let dir = TempDir::new().unwrap();
        let store = FilesystemSpecChallengeStore::at_dir(dir.path().to_path_buf());
        store.save(&make_challenge("alpha")).unwrap();
        store.save(&make_challenge("beta")).unwrap();
        let alpha = store.load(&WorkId::new("alpha").unwrap()).unwrap().unwrap();
        let beta = store.load(&WorkId::new("beta").unwrap()).unwrap().unwrap();
        assert_eq!(alpha.work_id.as_str(), "alpha");
        assert_eq!(beta.work_id.as_str(), "beta");
    }
}
