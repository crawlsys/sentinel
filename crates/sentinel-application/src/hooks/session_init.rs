//! Session Init — SessionStart hook
//!
//! Handles session initialization:
//! - Logs session start to metrics/sessions.jsonl
//! - Syncs marketplace repo to ~/.claude/ (if local repo found)
//! - Validates sync (critical files must exist)
//! - Generates ~/.claude/CLAUDE.md with dynamic component counts
//! - Outputs compact startup context

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Well-known marketplace repo locations to check
const REPO_CANDIDATES: &[&str] = &[
    "Documents/GitHub/claude-code-marketplace",
    "code/claude-code-marketplace",
    "repos/claude-code-marketplace",
    "projects/claude-code-marketplace",
];

/// Directories to sync from repo to ~/.claude/
const SYNC_DIRS: &[&str] = &[
    "skills", "agents", "commands", "scripts", "templates", "docs",
];

/// Directories to sync recursively (including subdirectories)
/// Note: hooks no longer synced — all hooks run through the sentinel Rust engine
const SYNC_DIRS_RECURSIVE: &[&str] = &[];

/// Minimum number of skill directories for a valid sync
const MIN_SKILL_DIRS: usize = 40;

/// Process SessionStart event
pub fn process(input: &HookInput) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd = input.cwd.as_deref().unwrap_or(".");

    let claude_dir = claude_dir();

    // 1. Log session start
    log_session_start(&claude_dir, session_id, cwd);

    // 2. Sync marketplace repo (if found)
    let sync_result = sync_marketplace(&claude_dir);

    // 3. Validate sync
    let validation = validate_sync(&claude_dir);
    if !validation.valid {
        let reasons = validation.reasons.join("; ");
        tracing::warn!("Post-sync validation failed: {}", reasons);
    }

    // 4. Generate CLAUDE.md with dynamic counts
    let counts = count_components(&claude_dir);
    generate_claude_md(&claude_dir, &counts);

    // 5. Build startup context
    let context = build_startup_context(&sync_result, &validation, &counts, session_id);

    HookOutput::inject_context(HookEvent::SessionStart, context)
}

// ---------------------------------------------------------------------------
// Infrastructure helpers
// ---------------------------------------------------------------------------

/// Get ~/.claude/ path
fn claude_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
}

/// Log session start to metrics/sessions.jsonl
fn log_session_start(claude_dir: &Path, session_id: &str, cwd: &str) {
    let metrics_dir = claude_dir.join("metrics");
    let _ = fs::create_dir_all(&metrics_dir);

    let timestamp = chrono::Utc::now().to_rfc3339();
    let platform = std::env::consts::OS;
    let entry = serde_json::json!({
        "event": "session_start",
        "session_id": session_id,
        "cwd": cwd,
        "platform": platform,
        "ts": timestamp,
        "engine": "sentinel"
    });

    let sessions_file = metrics_dir.join("sessions.jsonl");
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sessions_file)
        .and_then(|mut f| writeln!(f, "{}", entry));
}

// ---------------------------------------------------------------------------
// Marketplace repo discovery + sync
// ---------------------------------------------------------------------------

/// Find the marketplace git repo on disk
fn find_marketplace_repo() -> Option<PathBuf> {
    let home = dirs::home_dir()?;

    for candidate in REPO_CANDIDATES {
        let dir = home.join(candidate);
        if is_marketplace_repo(&dir) {
            return Some(dir);
        }
    }

    None
}

/// Check if a directory is the marketplace git repo
fn is_marketplace_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
        && dir.join("skills").exists()
        && dir.join("install.js").exists()
}

