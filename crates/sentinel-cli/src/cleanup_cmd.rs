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

// ============================================================================
// `cleanup tasks` — orphan session task directories under ~/.claude/tasks/
// ============================================================================

/// Status of one `~/.claude/tasks/<session_id>/` directory.
///
/// `Live` carries the matching JSONL transcript path purely for diagnosis;
/// `print_report_tasks` only counts them. Marked `dead_code` since the
/// field is read via `Debug` and reserved for the future machine-readable
/// report variant.
#[derive(Debug)]
#[allow(dead_code)]
enum TaskStatus {
    /// `session_id` appears as a `<id>.jsonl` under some project — live
    /// session, keep its tasks dir.
    Live { transcript: PathBuf },
    /// `session_id` has no matching `.jsonl` anywhere AND the dir mtime
    /// is older than the cutoff — safe to remove.
    Orphan {
        age_days: u64,
        size_bytes: u64,
    },
    /// `session_id` has no matching `.jsonl` but the dir mtime is within
    /// the cutoff — could be a fresh session whose transcript hasn't
    /// been written yet. Kept to avoid races.
    YoungerThanCutoff { age_days: u64 },
    /// Directory name is not a valid UUID — could be unrelated state
    /// that crept in. Reported but never removed by --apply.
    NotASession,
}

/// Run the session-task cleanup. `apply == false` is dry-run.
///
/// Algorithm:
/// 1. Scan `~/.claude/projects/**/<id>.jsonl` to build a set of live
///    session IDs.
/// 2. Scan `~/.claude/tasks/<id>/` directories. For each:
///    - If the name isn't a UUID → `NotASession` (reported, never
///      removed).
///    - If the session ID is in the live set → `Live`.
///    - Else, check the directory mtime: if older than `older_than`
///      days → `Orphan` (eligible for removal). Otherwise →
///      `YoungerThanCutoff` (kept defensively).
/// 3. Report. If `apply`, remove `Orphan` directories. `NotASession`
///    and `YoungerThanCutoff` are never auto-removed.
pub fn run_session_tasks(older_than_days: u64, apply: bool) -> Result<()> {
    let home = dirs::home_dir().context("could not resolve home dir")?;
    let tasks_root = home.join(".claude").join("tasks");
    let projects_root = home.join(".claude").join("projects");

    if !tasks_root.is_dir() {
        println!("No tasks directory at {}", tasks_root.display());
        return Ok(());
    }

    let live_sessions = collect_live_session_ids(&projects_root);
    let cutoff_secs = older_than_days.saturating_mul(86_400);
    let now = std::time::SystemTime::now();

    let mut entries: Vec<(String, TaskStatus)> = Vec::new();
    for entry in std::fs::read_dir(&tasks_root)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let status = classify_session_task(&path, &name, &live_sessions, &now, cutoff_secs);
        entries.push((name, status));
    }

    print_report_tasks(&entries, older_than_days);

    if apply {
        let removed = apply_session_task_removals(&tasks_root, &entries)?;
        println!(
            "\nApplied: removed {removed} orphan session task dir(s) older than {older_than_days}d. \
             Live + younger-than-cutoff dirs preserved."
        );
    } else {
        println!(
            "\nDry-run only — pass --apply to actually remove the orphan dirs above."
        );
    }
    Ok(())
}

/// Walk `~/.claude/projects/*/` for `*.jsonl` files. Each filename stem is
/// a `session_id`. Missing or unreadable project dirs are silently skipped
/// (it's a "what's currently alive" snapshot — better to miss a few than
/// fail loud and prevent any cleanup at all).
fn collect_live_session_ids(projects_root: &Path) -> std::collections::HashSet<String> {
    let mut live = std::collections::HashSet::new();
    let Ok(entries) = std::fs::read_dir(projects_root) else {
        return live;
    };
    for project_entry in entries.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&project_path) else {
            continue;
        };
        for file_entry in files.flatten() {
            let file_path = file_entry.path();
            if file_path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = file_path.file_stem().and_then(|s| s.to_str()) {
                live.insert(stem.to_string());
            }
        }
    }
    live
}

/// Classify one tasks/<name>/ directory.
fn classify_session_task(
    path: &Path,
    name: &str,
    live_sessions: &std::collections::HashSet<String>,
    now: &std::time::SystemTime,
    cutoff_secs: u64,
) -> TaskStatus {
    if !looks_like_uuid(name) {
        return TaskStatus::NotASession;
    }
    if live_sessions.contains(name) {
        return TaskStatus::Live {
            transcript: PathBuf::from(format!("~/.claude/projects/<project>/{name}.jsonl")),
        };
    }
    let age_secs = match path.metadata().and_then(|m| m.modified()) {
        Ok(mtime) => now
            .duration_since(mtime)
            .map_or(0, |d| d.as_secs()),
        Err(_) => 0,
    };
    let age_days = age_secs / 86_400;
    if age_secs < cutoff_secs {
        TaskStatus::YoungerThanCutoff { age_days }
    } else {
        let size_bytes = dir_size_bytes(path);
        TaskStatus::Orphan { age_days, size_bytes }
    }
}

