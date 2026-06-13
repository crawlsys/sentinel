//! Cross-session proof chain archive.
//!
//! On Stop, every active `ProofChain` in `SessionState.proof_chains` gets
//! serialized to `~/.claude/sentinel/proofs/<session_id>__<skill>.json` plus
//! a one-line summary appended to `~/.claude/sentinel/proofs/index.jsonl`.
//! `query_proof_corpus` reads the index to surface chains from prior
//! sessions — this is what turns the M4.3 corpus query from
//! "live-session-only" into "cross-session," which is the load-bearing
//! input for M7.5 router-as-planner ("for prompts like X, which step
//! sequences had which success rates across all past chains?").
//!
//! # Storage shape
//!
//! ```text
//! ~/.claude/sentinel/proofs/
//! ├── index.jsonl                        ← append-only summary log
//! ├── <session_id>__<skill>.json         ← versioned full chain
//! └── <session_id>__<skill>.json         ← one file per (session, skill)
//! ```
//!
//! One file per `(session_id, skill)` pair so a Stop fire that sees N
//! distinct chains writes N independent files — no read-modify-write,
//! no concurrency landmines if multiple Stops fire close together.
//!
//! # Schema versioning
//!
//! Every archived JSON carries `"schema_version": 1` at the top level.
//! Index entries also carry it. Cheap to add now, painful to retrofit
//! later — bumping to v2 in the future means readers can dispatch on
//! the field instead of trying-and-falling-back on parse shape.
//!
//! # Idempotency
//!
//! Stop firing twice for the same session must not duplicate index
//! entries or corrupt the chain JSON. Implementation: write the chain
//! JSON via atomic-rename (always overwrites the previous archive
//! cleanly), and skip the index append when the index already has a
//! line for that `(session_id, skill)` key. Net effect: re-runs are
//! safe; the index has at most one entry per pair, and the JSON
//! always reflects the most recent state.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sentinel_domain::ports::FileSystemPort;
use sentinel_domain::proof::{ProofChain, ProofEntry};
use sentinel_domain::state::SessionState;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Current schema version for archived chain JSONs and index entries.
/// Bump when the on-disk shape changes in a non-additive way.
pub const SCHEMA_VERSION: u32 = 1;

/// Versioned wrapper around a `ProofChain` for on-disk storage.
///
/// The wrapper exists so future schema bumps (added/renamed fields,
/// changed semantics) can be detected on read without trying-to-parse-and-
/// fall-back. Today's reader only honors v1; a v2 reader will dispatch
/// on this field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedChainRecord {
    /// Schema version of the on-disk format. See [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// When the archive was written (RFC3339 UTC).
    pub archived_at: DateTime<Utc>,
    /// The chain as it stood at archive time. Carries skill, `session_id`,
    /// entries, `head_hash` — everything `query_proof_corpus` needs.
    pub chain: ProofChain,
}

/// One line in `index.jsonl`. Compact summary used by router queries
/// without dragging full `StepProof` payloads across the MCP boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedChainSummary {
    /// Schema version of this index entry. See [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Session that produced the chain.
    pub session_id: String,
    /// Skill name (linear, git, deploy, ...).
    pub skill: String,
    /// Number of step-level proofs in the chain.
    pub step_count: usize,
    /// Number of phase-level proofs.
    pub phase_count: usize,
    /// Head hash — chain tip at archive time. Pairs with the full JSON
    /// for tamper detection.
    pub head_hash: String,
    /// Whether all judge verdicts in the chain were sufficient.
    pub all_sufficient: bool,
    /// Step coordinates as `phase_id.step_id`, in chain order.
    pub step_sequence: Vec<String>,
    /// When the archive was written (RFC3339 UTC).
    pub archived_at: DateTime<Utc>,
}

/// Compute `~/.claude/sentinel/proofs/`.
fn proofs_dir(home: &Path) -> PathBuf {
    home.join(".claude").join("sentinel").join("proofs")
}

/// Compute the per-(session, skill) chain JSON path.
fn chain_path(home: &Path, session_id: &str, skill: &str) -> PathBuf {
    let safe_session = session_id.replace(['/', '\\', ':'], "_");
    let safe_skill = skill.replace(['/', '\\', ':'], "_");
    proofs_dir(home).join(format!("{safe_session}__{safe_skill}.json"))
}