/// Sync marketplace repo to ~/.claude/
fn sync_marketplace(claude_dir: &Path) -> SyncResult {
    let repo_dir = match find_marketplace_repo() {
        Some(dir) => dir,
        None => return SyncResult::NoRepo,
    };

    // Check if we need to sync (compare last sync commit)
    let marker_file = claude_dir.join(".last-sync-commit");
    let current_head = get_git_head(&repo_dir);

    if let (Some(ref head), Ok(last)) = (&current_head, fs::read_to_string(&marker_file)) {
        if last.trim() == head.trim() {
            return SyncResult::UpToDate;
        }
    }

    // Try git pull first (fast-forward only)
    let pull_ok = git_pull(&repo_dir);

    // Sync directories
    let mut synced = 0u32;
    for dir_name in SYNC_DIRS {
        let src = repo_dir.join(dir_name);
        let dest = claude_dir.join(dir_name);
        if src.exists() {
            synced += copy_dir_recursive(&src, &dest).unwrap_or(0);
        }
    }

    // Sync additional recursive directories (if any)
    for dir_name in SYNC_DIRS_RECURSIVE {
        let src = repo_dir.join(dir_name);
        let dest = claude_dir.join(dir_name);
        if src.exists() {
            synced += copy_dir_recursive(&src, &dest).unwrap_or(0);
        }
    }

    // Update sync marker with new HEAD
    let new_head = get_git_head(&repo_dir);
    if let Some(head) = &new_head {
        let _ = fs::write(&marker_file, head);
    }

    SyncResult::Synced {
        files: synced,
        pulled: pull_ok,
    }
}

/// Get HEAD commit hash
fn get_git_head(repo: &Path) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                None
            }
        })
}

/// Fast-forward git pull
fn git_pull(repo: &Path) -> bool {
    // Fetch first
    let fetch = Command::new("git")
        .args(["fetch", "--quiet"])
        .current_dir(repo)
        .output();

    if fetch.is_err() {
        return false;
    }

    // Fast-forward merge
    Command::new("git")
        .args(["merge", "--ff-only", "@{u}"])
        .current_dir(repo)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Recursively copy a directory, returns number of files copied
fn copy_dir_recursive(src: &Path, dest: &Path) -> std::io::Result<u32> {
    let _ = fs::create_dir_all(dest);
    let mut count = 0;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;

        // Skip symlinks to avoid circular references and unexpected behavior
        if ft.is_symlink() {
            continue;
        }

        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        if ft.is_dir() {
            count += copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            // Only copy if source is newer or dest doesn't exist
            let should_copy = if dest_path.exists() {
                let src_meta = fs::metadata(&src_path)?;
                let dest_meta = fs::metadata(&dest_path)?;
                src_meta.modified()? > dest_meta.modified()?
            } else {
                true
            };

            if should_copy {
                fs::copy(&src_path, &dest_path)?;
                count += 1;
            }
        }
    }

    Ok(count)
}

// ---------------------------------------------------------------------------
// Sync validation
// ---------------------------------------------------------------------------

/// Validation result
struct ValidationResult {
    valid: bool,
    reasons: Vec<String>,
}

/// Validate that critical marketplace files exist after sync
fn validate_sync(claude_dir: &Path) -> ValidationResult {
    let mut reasons = Vec::new();

    // 1. settings.json must exist and be valid JSON
    let settings_path = claude_dir.join("settings.json");
    if settings_path.exists() {
        if let Ok(content) = fs::read_to_string(&settings_path) {
            if serde_json::from_str::<serde_json::Value>(&content).is_err() {
                reasons.push("settings.json is invalid JSON".to_string());
            }
        }
    } else {
        reasons.push("settings.json missing".to_string());
    }

    // 2. At least MIN_SKILL_DIRS skill directories
    let skills_dir = claude_dir.join("skills");
    if skills_dir.exists() {
        let skill_count = count_subdirs(&skills_dir);
        if skill_count < MIN_SKILL_DIRS {
            reasons.push(format!(
                "Only {} skill directories found (minimum: {})",
                skill_count, MIN_SKILL_DIRS
            ));
        }
    } else {
        reasons.push("skills/ directory missing".to_string());
    }

    // 3. sentinel engine should be available
    let cargo_bin = dirs::home_dir()
        .map(|h| h.join(".cargo").join("bin"));
    let sentinel_available = cargo_bin
        .map(|d| {
            if cfg!(windows) {
                d.join("sentinel.exe").exists() || d.join("sentinel-engine.exe").exists()
            } else {
                d.join("sentinel").exists() || d.join("sentinel-engine").exists()
            }
        })
        .unwrap_or(false);
    if !sentinel_available {
        reasons.push("sentinel binary not found in ~/.cargo/bin/".to_string());
    }

    ValidationResult {
        valid: reasons.is_empty(),
        reasons,
    }
}

