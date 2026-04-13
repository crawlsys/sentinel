//! Skill Telemetry
//!
//! Aggregates skill usage metrics. On every Stop event, appends a
//! telemetry entry to `~/.claude/metrics/skill-telemetry.jsonl`.
//! Every 10 executions, regenerates a summary file with aggregates.

use sentinel_domain::events::{HookInput, HookOutput};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{FileSystemPort, HookContext};

/// Resolve `~/.claude/metrics` directory, creating it if needed.
fn metrics_dir(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = home.join(".claude").join("metrics");
    fs.create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Detect project language from well-known manifest files in `dir`.
fn detect_language(fs: &dyn FileSystemPort, dir: &Path) -> &'static str {
    if fs.exists(&dir.join("package.json")) {
        return "typescript";
    }
    if fs.exists(&dir.join("Cargo.toml")) {
        return "rust";
    }
    if fs.exists(&dir.join("pubspec.yaml")) {
        return "dart";
    }
    if fs.exists(&dir.join("pyproject.toml")) || fs.exists(&dir.join("setup.py")) {
        return "python";
    }
    if fs.exists(&dir.join("go.mod")) {
        return "go";
    }
    "unknown"
}

/// Directory for telemetry state files — must match skill_router::telemetry_dir().
fn telemetry_state_dir(fs: &dyn FileSystemPort) -> PathBuf {
    fs.home_dir()
        .map(|h| h.join(".claude").join("sentinel").join("telemetry"))
        .unwrap_or_else(|| std::env::temp_dir())
}

/// Read the current skill from the telemetry state file written by skill-router.
fn read_current_skill(fs: &dyn FileSystemPort) -> String {
    let path = telemetry_state_dir(fs).join("claude-current-skill");
    fs.read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".to_string())
}

/// Read the run ID from the telemetry state file written by skill-router.
fn read_run_id(fs: &dyn FileSystemPort) -> Option<String> {
    let path = telemetry_state_dir(fs).join("claude-skill-run-id");
    fs.read_to_string(&path)
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
}

