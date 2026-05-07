//! `sentinel cleanup` — prune orphan state directories left behind by old paths.
//!
//! Today's primary subcommand is `cleanup persistent-tasks` which prunes
//! orphan project-hash buckets under `~/.claude/sentinel/persistent-tasks/`.
//! These accumulate every time sentinel sees a new cwd — including every
//! removed git worktree path, since the worktree-collapse fix to
//! `project_hash` only landed on commit 5ac549a (this session's first
//! merge to main). Anything from before that lives under the un-canonical
//! per-worktree-path hash, no living cwd points to it, and the data is
//! pure bloat.
//!
//! Default mode is dry-run — print orphans, don't touch them. `--apply`
//! does the actual removal.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One bucket under `persistent-tasks/`. Carries the `cwd` recorded by
/// `task_persist` so we can decide whether the path still exists. The
/// `project_hash` field is read by serde for shape-validation only — we
/// already have the hash from the directory name, so the meta copy is
/// just a sanity check.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)] // project_hash is shape-validation only
struct PersistMeta {
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    project_hash: String,
    #[serde(default)]
    task_count: u64,
    #[serde(default)]
    incomplete_count: u64,
    #[serde(default)]
    updated_at: String,
}

/// Outcome of inspecting one bucket. Returned in dry-run output and used
/// internally for `--apply` to drive removals.
///
/// `Live { cwd, task_count }` carries data that today's `print_report`
/// only summarises numerically — the fields are kept for the
/// machine-readable report variant that's coming next (M6.2 dashboard
/// integration) and for `Debug` output during diagnosis. Marking
/// `dead_code` here rather than removing keeps that future surface in
/// place.
#[derive(Debug)]
#[allow(dead_code)] // Live fields read via Debug + future report formats
enum Status {
    /// `meta.json` is present, `cwd` resolves to a directory that exists
    /// on disk — leave it alone.
    Live { cwd: String, task_count: u64 },
    /// `meta.json` is present, `cwd` does NOT exist on disk — orphan.
    Orphan {
        cwd: String,
        task_count: u64,
        incomplete_count: u64,
        updated_at: String,
    },
    /// `meta.json` is missing or malformed — treat as orphan since we
    /// can't tell what cwd it was tied to.
    Unreadable { reason: String },
}

/// Run the cleanup. `apply == false` is dry-run; `apply == true` removes
/// orphan buckets. Returns the human-readable report as String so the
/// dispatch site can `println!` it (cleaner than mid-function I/O).
pub fn run_persistent_tasks(apply: bool) -> Result<()> {
    let home = dirs::home_dir().context("could not resolve home dir")?;
    let root = home
        .join(".claude")
        .join("sentinel")
        .join("persistent-tasks");
    if !root.is_dir() {
        println!("No persistent-tasks directory at {}", root.display());
        return Ok(());
    }

    let mut buckets: HashMap<String, Status> = HashMap::new();
    for entry in std::fs::read_dir(&root)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        buckets.insert(name, classify(&path));
    }

    print_report(&buckets);

    if apply {
        let removed = apply_removals(&root, &buckets)?;
        println!(
            "\nApplied: removed {removed} orphan bucket(s). \
             Live buckets preserved."
        );
    } else {
        println!(
            "\nDry-run only — pass --apply to actually remove the orphan buckets above."
        );
    }
    Ok(())
}

/// Classify one bucket by reading its `meta.json` and stat-ing the cwd.
fn classify(bucket_dir: &Path) -> Status {
    let meta_path = bucket_dir.join("meta.json");
    let content = match std::fs::read_to_string(&meta_path) {
        Ok(c) => c,
        Err(e) => {
            return Status::Unreadable {
                reason: format!("cannot read meta.json: {e}"),
            }
        }
    };
    let meta: PersistMeta = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(e) => {
            return Status::Unreadable {
                reason: format!("malformed meta.json: {e}"),
            }
        }
    };
    if meta.cwd.is_empty() {
        return Status::Unreadable {
            reason: "meta.json has empty cwd".to_string(),
        };
    }
    let cwd_path = PathBuf::from(&meta.cwd);
    if cwd_path.is_dir() {
        Status::Live {
            cwd: meta.cwd,
            task_count: meta.task_count,
        }
    } else {
        Status::Orphan {
            cwd: meta.cwd,
            task_count: meta.task_count,
            incomplete_count: meta.incomplete_count,
            updated_at: meta.updated_at,
        }
    }
}