/// Compute the index.jsonl path.
fn index_path(home: &Path) -> PathBuf {
    proofs_dir(home).join("index.jsonl")
}

/// Load every archived [`ProofChain`] for `skill` across all prior sessions.
///
/// Reads the full `<session>__<skill>.json` records (not the index summaries),
/// so callers get the complete `entries` — e.g. `step_anomaly` comparing the
/// current run against the historical distribution of prior runs of a step.
/// Missing dir / unreadable / wrong-schema / wrong-skill files are skipped;
/// the result is best-effort (an empty Vec on a cold machine is correct, not
/// an error). Chains are returned in filesystem-listing order.
#[must_use]
pub fn read_chains_for_skill(fs: &dyn FileSystemPort, home: &Path, skill: &str) -> Vec<ProofChain> {
    let safe_skill = skill.replace(['/', '\\', ':'], "_");
    let suffix = format!("__{safe_skill}.json");
    let Ok(entries) = fs.read_dir(&proofs_dir(home)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for path in entries {
        let is_match = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(&suffix));
        if !is_match {
            continue;
        }
        let Ok(content) = fs.read_to_string(&path) else {
            continue;
        };
        if let Ok(record) = serde_json::from_str::<ArchivedChainRecord>(&content) {
            // Only honor schemas we understand (today: v1). Belt-and-braces
            // double-check the chain's own skill matches, in case of a
            // filename collision after sanitisation.
            if record.schema_version <= SCHEMA_VERSION && record.chain.skill == skill {
                out.push(record.chain);
            }
        }
    }
    out
}

/// Build the summary line for a chain at archive time.
fn summarize(chain: &ProofChain) -> ArchivedChainSummary {
    let step_entries: Vec<&sentinel_domain::step_proof::StepProof> = chain
        .entries
        .iter()
        .filter_map(|e| match e {
            ProofEntry::Step(s) => Some(s),
            // Disagreement markers carry their own per-judge breakdown
            // (#82 Stage B); the corpus summary doesn't aggregate them
            // today. A future commit can extend ArchivedChainSummary to
            // surface disagreement counts per skill — for now skip.
            ProofEntry::Phase(_) | ProofEntry::Disagreement(_) => None,
        })
        .collect();

    let phase_count = chain.proofs.len()
        + chain
            .entries
            .iter()
            .filter(|e| matches!(e, ProofEntry::Phase(_)))
            .count();

    let all_sufficient = step_entries.iter().all(|s| s.judge_verdict.sufficient)
        && chain.proofs.iter().all(|p| p.judge_verdict.sufficient);

    let step_sequence: Vec<String> = step_entries
        .iter()
        .map(|s| format!("{}.{}", s.phase_id, s.step_id))
        .collect();

    ArchivedChainSummary {
        schema_version: SCHEMA_VERSION,
        session_id: chain.session_id.clone(),
        skill: chain.skill.clone(),
        step_count: step_entries.len(),
        phase_count,
        head_hash: chain.head_hash().to_string(),
        all_sufficient,
        step_sequence,
        archived_at: Utc::now(),
    }
}

/// Check whether the index already contains an entry for a given
/// `(session_id, skill)` pair. Idempotency gate so re-running Stop
/// for the same session doesn't duplicate index lines.
fn index_has_entry(fs: &dyn FileSystemPort, home: &Path, session_id: &str, skill: &str) -> bool {
    let path = index_path(home);
    let Ok(content) = fs.read_to_string(&path) else {
        return false;
    };
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<ArchivedChainSummary>(trimmed) {
            if entry.session_id == session_id && entry.skill == skill {
                return true;
            }
        }
    }
    false
}

