//! Skill Telemetry
//!
//! Aggregates skill usage metrics. On every Stop event, appends a
//! telemetry entry to `~/.claude/metrics/skill-telemetry.jsonl`.
//! Every 10 executions, regenerates a summary file with aggregates.

use sentinel_domain::events::{HookInput, HookOutput};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Resolve `~/.claude/metrics` directory, creating it if needed.
fn metrics_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let dir = home.join(".claude").join("metrics");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Detect project language from well-known manifest files in `dir`.
fn detect_language(dir: &Path) -> &'static str {
    if dir.join("package.json").exists() {
        return "typescript";
    }
    if dir.join("Cargo.toml").exists() {
        return "rust";
    }
    if dir.join("pubspec.yaml").exists() {
        return "dart";
    }
    if dir.join("pyproject.toml").exists() || dir.join("setup.py").exists() {
        return "python";
    }
    if dir.join("go.mod").exists() {
        return "go";
    }
    "unknown"
}

/// Directory for telemetry state files — must match skill_router::telemetry_dir().
/// Falls back to temp_dir() if home dir unavailable (shouldn't happen in practice).
fn telemetry_state_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".claude").join("sentinel").join("telemetry"))
        .unwrap_or_else(|| std::env::temp_dir())
}

/// Read the current skill from the telemetry state file written by skill-router.
fn read_current_skill() -> String {
    let path = telemetry_state_dir().join("claude-current-skill");
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".to_string())
}

/// Read the run ID from the telemetry state file written by skill-router.
fn read_run_id() -> Option<String> {
    let path = telemetry_state_dir().join("claude-skill-run-id");
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
}

/// Read the start time from the telemetry state file written by skill-router.
fn read_start_time() -> Option<i64> {
    let path = telemetry_state_dir().join("claude-skill-start-time");
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Generate the aggregate summary from the full telemetry file.
fn regenerate_summary(telemetry_path: &Path, summary_path: &Path) {
    let content = match fs::read_to_string(telemetry_path) {
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
        .map(|(skill, count)| {
            serde_json::json!({ "skill": skill, "count": count })
        })
        .collect();
    skills_by_usage.sort_by(|a, b| {
        b.get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .cmp(&a.get("count").and_then(|v| v.as_u64()).unwrap_or(0))
    });

    let mut langs_by_usage: Vec<serde_json::Value> = lang_counts
        .iter()
        .map(|(lang, count)| {
            serde_json::json!({ "language": lang, "count": count })
        })
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

    let _ = fs::write(
        summary_path,
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    );
}

/// Process the skill-telemetry hook event (Stop).
pub fn process(input: &HookInput) -> HookOutput {
    let current_skill = read_current_skill();

    let metrics = match metrics_dir() {
        Some(d) => d,
        None => return HookOutput::allow(),
    };

    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd_str = input.cwd.as_deref().unwrap_or(".");
    let cwd = Path::new(cwd_str);

    let run_id = read_run_id().unwrap_or_default();
    let start_time = read_start_time();
    let duration_ms = start_time
        .map(|st| {
            chrono::Utc::now().timestamp_millis() - st
        })
        .unwrap_or(0);

    let language = detect_language(cwd);
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

    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&telemetry_file)
    {
        let _ = writeln!(file, "{}", serde_json::to_string(&entry).unwrap_or_default());
    }

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

        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&routing_file)
        {
            let _ = writeln!(
                file,
                "{}",
                serde_json::to_string(&completion).unwrap_or_default()
            );
        }

        // Clean up one-time run ID file
        let _ = fs::remove_file(telemetry_state_dir().join("claude-skill-run-id"));
    }

    // Regenerate summary every 10 executions
    let line_count = fs::read_to_string(&telemetry_file)
        .map(|c| c.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0);

    if line_count > 0 && line_count % 10 == 0 {
        let summary_file = metrics.join("telemetry-summary.json");
        regenerate_summary(&telemetry_file, &summary_file);
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_language_typescript() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect_language(dir.path()), "typescript");
    }

    #[test]
    fn test_detect_language_rust() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(detect_language(dir.path()), "rust");
    }

    #[test]
    fn test_detect_language_python() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(detect_language(dir.path()), "python");
    }

    #[test]
    fn test_detect_language_go() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("go.mod"), "").unwrap();
        assert_eq!(detect_language(dir.path()), "go");
    }

    #[test]
    fn test_detect_language_unknown() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_language(dir.path()), "unknown");
    }

    #[test]
    fn test_process_no_metrics_dir_is_ok() {
        // Even if metrics dir creation fails, process should return allow
        let input = HookInput::default();
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_regenerate_summary_empty() {
        let dir = tempfile::tempdir().unwrap();
        let telemetry = dir.path().join("telemetry.jsonl");
        let summary = dir.path().join("summary.json");
        fs::write(&telemetry, "").unwrap();
        regenerate_summary(&telemetry, &summary);
        // Summary should not be created for empty file
        assert!(!summary.exists());
    }

    #[test]
    fn test_regenerate_summary_with_entries() {
        let dir = tempfile::tempdir().unwrap();
        let telemetry = dir.path().join("telemetry.jsonl");
        let summary = dir.path().join("summary.json");

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
        fs::write(&telemetry, content).unwrap();

        regenerate_summary(&telemetry, &summary);

        let result: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&summary).unwrap()).unwrap();
        assert_eq!(result["total_executions"], 3);
    }
}