/// Print the report. Live buckets first (count only — they're not
/// actionable), then unreadable, then orphans (the ones --apply will
/// remove).
fn print_report(buckets: &HashMap<String, Status>) {
    let live = buckets
        .values()
        .filter(|s| matches!(s, Status::Live { .. }))
        .count();
    let unreadable: Vec<(&String, &String)> = buckets
        .iter()
        .filter_map(|(h, s)| match s {
            Status::Unreadable { reason } => Some((h, reason)),
            _ => None,
        })
        .collect();
    let orphans: Vec<(&String, &str, u64, u64, &str)> = buckets
        .iter()
        .filter_map(|(h, s)| match s {
            Status::Orphan {
                cwd,
                task_count,
                incomplete_count,
                updated_at,
            } => Some((
                h,
                cwd.as_str(),
                *task_count,
                *incomplete_count,
                updated_at.as_str(),
            )),
            _ => None,
        })
        .collect();

    println!("Persistent-tasks bucket audit:");
    println!("  total buckets: {}", buckets.len());
    println!("  live (cwd still exists): {live}");
    println!("  unreadable (missing/malformed meta): {}", unreadable.len());
    println!("  orphans (cwd no longer exists): {}", orphans.len());

    if !unreadable.is_empty() {
        println!("\nUnreadable buckets (treated as orphans for cleanup):");
        for (hash, reason) in &unreadable {
            println!("  {hash}: {reason}");
        }
    }

    if !orphans.is_empty() {
        println!("\nOrphans:");
        for (hash, cwd, tasks, incomplete, updated) in &orphans {
            println!(
                "  {hash}  tasks={tasks} (incomplete={incomplete})  updated={updated}\n    cwd: {cwd}"
            );
        }
    }
}

/// Remove all orphan and unreadable buckets. Returns the count actually
/// removed. Per-bucket failures are logged but don't abort.
fn apply_removals(root: &Path, buckets: &HashMap<String, Status>) -> Result<usize> {
    let mut removed = 0usize;
    for (hash, status) in buckets {
        let should_remove = matches!(status, Status::Orphan { .. } | Status::Unreadable { .. });
        if !should_remove {
            continue;
        }
        let path = root.join(hash);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                removed += 1;
            }
            Err(e) => {
                eprintln!(
                    "  warning: failed to remove {} — {e}",
                    path.display()
                );
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_bucket(root: &Path, hash: &str, cwd: &str, tasks: u64) {
        let dir = root.join(hash);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "project_hash": hash,
            "cwd": cwd,
            "session_id": "test-session",
            "updated_at": "2026-05-06T22:00:00Z",
            "task_count": tasks,
            "incomplete_count": 0,
            "last_block_hash": "x"
        });
        std::fs::write(dir.join("meta.json"), meta.to_string()).unwrap();
    }

    #[test]
    fn classify_live_bucket_when_cwd_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("persistent-tasks");
        // The cwd is the tmp dir itself — guaranteed to exist.
        write_bucket(&root, "aaaa1111", &tmp.path().to_string_lossy(), 3);
        let s = classify(&root.join("aaaa1111"));
        assert!(matches!(s, Status::Live { task_count: 3, .. }));
    }

    #[test]
    fn classify_orphan_bucket_when_cwd_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("persistent-tasks");
        write_bucket(&root, "bbbb2222", "/this/path/does/not/exist/anywhere", 7);
        let s = classify(&root.join("bbbb2222"));
        match s {
            Status::Orphan { task_count, .. } => assert_eq!(task_count, 7),
            other => panic!("expected Orphan, got {other:?}"),
        }
    }

    #[test]
    fn classify_unreadable_when_meta_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ccc-no-meta");
        std::fs::create_dir_all(&dir).unwrap();
        let s = classify(&dir);
        assert!(matches!(s, Status::Unreadable { .. }));
    }

    #[test]
    fn classify_unreadable_when_meta_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dddd-bad-json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), "{not json}").unwrap();
        let s = classify(&dir);
        assert!(matches!(s, Status::Unreadable { .. }));
    }

    #[test]
    fn classify_unreadable_when_cwd_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("eeee-empty-cwd");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), r#"{"cwd":""}"#).unwrap();
        let s = classify(&dir);
        assert!(matches!(s, Status::Unreadable { .. }));
    }

    #[test]
    fn apply_removes_orphans_and_unreadable_preserves_live() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("persistent-tasks");
        // Live bucket: cwd points at tmp itself.
        write_bucket(&root, "live0001", &tmp.path().to_string_lossy(), 1);
        // Orphan: cwd doesn't exist.
        write_bucket(&root, "orph0002", "/nope/nowhere", 1);
        // Unreadable: bucket dir exists but no meta.json.
        std::fs::create_dir_all(root.join("badx0003")).unwrap();

        let mut buckets = HashMap::new();
        for h in ["live0001", "orph0002", "badx0003"] {
            buckets.insert(h.to_string(), classify(&root.join(h)));
        }
        let removed = apply_removals(&root, &buckets).unwrap();
        assert_eq!(removed, 2, "must remove orphan + unreadable, keep live");
        assert!(root.join("live0001").exists());
        assert!(!root.join("orph0002").exists());
        assert!(!root.join("badx0003").exists());
    }
}