/// Archive every chain in the session state.
///
/// Best-effort: per-chain failures are logged via `tracing::warn!` and
/// skipped — a single bad chain doesn't block the rest. The Stop hook
/// caller treats the overall result as advisory; a full failure should
/// not abort session shutdown.
///
/// Idempotent: re-running over the same state writes the chain JSONs
/// again (atomic overwrite with the latest snapshot — same content if
/// nothing changed) but does NOT duplicate index lines.
pub fn archive_chains(state: &SessionState, fs: &dyn FileSystemPort, home: &Path) -> Result<()> {
    fs.create_dir_all(&proofs_dir(home))
        .context("create proofs archive dir")?;

    for (skill, chain) in &state.proof_chains {
        if let Err(e) = archive_one_chain(fs, home, skill, chain) {
            tracing::warn!(
                skill = %skill,
                session_id = %chain.session_id,
                error = %e,
                "proof chain archive failed for chain — continuing"
            );
        }
    }
    Ok(())
}

/// Archive a single chain. Atomic write of the JSON + idempotent index append.
fn archive_one_chain(
    fs: &dyn FileSystemPort,
    home: &Path,
    skill: &str,
    chain: &ProofChain,
) -> Result<()> {
    let session_id = &chain.session_id;
    let path = chain_path(home, session_id, skill);

    let record = ArchivedChainRecord {
        schema_version: SCHEMA_VERSION,
        archived_at: Utc::now(),
        chain: chain.clone(),
    };

    let json_bytes = serde_json::to_vec_pretty(&record).context("serialize archived chain")?;

    // Atomic write via tmp + rename. The fs.write impl handles parent dir
    // creation, but for atomicity we go through the .tmp dance ourselves so
    // a crashed Stop can't leave half-written JSON in the bucket.
    let tmp_path = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name().map_or_else(|| "chain".to_string(), |n| n.to_string_lossy().into_owned())
    ));
    fs.write(&tmp_path, &json_bytes)
        .context("write archived chain tmp file")?;
    // Rename through std::fs since the port doesn't expose rename. Fall
    // back to a plain write if rename fails (cross-device or permissions
    // edge cases) — better to have a non-atomic write than no archive.
    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        tracing::warn!(
            error = %e,
            path = %path.display(),
            "atomic rename failed — falling back to direct write"
        );
        fs.write(&path, &json_bytes)
            .context("fallback direct write of archived chain")?;
        // Best-effort cleanup of the leftover tmp.
        let _ = std::fs::remove_file(&tmp_path);
    }

    // Append to index only if no entry yet — idempotency.
    if !index_has_entry(fs, home, session_id, skill) {
        let summary = summarize(chain);
        let mut line = serde_json::to_string(&summary).context("serialize index entry")?;
        line.push('\n');
        fs.append(&index_path(home), line.as_bytes())
            .context("append index entry")?;
    }

    Ok(())
}