/// Read the start time from the telemetry state file written by skill-router.
fn read_start_time(fs: &dyn FileSystemPort) -> Option<i64> {
    let path = telemetry_state_dir(fs).join("claude-skill-start-time");
    fs.read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Generate the aggregate summary from the full telemetry file.
fn regenerate_summary(fs: &dyn FileSystemPort, telemetry_path: &Path, summary_path: &Path) {
    let content = match fs.read_to_string(telemetry_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let entries: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    let total = entries.len();
    if total == 0 {
        return;
    }

    let mut skill_counts: HashMap<String, usize> = HashMap::new();
    let mut lang_counts: HashMap<String, usize> = HashMap::new();
    let mut skill_durations: HashMap<String, Vec<i64>> = HashMap::new();

    for entry in &entries {
        let skill = entry
            .get("skill")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let lang = entry
            .get("language")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let duration = entry
            .get("duration_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        *skill_counts.entry(skill.clone()).or_insert(0) += 1;
        *lang_counts.entry(lang).or_insert(0) += 1;
        skill_durations.entry(skill).or_default().push(duration);
    }

    let mut skills_by_usage: Vec<serde_json::Value> = skill_counts
        .iter()
        .map(|(skill, count)| serde_json::json!({ "skill": skill, "count": count }))
        .collect();
    skills_by_usage.sort_by(|a, b| {
        b.get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .cmp(&a.get("count").and_then(|v| v.as_u64()).unwrap_or(0))
    });

    let mut langs_by_usage: Vec<serde_json::Value> = lang_counts
        .iter()
        .map(|(lang, count)| serde_json::json!({ "language": lang, "count": count }))
        .collect();
    langs_by_usage.sort_by(|a, b| {
        b.get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .cmp(&a.get("count").and_then(|v| v.as_u64()).unwrap_or(0))
    });

    let avg_durations: Vec<serde_json::Value> = skill_durations
        .iter()
        .map(|(skill, durations)| {
            let sum: i64 = durations.iter().sum();
            let avg = if durations.is_empty() {
                0
            } else {
                sum / durations.len() as i64
            };
            serde_json::json!({ "skill": skill, "avg_ms": avg })
        })
        .collect();

    let summary = serde_json::json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "total_executions": total,
        "skills_by_usage": skills_by_usage,
        "languages_by_usage": langs_by_usage,
        "avg_duration_by_skill": avg_durations,
    });

    let _ = fs.write(
        summary_path,
        serde_json::to_string_pretty(&summary).unwrap_or_default().as_bytes(),
    );
}

/// Process the skill-telemetry hook event (Stop).
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let current_skill = read_current_skill(ctx.fs);

    let metrics = match metrics_dir(ctx.fs) {
        Some(d) => d,
        None => return HookOutput::allow(),
    };

    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd_str = input.cwd.as_deref().unwrap_or(".");
    let cwd = Path::new(cwd_str);

    let run_id = read_run_id(ctx.fs).unwrap_or_default();
    let start_time = read_start_time(ctx.fs);
    let duration_ms = start_time
        .map(|st| chrono::Utc::now().timestamp_millis() - st)
        .unwrap_or(0);

    let language = detect_language(ctx.fs, cwd);
    let timestamp = chrono::Utc::now().to_rfc3339();

    // Append telemetry entry
    let telemetry_file = metrics.join("skill-telemetry.jsonl");
    let entry = serde_json::json!({
        "event": "skill_execution",
        "skill": current_skill,
        "run_id": run_id,
        "session_id": session_id,
        "language": language,
        "duration_ms": duration_ms,
        "cwd": cwd_str,
        "ts": timestamp,
    });

    let entry_line = format!("{}\n", serde_json::to_string(&entry).unwrap_or_default());
    let _ = ctx.fs.append(&telemetry_file, entry_line.as_bytes());

    // Append completion entry to routing.jsonl if we have a run_id
    if !run_id.is_empty() {
        let routing_file = metrics.join("routing.jsonl");
        let completion = serde_json::json!({
            "run_id": run_id,
            "session_id": session_id,
            "event": "skill_complete",
            "status": "success",
            "ts": timestamp,
        });

        let completion_line = format!(
            "{}\n",
            serde_json::to_string(&completion).unwrap_or_default()
        );
        let _ = ctx.fs.append(&routing_file, completion_line.as_bytes());

        // Clean up one-time run ID file (write empty to clear)
        let _ = ctx
            .fs
            .write(&telemetry_state_dir(ctx.fs).join("claude-skill-run-id"), b"");
    }

    // Regenerate summary every 10 executions
    let line_count = ctx
        .fs
        .read_to_string(&telemetry_file)
        .map(|c| c.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0);

    if line_count > 0 && line_count % 10 == 0 {
        let summary_file = metrics.join("telemetry-summary.json");
        regenerate_summary(ctx.fs, &telemetry_file, &summary_file);
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdMap;

    /// Test FS that tracks exists() calls via a set of paths.
    struct TestFs {
        existing: std::collections::HashSet<PathBuf>,
        files: StdMap<PathBuf, String>,
        written: std::sync::Mutex<StdMap<PathBuf, Vec<u8>>>,
    }
    impl TestFs {
        fn with_existing(paths: &[&str]) -> Self {
            Self {
                existing: paths.iter().map(PathBuf::from).collect(),
                files: StdMap::new(),
                written: std::sync::Mutex::new(StdMap::new()),
            }
        }
        fn with_files(files: Vec<(&str, &str)>) -> Self {
            Self {
                existing: files.iter().map(|(k, _)| PathBuf::from(k)).collect(),
                files: files.into_iter().map(|(k, v)| (PathBuf::from(k), v.to_string())).collect(),
                written: std::sync::Mutex::new(StdMap::new()),
            }
        }
    }
    impl FileSystemPort for TestFs {
        fn home_dir(&self) -> Option<PathBuf> { Some(PathBuf::from("/mock/home")) }
        fn read_to_string(&self, p: &Path) -> anyhow::Result<String> {
            self.files.get(p).cloned().ok_or_else(|| anyhow::anyhow!("not found"))
        }
        fn write(&self, p: &Path, c: &[u8]) -> anyhow::Result<()> {
            self.written.lock().unwrap().insert(p.to_path_buf(), c.to_vec()); Ok(())
        }
        fn create_dir_all(&self, _: &Path) -> anyhow::Result<()> { Ok(()) }
        fn read_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> { Ok(vec![]) }
        fn exists(&self, p: &Path) -> bool { self.existing.contains(p) }
        fn is_dir(&self, _: &Path) -> bool { false }
        fn metadata(&self, _: &Path) -> anyhow::Result<std::fs::Metadata> { anyhow::bail!("no") }
        fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
    }

    #[test]
    fn test_detect_language_typescript() {
        let fs = TestFs::with_existing(&["/proj/package.json"]);
        assert_eq!(detect_language(&fs, Path::new("/proj")), "typescript");
    }

    #[test]
    fn test_detect_language_rust() {
        let fs = TestFs::with_existing(&["/proj/Cargo.toml"]);
        assert_eq!(detect_language(&fs, Path::new("/proj")), "rust");
    }

    #[test]
    fn test_detect_language_python() {
        let fs = TestFs::with_existing(&["/proj/pyproject.toml"]);
        assert_eq!(detect_language(&fs, Path::new("/proj")), "python");
    }

    #[test]
    fn test_detect_language_go() {
        let fs = TestFs::with_existing(&["/proj/go.mod"]);
        assert_eq!(detect_language(&fs, Path::new("/proj")), "go");
    }

    #[test]
    fn test_detect_language_unknown() {
        let fs = TestFs::with_existing(&[]);
        assert_eq!(detect_language(&fs, Path::new("/proj")), "unknown");
    }

    #[test]
    fn test_process_no_metrics_dir_is_ok() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_regenerate_summary_empty() {
        let fs = TestFs::with_files(vec![("/tel.jsonl", "")]);
        regenerate_summary(&fs, Path::new("/tel.jsonl"), Path::new("/summary.json"));
        // Empty file → no summary written
        assert!(!fs.written.lock().unwrap().contains_key(Path::new("/summary.json")));
    }

    #[test]
    fn test_regenerate_summary_with_entries() {
        let entries = vec![
            serde_json::json!({"skill":"linear","language":"typescript","duration_ms":1000}),
            serde_json::json!({"skill":"linear","language":"typescript","duration_ms":2000}),
            serde_json::json!({"skill":"review","language":"rust","duration_ms":500}),
        ];
        let content: String = entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let fs = TestFs::with_files(vec![("/tel.jsonl", &content)]);
        regenerate_summary(&fs, Path::new("/tel.jsonl"), Path::new("/summary.json"));

        let written = fs.written.lock().unwrap();
        let bytes = written.get(Path::new("/summary.json")).expect("should be written");
        let result: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(result["total_executions"], 3);
    }
}