/// Best-effort UUID-ish check. Session dirs are RFC4122 UUIDs (lowercase
/// hex, 8-4-4-4-12). We're lax — any 36-char string with hyphens at
/// positions 8/13/18/23 and hex everywhere else passes. Goal is to
/// filter junk dirs (e.g. a stray `.lock` directory or a typo), not
/// validate cryptographically.
fn looks_like_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    let bytes = s.as_bytes();
    bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && s.chars()
            .filter(|c| *c != '-')
            .all(|c| c.is_ascii_hexdigit())
}

/// Recursive directory size in bytes. Best-effort — any unreadable
/// entry is skipped, not propagated. Used only for the audit report.
fn dir_size_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            total = total.saturating_add(dir_size_bytes(&p));
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

fn print_report_tasks(entries: &[(String, TaskStatus)], cutoff_days: u64) {
    let mut live = 0usize;
    let mut younger = 0usize;
    let mut not_session = 0usize;
    let mut orphans: Vec<(&String, u64, u64)> = Vec::new();
    let mut total_orphan_bytes = 0u64;
    for (name, status) in entries {
        match status {
            TaskStatus::Live { .. } => live += 1,
            TaskStatus::YoungerThanCutoff { .. } => younger += 1,
            TaskStatus::NotASession => not_session += 1,
            TaskStatus::Orphan { age_days, size_bytes } => {
                orphans.push((name, *age_days, *size_bytes));
                total_orphan_bytes = total_orphan_bytes.saturating_add(*size_bytes);
            }
        }
    }
    orphans.sort_by(|a, b| b.1.cmp(&a.1)); // oldest first

    println!("Session-tasks audit (cutoff: {cutoff_days} days):");
    println!("  total directories: {}", entries.len());
    println!("  live (transcript exists in any project): {live}");
    println!("  younger than cutoff (kept defensively): {younger}");
    println!("  not-a-session (non-UUID name, never auto-removed): {not_session}");
    println!(
        "  orphans (no transcript, older than cutoff): {} ({} total)",
        orphans.len(),
        format_size(total_orphan_bytes),
    );

    if not_session > 0 {
        println!("\nNot-a-session directories (skipped by --apply):");
        for (name, status) in entries {
            if matches!(status, TaskStatus::NotASession) {
                println!("  {name}");
            }
        }
    }

    if !orphans.is_empty() {
        println!("\nOrphans (oldest first):");
        for (name, age_days, size_bytes) in &orphans {
            println!("  {name}  age={age_days}d  size={}", format_size(*size_bytes));
        }
    }
}