/// Read all index entries. Malformed lines (corrupted writes, schema
/// drift) are silently skipped — index reads must never fail callers.
pub fn read_index(fs: &dyn FileSystemPort, home: &Path) -> Vec<ArchivedChainSummary> {
    let path = index_path(home);
    let Ok(content) = fs.read_to_string(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<ArchivedChainSummary>(trimmed) {
            // Future-proofing: skip entries from a future schema we don't
            // know how to read. Today only v1 exists.
            if entry.schema_version <= SCHEMA_VERSION {
                out.push(entry);
            }
        }
        // Malformed lines silently dropped — see module doc.
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sentinel_domain::evidence::Evidence;
    use sentinel_domain::judge::JudgeVerdict;
    use sentinel_domain::proof::{PhaseProof, ProofChain, GENESIS_HASH};
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Real-FS adapter scoped to a tempdir-supplied home. Mirrors the
    /// pattern used in `hooks/mod.rs` migrate tests.
    struct ScopedHomeFs {
        home: PathBuf,
    }
    impl FileSystemPort for ScopedHomeFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(&self, p: &Path) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            std::fs::read_to_string(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn write(&self, p: &Path, c: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par).map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
            }
            std::fs::write(p, c).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn create_dir_all(&self, p: &Path) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            std::fs::create_dir_all(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn read_dir(&self, p: &Path) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            std::fs::read_dir(p)
                .map_err(sentinel_domain::port_errors::FileSystemError::backend)
                .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(&self, p: &Path) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            std::fs::metadata(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn append(&self, p: &Path, c: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            use std::io::Write;
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par).map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
            f.write_all(c).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
    }

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Construct a PhaseProof matching the canonical fixture pattern in
    /// `proof.rs::tests::make_proof`. Hashes are computed correctly so
    /// `chain.add_proof` accepts the result.
    fn make_phase_proof(phase_id: &str, skill: &str, session_id: &str, prev: &str) -> PhaseProof {
        let evidence = Evidence::default();
        let evidence_hash = PhaseProof::compute_evidence_hash(&evidence);
        let combined_hash =
            PhaseProof::compute_combined_hash(phase_id, skill, &evidence_hash, prev, true);
        let now = Utc::now();
        PhaseProof {
            phase_id: phase_id.to_string(),
            skill: skill.to_string(),
            session_id: session_id.to_string(),
            evidence,
            evidence_hash,
            previous_hash: prev.to_string(),
            combined_hash,
            judge_model: "kimi".to_string(),
            judge_verdict: JudgeVerdict::pass(0.95, "ok"),
            started_at: now,
            completed_at: now,
            duration_ms: 1,
        }
    }

    /// Build a minimal SessionState with one chain containing one phase proof.
    fn build_state(session_id: &str, skill: &str) -> SessionState {
        let mut state = SessionState::new(session_id);
        let phase = make_phase_proof("claim", skill, session_id, GENESIS_HASH);
        let mut chain = ProofChain::new(skill, session_id);
        chain.add_proof(phase).unwrap();
        state.proof_chains.insert(skill.to_string(), chain);
        state
    }

    #[test]
    fn archive_writes_chain_json_with_schema_version_1() {
        let _g = ENV_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let state = build_state("sess-1", "linear");

        archive_chains(&state, &fs, tmp.path()).unwrap();

        let p = chain_path(tmp.path(), "sess-1", "linear");
        let raw = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["schema_version"].as_u64(), Some(1));
        assert!(v["archived_at"].is_string());
        assert_eq!(v["chain"]["skill"].as_str(), Some("linear"));
        assert_eq!(v["chain"]["session_id"].as_str(), Some("sess-1"));
    }

    #[test]
    fn archive_appends_one_index_line_per_chain() {
        let _g = ENV_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let state = build_state("sess-1", "linear");

        archive_chains(&state, &fs, tmp.path()).unwrap();

        let idx = std::fs::read_to_string(index_path(tmp.path())).unwrap();
        let lines: Vec<&str> = idx.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "expected one index line, got {idx:?}");
        let summary: ArchivedChainSummary = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(summary.schema_version, SCHEMA_VERSION);
        assert_eq!(summary.session_id, "sess-1");
        assert_eq!(summary.skill, "linear");
        // build_state currently produces a phase-only chain (no step proofs).
        // step_count == 0 is the correct assertion for this fixture; the
        // step-count > 0 path is exercised in the live-session tests in
        // mcp_handler::query_proof_corpus_returns_summaries_for_live_chains.
        assert_eq!(summary.step_count, 0);
        assert_eq!(summary.phase_count, 1);
        assert!(summary.all_sufficient);
        assert!(summary.step_sequence.is_empty());
    }

    #[test]
    fn read_chains_for_skill_round_trips_archived_chains() {
        let _g = ENV_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        // Two prior sessions of `linear`, one of `git`.
        archive_chains(&build_state("sess-a", "linear"), &fs, tmp.path()).unwrap();
        archive_chains(&build_state("sess-b", "linear"), &fs, tmp.path()).unwrap();
        archive_chains(&build_state("sess-c", "git"), &fs, tmp.path()).unwrap();

        let linear = read_chains_for_skill(&fs, tmp.path(), "linear");
        assert_eq!(linear.len(), 2, "two archived linear chains expected");
        assert!(linear.iter().all(|c| c.skill == "linear"));

        let git = read_chains_for_skill(&fs, tmp.path(), "git");
        assert_eq!(git.len(), 1);

        // Unknown skill / cold machine → empty, not an error.
        assert!(read_chains_for_skill(&fs, tmp.path(), "deploy").is_empty());
        let cold = tempfile::tempdir().unwrap();
        let cold_fs = ScopedHomeFs {
            home: cold.path().to_path_buf(),
        };
        assert!(read_chains_for_skill(&cold_fs, cold.path(), "linear").is_empty());
    }

    #[test]
    fn archive_is_idempotent_no_duplicate_index_lines() {
        let _g = ENV_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let state = build_state("sess-1", "linear");

        archive_chains(&state, &fs, tmp.path()).unwrap();
        archive_chains(&state, &fs, tmp.path()).unwrap();
        archive_chains(&state, &fs, tmp.path()).unwrap();

        let idx = std::fs::read_to_string(index_path(tmp.path())).unwrap();
        let count = idx.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(count, 1, "re-runs must not duplicate index entries");
    }

    #[test]
    fn archive_one_file_per_session_skill_pair() {
        let _g = ENV_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        // Two skills in one session.
        let mut state = SessionState::new("sess-1");
        for skill in ["linear", "git"] {
            let phase = make_phase_proof("claim", skill, "sess-1", GENESIS_HASH);
            let mut chain = ProofChain::new(skill, "sess-1");
            chain.add_proof(phase).unwrap();
            state.proof_chains.insert(skill.to_string(), chain);
        }

        archive_chains(&state, &fs, tmp.path()).unwrap();

        assert!(chain_path(tmp.path(), "sess-1", "linear").exists());
        assert!(chain_path(tmp.path(), "sess-1", "git").exists());
        let idx = std::fs::read_to_string(index_path(tmp.path())).unwrap();
        let lines: Vec<&str> = idx.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2, "two skills => two index lines");
    }

    #[test]
    fn read_index_returns_summaries_and_skips_malformed() {
        let _g = ENV_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let state = build_state("sess-1", "linear");
        archive_chains(&state, &fs, tmp.path()).unwrap();

        // Append a malformed line that read_index must drop silently.
        std::fs::OpenOptions::new()
            .append(true)
            .open(index_path(tmp.path()))
            .unwrap();
        let mut content = std::fs::read_to_string(index_path(tmp.path())).unwrap();
        content.push_str("{this-is-not-json\n\n");
        std::fs::write(index_path(tmp.path()), content).unwrap();

        let entries = read_index(&fs, tmp.path());
        assert_eq!(entries.len(), 1, "malformed line must be skipped");
        assert_eq!(entries[0].session_id, "sess-1");
    }

    #[test]
    fn read_index_returns_empty_when_no_archive() {
        let _g = ENV_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let entries = read_index(&fs, tmp.path());
        assert!(entries.is_empty());
    }

    #[test]
    fn archive_skips_session_id_with_separator_chars_safely() {
        let _g = ENV_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        // session ids in production are UUID-shaped, but be defensive
        // against / \ : appearing — they'd break the file path.
        let state = build_state("sess/with/slash", "linear");
        archive_chains(&state, &fs, tmp.path()).unwrap();

        // The on-disk file name has slashes replaced; chain_path mirrors.
        let p = chain_path(tmp.path(), "sess/with/slash", "linear");
        assert!(p.exists(), "archive must write under sanitized path");
    }

    #[test]
    fn read_index_skips_future_schema_versions() {
        let _g = ENV_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        // Hand-craft an index entry with schema_version = 999 (future).
        let future_entry = serde_json::json!({
            "schema_version": 999,
            "session_id": "future",
            "skill": "linear",
            "step_count": 0,
            "phase_count": 0,
            "head_hash": "GENESIS",
            "all_sufficient": true,
            "step_sequence": [],
            "archived_at": "2030-01-01T00:00:00Z",
        });
        let line = format!("{future_entry}\n");
        fs.create_dir_all(&proofs_dir(tmp.path())).unwrap();
        fs.append(&index_path(tmp.path()), line.as_bytes()).unwrap();

        let entries = read_index(&fs, tmp.path());
        assert!(
            entries.is_empty(),
            "v999 entries must be skipped by v1 reader"
        );
    }

    #[test]
    fn empty_session_state_archive_is_no_op() {
        let _g = ENV_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        let state = SessionState::new("sess-empty");
        archive_chains(&state, &fs, tmp.path()).unwrap();

        // No chains in state => proofs dir created, but no JSONs and no index.
        assert!(proofs_dir(tmp.path()).is_dir());
        assert!(!index_path(tmp.path()).exists());
    }
}