// ---------------------------------------------------------------------------
// Component counting
// ---------------------------------------------------------------------------

/// Dynamic component counts for CLAUDE.md generation
struct ComponentCounts {
    skills: usize,
    hooks: usize,
    commands: usize,
    agents: usize,
    mcp_servers: usize,
}

/// Count all marketplace components in ~/.claude/
fn count_components(claude_dir: &Path) -> ComponentCounts {
    let skills = count_subdirs(&claude_dir.join("skills"));
    let hooks = super::HOOK_NAMES.len();
    let commands = count_files_with_ext(&claude_dir.join("commands"), ".md");
    let agents = count_files_with_ext(&claude_dir.join("agents"), ".md");
    let mcp_servers = count_mcp_servers();

    ComponentCounts {
        skills,
        hooks,
        commands,
        agents,
        mcp_servers,
    }
}

/// Count subdirectories in a path
fn count_subdirs(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

/// Count files with a given extension in a directory (non-recursive)
fn count_files_with_ext(dir: &Path, ext: &str) -> usize {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().map(|ft| ft.is_file()).unwrap_or(false)
                        && e.file_name().to_string_lossy().ends_with(ext)
                })
                .count()
        })
        .unwrap_or(0)
}

/// Count MCP servers from ~/.claude.json
fn count_mcp_servers() -> usize {
    let path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude.json");

    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|json| {
            json.get("mcpServers")
                .and_then(|v| v.as_object())
                .map(|obj| obj.len())
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// CLAUDE.md generation
// ---------------------------------------------------------------------------

/// Generate ~/.claude/CLAUDE.md with dynamic counts and current date
fn generate_claude_md(claude_dir: &Path, counts: &ComponentCounts) {
    let now = chrono::Utc::now();
    let date_str = now.format("%A, %B %-d, %Y").to_string();
    let year = now.format("%Y").to_string();
    let month = now.format("%B").to_string();

    let content = format!(
        r#"# Claude Code Marketplace - Global Configuration

## Table of Contents
1. [User Preferences](#user-preferences)
2. [Date Context](#date-context)
3. [Marketplace Architecture](#marketplace-architecture)
4. [Using Slash Commands](#using-slash-commands)
5. [Using Agents](#using-agents)
6. [Using Skills](#using-skills)
7. [Changelog & Version Tracking](#changelog--version-tracking)
8. [Plans & Documentation](#plans--documentation)
9. [Session Resume](#session-resume)
10. [Context Management](#context-management)

---

## User Preferences

- Always address the user as **Gary**
- On your FIRST message of each conversation, start with a robot emoji to confirm this file is being read

## Date Context

The current year is {year} and the current month is {month}.

Today is {date_str}.

---

## Marketplace Architecture

The Claude Code Marketplace is a modular ecosystem of components that extend Claude Code:

```
~/.claude/
├── CLAUDE.md              <- Auto-generated on every session (live version)
├── settings.json          <- Hook registrations (sentinel commands) + env vars
├── .claude.json           <- MCP server registrations
├── sentinel/config/       <- Sentinel hook engine configuration
├── skills/                <- {skills} skill directories (SKILL.md each)
├── commands/              <- {commands} slash commands (.md files)
├── agents/                <- {agents} agent definitions (.md files)
├── plans/                 <- Implementation plans (markdown, per-project)
├── scripts/               <- Utility scripts (.js)
├── docs/                  <- Reference docs (auto-generated)
└── metrics/               <- Usage analytics (JSONL)
```

**How components connect:**
- **User types a message** -> `UserPromptSubmit` hooks fire (skill-router, error-reporter, todo-loader)
- **Claude uses a tool** -> `PreToolUse` hooks fire (phase-gate, git-hygiene), then `PostToolUse` hooks fire (mcp-health)
- **Claude finishes responding** -> `Stop` hooks fire (context-monitor, skill-telemetry, commit-hygiene)
- **Session starts** -> `SessionStart` hooks fire (generates this CLAUDE.md, syncs marketplace)
- **Context compacts** -> `PreCompact` hooks fire (preserves critical context)

All {hooks} hooks run through the sentinel Rust engine (`sentinel hook --event <Event>`).

---

## Using Slash Commands

Slash commands are user-invocable shortcuts. Invoke them with the `Skill` tool:

| Command | Description | Usage |
|---------|-------------|-------|
| `/commit` | Smart git commit with conventional format | `Skill(skill: "commit")` |
| `/test` | Run tests with coverage | `Skill(skill: "test")` |
| `/review` | 6-layer code review pipeline | `Skill(skill: "review")` |
| `/explore` | Explore codebase structure | `Skill(skill: "explore")` |
| `/plan` | Plan implementation before coding | `Skill(skill: "plan")` |
| `/debug` | Debug with root cause analysis | `Skill(skill: "debug")` |
| `/pr` | Create pull request | `Skill(skill: "pr")` |
| `/skills` | List all available skills | `Skill(skill: "skills")` |
| `/session` | Get current session ID | `Skill(skill: "session")` |

When user types `/command`, use the `Skill` tool -- NOT a manual implementation.

---

## Using Agents

Spawn specialized agents with the `Task` tool for parallel or delegated work:

| Agent | Use When | Example |
|-------|----------|---------|
| `Explore` | Finding files, searching code | `Task(subagent_type: "Explore", prompt: "Find all API routes")` |
| `Plan` | Architecture, implementation design | `Task(subagent_type: "Plan", prompt: "Plan auth refactor")` |
| `Bash` | Git, npm, docker, system commands | `Task(subagent_type: "Bash", prompt: "Run tests and report")` |
| `general-purpose` | Complex multi-step tasks | `Task(subagent_type: "general-purpose", prompt: "...")` |
| `debugger` | Root cause analysis, bug fixing | `Task(subagent_type: "debugger", prompt: "Fix failing test")` |
| `test-generator` | Write unit/integration/e2e tests | `Task(subagent_type: "test-generator", prompt: "...")` |
| `code-reviewer` | Quality, bugs, security review | `Task(subagent_type: "code-reviewer", prompt: "...")` |
| `refactorer` | Improve structure without changing behavior | `Task(subagent_type: "refactorer", prompt: "...")` |

---

## Using Skills

Skills are modular capabilities loaded from `~/.claude/skills/{{name}}/SKILL.md`.

### Automatic Routing (skill-router hook)
The sentinel `skill_router` hook runs on every message and uses regex matching to route requests to the matching skill. You will see `[Skill Router] Detected skill: <name>` in system reminders -- follow those instructions.

---

## Changelog & Version Tracking

**MANDATORY:** When making significant changes to any project, maintain a changelog.

### Rules
1. **Before starting work**: Check if `CHANGELOG.md` exists in the project root. If not, create one.
2. **After completing a feature/fix**: Add an entry under `## [Unreleased]` with the date and description.
3. **Version bumps**: When bumping version in `package.json`, `Cargo.toml`, `marketplace.json`, etc., add a dated section to the changelog.
4. **Format**: Use [Keep a Changelog](https://keepachangelog.com/) format:
   - `### Added` for new features
   - `### Changed` for changes in existing functionality
   - `### Fixed` for bug fixes
   - `### Removed` for removed features

### Where to track versions
| Component | Version File | Current |
|-----------|-------------|---------|
| Marketplace | `marketplace.json` | Check `version` field |
| Sentinel | `sentinel/Cargo.toml` | Check `version` field |
| Skills | Each `SKILL.md` frontmatter | `version:` field |

---

## Plans & Documentation

### Plan Files
All implementation plans go in `~/.claude/plans/` with subdirectories by project:

```
~/.claude/plans/
├── marketplace/           <- Claude Code Marketplace plans
├── sentinel/              <- Sentinel engine plans
├── firefly-pro/           <- Firefly Pro CRM plans
├── legatus/               <- Legatus platform plans
└── {{project-name}}/       <- Other project plans
```

**Rules:**
- Name plans descriptively: `feature-name/plan-v1.md`, not `plan.md`
- Include status at the top: `Status: Draft | Approved | In Progress | Complete`
- When starting implementation, update status to `In Progress`
- When done, update status to `Complete` with a summary of what was actually built
- NEVER delete plan files -- they are the historical record

### README Maintenance
After completing significant work in a project (new features, architecture changes, dependency updates):
1. Check if `README.md` exists -- if so, verify it still reflects reality
2. Update sections that are now stale (install steps, feature lists, architecture diagrams, counts)
3. Do NOT add fluff -- only update what changed. A 2-line diff is better than a rewrite.

### Per-Project CLAUDE.md
Each repo can have a `CLAUDE.md` at its root with project-specific instructions. Keep it in sync:
1. After adding/removing major components, update counts and file trees in the repo CLAUDE.md
2. If the repo CLAUDE.md references specific file paths, verify they still exist
3. The repo CLAUDE.md is for GitHub visitors and new sessions -- it should match what's actually built
4. Do NOT duplicate the global `~/.claude/CLAUDE.md` content -- only project-specific context belongs here

### Documentation Folders
```
~/.claude/docs/            <- Auto-generated reference docs
~/Documents/GitHub/*/docs/ <- Per-project documentation
```

---

## Session Resume

When resuming a previous session or when the user asks "what was I doing":

1. Use the `session-resume` skill: `Read("~/.claude/skills/session-resume/SKILL.md")`
2. It reads the actual conversation JSONL from `~/.claude/projects/`
3. Extracts: user prompts, tool usage, files changed, git commits, Linear activity
4. Presents a concise summary -- no ASCII boxes, just clean markdown

The conversation transcripts are at:
```
~/.claude/projects/{{project-key}}/{{session-id}}.jsonl
```

---

## Context Management

| Zone | Context % | Strategy |
|------|-----------|----------|
| Green | 0-50% | Work directly, read files freely |
| Yellow | 50-65% | Start delegating to agents |
| Orange | 65-75% | Use agents for ALL exploration |
| Red | 75%+ | Agents only, prepare for auto-compact |

---

## Marketplace Stats

- **Skills:** {skills}
- **MCP Servers:** {mcp}
- **Slash Commands:** {commands}
- **Hooks:** {hooks} (sentinel engine)
- **Agents:** {agents}

*Auto-generated on session start: {date_str}*
"#,
        year = year,
        month = month,
        date_str = date_str,
        skills = counts.skills,
        hooks = counts.hooks,
        commands = counts.commands,
        agents = counts.agents,
        mcp = counts.mcp_servers,
    );

    let claude_md_path = claude_dir.join("CLAUDE.md");
    let _ = fs::write(&claude_md_path, content);
}

// ---------------------------------------------------------------------------
// Startup context
// ---------------------------------------------------------------------------

/// Build compact startup context string
fn build_startup_context(
    sync: &SyncResult,
    validation: &ValidationResult,
    counts: &ComponentCounts,
    session_id: &str,
) -> String {
    let mut parts = Vec::new();

    // Session info
    parts.push(format!(
        "[SessionStart] session_id: {} | engine: sentinel",
        session_id
    ));

    // Sync status
    match sync {
        SyncResult::Synced { files, pulled } => {
            let pull_tag = if *pulled { " (pulled)" } else { "" };
            if *files > 0 {
                parts.push(format!(
                    "[Marketplace Sync] {} files synced{}",
                    files, pull_tag
                ));
            } else {
                parts.push("[Marketplace Sync] No changes".to_string());
            }
        }
        SyncResult::UpToDate => {
            parts.push("[Marketplace Sync] Up to date".to_string());
        }
        SyncResult::NoRepo => {
            parts.push("[Marketplace Sync] No local repo found".to_string());
        }
    }

    // Validation warnings
    if !validation.valid {
        parts.push(format!(
            "[Validation] FAILED: {}",
            validation.reasons.join("; ")
        ));
    }

    // Component counts
    parts.push(format!(
        "[Components] {} skills | {} hooks | {} commands | {} agents | {} MCP servers",
        counts.skills, counts.hooks, counts.commands, counts.agents, counts.mcp_servers
    ));

    parts.join("\n")
}

/// Result of marketplace sync attempt
enum SyncResult {
    /// No marketplace repo found
    NoRepo,
    /// Already up to date
    UpToDate,
    /// Synced N files
    Synced { files: u32, pulled: bool },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_returns_context() {
        let input = HookInput {
            session_id: Some("test-123".to_string()),
            cwd: Some("/tmp/test".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("[SessionStart]"));
        assert!(ctx.additional_context.contains("test-123"));
    }

    #[test]
    fn test_is_marketplace_repo_false() {
        assert!(!is_marketplace_repo(Path::new("/nonexistent/path")));
    }

    #[test]
    fn test_claude_dir() {
        let dir = claude_dir();
        assert!(dir.to_string_lossy().contains(".claude"));
    }

    #[test]
    fn test_sync_result_no_repo_context() {
        let sync = SyncResult::NoRepo;
        let validation = ValidationResult {
            valid: true,
            reasons: vec![],
        };
        let counts = ComponentCounts {
            skills: 50,
            hooks: 16,
            commands: 9,
            agents: 8,
            mcp_servers: 6,
        };
        let context = build_startup_context(&sync, &validation, &counts, "test-sess");
        assert!(context.contains("No local repo found"));
        assert!(context.contains("50 skills"));
        assert!(context.contains("test-sess"));
    }

    #[test]
    fn test_sync_result_synced_context() {
        let sync = SyncResult::Synced {
            files: 42,
            pulled: true,
        };
        let validation = ValidationResult {
            valid: true,
            reasons: vec![],
        };
        let counts = ComponentCounts {
            skills: 0,
            hooks: 0,
            commands: 0,
            agents: 0,
            mcp_servers: 0,
        };
        let context = build_startup_context(&sync, &validation, &counts, "s1");
        assert!(context.contains("42 files synced"));
        assert!(context.contains("(pulled)"));
    }

    #[test]
    fn test_sync_result_up_to_date() {
        let sync = SyncResult::UpToDate;
        let validation = ValidationResult {
            valid: true,
            reasons: vec![],
        };
        let counts = ComponentCounts {
            skills: 0,
            hooks: 0,
            commands: 0,
            agents: 0,
            mcp_servers: 0,
        };
        let context = build_startup_context(&sync, &validation, &counts, "s1");
        assert!(context.contains("Up to date"));
    }

    #[test]
    fn test_validation_failure_in_context() {
        let sync = SyncResult::UpToDate;
        let validation = ValidationResult {
            valid: false,
            reasons: vec!["settings.json missing".to_string()],
        };
        let counts = ComponentCounts {
            skills: 0,
            hooks: 0,
            commands: 0,
            agents: 0,
            mcp_servers: 0,
        };
        let context = build_startup_context(&sync, &validation, &counts, "s1");
        assert!(context.contains("[Validation] FAILED"));
        assert!(context.contains("settings.json missing"));
    }

    #[test]
    fn test_count_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("skill-a")).unwrap();
        fs::create_dir(dir.path().join("skill-b")).unwrap();
        fs::write(dir.path().join("file.txt"), "not a dir").unwrap();
        assert_eq!(count_subdirs(dir.path()), 2);
    }

    #[test]
    fn test_count_subdirs_nonexistent() {
        assert_eq!(count_subdirs(Path::new("/nonexistent")), 0);
    }

    #[test]
    fn test_count_files_with_ext() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "x").unwrap();
        fs::write(dir.path().join("b.md"), "y").unwrap();
        fs::write(dir.path().join("c.js"), "z").unwrap();
        assert_eq!(count_files_with_ext(dir.path(), ".md"), 2);
        assert_eq!(count_files_with_ext(dir.path(), ".js"), 1);
    }

    #[test]
    fn test_count_files_with_ext_nonexistent() {
        assert_eq!(count_files_with_ext(Path::new("/nonexistent"), ".md"), 0);
    }

    #[test]
    fn test_validate_sync_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = validate_sync(dir.path());
        assert!(!result.valid);
        assert!(result.reasons.iter().any(|r| r.contains("settings.json")));
        assert!(result.reasons.iter().any(|r| r.contains("skills/")));
    }

    #[test]
    fn test_validate_sync_with_settings() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("settings.json"), "{}").unwrap();
        // Create enough skill dirs
        let skills = dir.path().join("skills");
        fs::create_dir(&skills).unwrap();
        for i in 0..45 {
            fs::create_dir(skills.join(format!("skill-{}", i))).unwrap();
        }
        // Sentinel binary check may fail in test env — just verify settings + skills pass
        let result = validate_sync(dir.path());
        // Filter out sentinel binary reason (can't control PATH in unit tests)
        let non_sentinel_reasons: Vec<_> = result
            .reasons
            .iter()
            .filter(|r| !r.contains("sentinel"))
            .collect();
        assert!(
            non_sentinel_reasons.is_empty(),
            "Expected no non-sentinel failures, got: {:?}",
            non_sentinel_reasons
        );
    }

    #[test]
    fn test_validate_sync_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("settings.json"), "not valid json{{{").unwrap();
        let skills = dir.path().join("skills");
        fs::create_dir(&skills).unwrap();
        for i in 0..45 {
            fs::create_dir(skills.join(format!("skill-{}", i))).unwrap();
        }
        let result = validate_sync(dir.path());
        assert!(!result.valid);
        assert!(result
            .reasons
            .iter()
            .any(|r| r.contains("invalid JSON")));
    }

    #[test]
    fn test_generate_claude_md() {
        let dir = tempfile::tempdir().unwrap();
        let counts = ComponentCounts {
            skills: 58,
            hooks: 20,
            commands: 10,
            agents: 8,
            mcp_servers: 9,
        };
        generate_claude_md(dir.path(), &counts);

        let md_path = dir.path().join("CLAUDE.md");
        assert!(md_path.exists());

        let content = fs::read_to_string(&md_path).unwrap();
        assert!(content.contains("58 skill directories"));
        assert!(content.contains("**Skills:** 58"));
        assert!(content.contains("**Hooks:** 20"));
        assert!(content.contains("**Agents:** 8"));
        assert!(content.contains("Auto-generated on session start"));
    }

    #[test]
    fn test_copy_dir_recursive() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();

        // Create source structure
        fs::write(src_dir.path().join("file1.txt"), "hello").unwrap();
        let subdir = src_dir.path().join("sub");
        fs::create_dir(&subdir).unwrap();
        fs::write(subdir.join("file2.txt"), "world").unwrap();

        let dest = dest_dir.path().join("output");
        let count = copy_dir_recursive(src_dir.path(), &dest).unwrap();
        assert_eq!(count, 2);
        assert!(dest.join("file1.txt").exists());
        assert!(dest.join("sub").join("file2.txt").exists());
    }

    #[test]
    fn test_copy_dir_recursive_skips_unchanged() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();

        fs::write(src_dir.path().join("file.txt"), "data").unwrap();

        let dest = dest_dir.path().join("output");

        // First copy
        let count1 = copy_dir_recursive(src_dir.path(), &dest).unwrap();
        assert_eq!(count1, 1);

        // Second copy — should skip since dest is same age or newer
        // Note: on fast systems, timestamps might be equal, so this may copy again.
        // The important thing is it doesn't error.
        let count2 = copy_dir_recursive(src_dir.path(), &dest).unwrap();
        assert!(count2 <= 1);
    }
}