/// Format bytes as a short human-readable size. Avoids pulling in a
/// dep — we only need three units (B/KB/MB) for the audit report.
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn apply_session_task_removals(
    root: &Path,
    entries: &[(String, TaskStatus)],
) -> Result<usize> {
    let mut removed = 0usize;
    for (name, status) in entries {
        if !matches!(status, TaskStatus::Orphan { .. }) {
            continue;
        }
        let path = root.join(name);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => removed += 1,
            Err(e) => {
                eprintln!("  warning: failed to remove {} — {e}", path.display());
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

    // ========================================================================
    // session-tasks tests
    // ========================================================================

    #[test]
    fn looks_like_uuid_accepts_valid_uuid() {
        assert!(looks_like_uuid("00266f13-d20a-4491-a776-c9175eafa758"));
        assert!(looks_like_uuid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"));
    }

    #[test]
    fn looks_like_uuid_rejects_junk() {
        assert!(!looks_like_uuid(".lock"));
        assert!(!looks_like_uuid("not-a-uuid-at-all"));
        // Wrong length:
        assert!(!looks_like_uuid("00266f13-d20a-4491-a776"));
        // Right length, wrong hyphen positions:
        assert!(!looks_like_uuid("00266f13d-20a-4491-a776-c9175eafa758"));
        // Right length + hyphens, but non-hex letter:
        assert!(!looks_like_uuid("zzzzzzzz-d20a-4491-a776-c9175eafa758"));
    }

    fn write_jsonl(projects_root: &Path, project: &str, session_id: &str) {
        let pdir = projects_root.join(project);
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join(format!("{session_id}.jsonl")), "{}").unwrap();
    }

    fn write_task_dir(tasks_root: &Path, session_id: &str) {
        let dir = tasks_root.join(session_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("1.json"), r#"{"id":"1","status":"completed"}"#).unwrap();
    }

    #[test]
    fn collect_live_session_ids_finds_jsonl_across_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let projects_root = tmp.path().join("projects");
        write_jsonl(&projects_root, "C--proj-one", "11111111-1111-4111-8111-111111111111");
        write_jsonl(&projects_root, "C--proj-two", "22222222-2222-4222-8222-222222222222");
        // Stray non-jsonl file — must be ignored.
        std::fs::write(projects_root.join("C--proj-one").join("notes.txt"), "x").unwrap();

        let live = collect_live_session_ids(&projects_root);
        assert!(live.contains("11111111-1111-4111-8111-111111111111"));
        assert!(live.contains("22222222-2222-4222-8222-222222222222"));
        assert_eq!(live.len(), 2, "must not pick up the .txt");
    }

    #[test]
    fn classify_live_session_when_jsonl_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_root = tmp.path().join("tasks");
        let session_id = "33333333-3333-4333-8333-333333333333";
        write_task_dir(&tasks_root, session_id);

        let mut live = std::collections::HashSet::new();
        live.insert(session_id.to_string());

        let now = std::time::SystemTime::now();
        let status = classify_session_task(
            &tasks_root.join(session_id),
            session_id,
            &live,
            &now,
            30 * 86_400,
        );
        assert!(matches!(status, TaskStatus::Live { .. }));
    }

    #[test]
    fn classify_orphan_when_no_jsonl_and_old() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_root = tmp.path().join("tasks");
        let session_id = "44444444-4444-4444-8444-444444444444";
        write_task_dir(&tasks_root, session_id);

        let live = std::collections::HashSet::new();
        // Far future "now" so the freshly-created dir reads as old.
        let now = std::time::SystemTime::now() + std::time::Duration::from_secs(365 * 86_400);
        let status = classify_session_task(
            &tasks_root.join(session_id),
            session_id,
            &live,
            &now,
            30 * 86_400,
        );
        assert!(matches!(status, TaskStatus::Orphan { .. }));
    }

    #[test]
    fn classify_younger_than_cutoff_when_no_jsonl_but_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_root = tmp.path().join("tasks");
        let session_id = "55555555-5555-4555-8555-555555555555";
        write_task_dir(&tasks_root, session_id);

        let live = std::collections::HashSet::new();
        let now = std::time::SystemTime::now();
        let status = classify_session_task(
            &tasks_root.join(session_id),
            session_id,
            &live,
            &now,
            30 * 86_400,
        );
        assert!(matches!(status, TaskStatus::YoungerThanCutoff { .. }));
    }

    #[test]
    fn classify_not_a_session_for_non_uuid_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".lock");
        std::fs::create_dir_all(&dir).unwrap();
        let live = std::collections::HashSet::new();
        let now = std::time::SystemTime::now();
        let status = classify_session_task(&dir, ".lock", &live, &now, 30 * 86_400);
        assert!(matches!(status, TaskStatus::NotASession));
    }

    #[test]
    fn apply_session_task_removals_keeps_live_younger_and_non_session() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_root = tmp.path().join("tasks");
        let live_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let orphan_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let young_id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
        let not_session = ".lock";
        write_task_dir(&tasks_root, live_id);
        write_task_dir(&tasks_root, orphan_id);
        write_task_dir(&tasks_root, young_id);
        std::fs::create_dir_all(tasks_root.join(not_session)).unwrap();

        let mut live = std::collections::HashSet::new();
        live.insert(live_id.to_string());
        // For the orphan, pretend "now" is a year in the future so it's stale.
        let now_for_orphan = std::time::SystemTime::now() + std::time::Duration::from_secs(365 * 86_400);
        let now_for_young = std::time::SystemTime::now();

        let entries = vec![
            (
                live_id.to_string(),
                classify_session_task(
                    &tasks_root.join(live_id),
                    live_id,
                    &live,
                    &now_for_young,
                    30 * 86_400,
                ),
            ),
            (
                orphan_id.to_string(),
                classify_session_task(
                    &tasks_root.join(orphan_id),
                    orphan_id,
                    &live,
                    &now_for_orphan,
                    30 * 86_400,
                ),
            ),
            (
                young_id.to_string(),
                classify_session_task(
                    &tasks_root.join(young_id),
                    young_id,
                    &live,
                    &now_for_young,
                    30 * 86_400,
                ),
            ),
            (
                not_session.to_string(),
                classify_session_task(
                    &tasks_root.join(not_session),
                    not_session,
                    &live,
                    &now_for_young,
                    30 * 86_400,
                ),
            ),
        ];

        let removed = apply_session_task_removals(&tasks_root, &entries).unwrap();
        assert_eq!(removed, 1, "must remove only the orphan");
        assert!(tasks_root.join(live_id).exists());
        assert!(!tasks_root.join(orphan_id).exists());
        assert!(tasks_root.join(young_id).exists());
        assert!(tasks_root.join(not_session).exists());
    }

    #[test]
    fn format_size_renders_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
    }
}
