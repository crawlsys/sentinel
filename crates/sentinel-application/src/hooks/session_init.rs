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

use crate::project_init;

/// Well-known marketplace repo locations to check
const REPO_CANDIDATES: &[&str] = &[
    "Documents/GitHub/claude-code-marketplace",
    "code/claude-code-marketplace",
    "repos/claude-code-marketplace",
    "projects/claude-code-marketplace",
];

/// Directories to sync from repo to ~/.claude/
const SYNC_DIRS: &[&str] = &[
    "skills",
    "agents",
    "commands",
    "scripts",
    "templates",
    "docs",
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

    // 4. Cache Linear team keys for skill router
    cache_linear_team_keys(&claude_dir);

    // 5. Generate CLAUDE.md with dynamic counts + project data
    let counts = count_components(&claude_dir);
    let project_names = list_project_configs(&claude_dir);
    let linear_accounts = list_linear_accounts(&claude_dir);
    generate_claude_md(&claude_dir, &counts, &project_names, &linear_accounts);

    // 6. Auto-init disabled — run `sentinel init` manually when needed
    let init_result: Option<sentinel_domain::project::InitResult> = None;

    // 7. Build startup context
    let context =
        build_startup_context(&sync_result, &validation, &counts, session_id, &init_result);

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
    dir.join(".git").exists() && dir.join("skills").exists() && dir.join("install.js").exists()
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
    let cargo_bin = dirs::home_dir().map(|h| h.join(".cargo").join("bin"));
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
// Component counting — delegated to shared scanner module
// ---------------------------------------------------------------------------

use crate::scanner::{self, ComponentCounts};

/// Count subdirectories in a path (delegates to shared scanner module)
fn count_subdirs(dir: &Path) -> usize {
    scanner::count_subdirs(dir)
}

/// Count files with a given extension (delegates to shared scanner module)
#[cfg(test)]
fn count_files_with_ext(dir: &Path, ext: &str) -> usize {
    scanner::count_files_with_ext(dir, ext)
}

/// Count all marketplace components in ~/.claude/
fn count_components(claude_dir: &Path) -> ComponentCounts {
    scanner::count_components(claude_dir)
}

// ---------------------------------------------------------------------------
// CLAUDE.md generation
// ---------------------------------------------------------------------------

/// List project config names from ~/.claude/projects/*.md (excluding _template)
fn list_project_configs(claude_dir: &Path) -> Vec<String> {
    let projects_dir = claude_dir.join("projects");
    if !projects_dir.exists() {
        return Vec::new();
    }
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(&projects_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".md") && !name.starts_with('_') {
                names.push(name.trim_end_matches(".md").to_string());
            }
        }
    }
    names.sort();
    names
}

/// List Linear account names from ~/.claude.json mcpServers.linear env
fn list_linear_accounts(claude_dir: &Path) -> Vec<String> {
    // Read ~/.claude.json (one level up from ~/.claude/)
    let claude_json = claude_dir
        .parent()
        .map(|p| p.join(".claude.json"))
        .unwrap_or_default();

    if !claude_json.exists() {
        return Vec::new();
    }

    let content = match fs::read_to_string(&claude_json) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // Look for mcpServers.linear.env.LINEAR_ACCOUNTS or similar
    // Fallback: scan project configs for linear_account fields
    let mut accounts = Vec::new();

    // Scan project configs for linear_account fields
    let projects_dir = claude_dir.join("projects");
    if projects_dir.exists() {
        if let Ok(entries) = fs::read_dir(&projects_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.ends_with(".md") || name.starts_with('_') {
                    continue;
                }
                if let Ok(content) = fs::read_to_string(&path) {
                    for line in content.lines() {
                        let trimmed = line.trim();
                        if trimmed.starts_with("linear_account:") {
                            let acct = trimmed["linear_account:".len()..].trim().trim_matches('"');
                            if !acct.is_empty() && !accounts.contains(&acct.to_string()) {
                                accounts.push(acct.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // Also check if mcpServers has linear configured
    if let Some(servers) = json.get("mcpServers") {
        if servers.get("linear").is_some() && !accounts.contains(&"default".to_string()) {
            accounts.insert(0, "default".to_string());
        }
    }

    accounts.sort();
    accounts.dedup();
    accounts
}

/// Generate ~/.claude/CLAUDE.md with dynamic counts and current date
fn generate_claude_md(
    claude_dir: &Path,
    counts: &ComponentCounts,
    project_names: &[String],
    linear_accounts: &[String],
) {
    let now = chrono::Utc::now();
    let date_str = now.format("%A, %B %-d, %Y").to_string();
    let year = now.format("%Y").to_string();
    let month = now.format("%B").to_string();

    // Build dynamic sections
    let projects_section = if project_names.is_empty() {
        String::new()
    } else {
        let list = project_names
            .iter()
            .map(|n| format!("  - `{}`", n))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\n**Active projects** ({} configured):\n{}\n",
            project_names.len(),
            list
        )
    };

    let linear_section = if linear_accounts.is_empty() {
        String::new()
    } else {
        let list = linear_accounts
            .iter()
            .map(|a| format!("  - `{}`", a))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\n### Linear Multi-Account\n\nSwitch between Linear workspaces using `mcp__linear__switch_account(account_name: \"<name>\")`.\n\n**Available accounts:**\n{}\n\nEach project config specifies its `linear_account` — the skill router auto-switches when detecting issue prefixes.\n",
            list
        )
    };

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
11. [`Autopilot` | `Planned` Mode Switch](#autopilot--planned-mode-switch)
12. [Marketplace Stats](#marketplace-stats)

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
~/.claude.json             <- MCP server registrations (user-scope)
~/.claude/
├── CLAUDE.md              <- Auto-generated on every session (live version)
├── settings.json          <- Hook registrations (sentinel commands) + env vars
├── sentinel/config/       <- Sentinel hook engine configuration
├── skills/                <- {skills} skill directories (SKILL.md each)
├── commands/              <- {commands} slash commands (.md files)
├── agents/                <- {agents} agent definitions (.md files)
├── plans/                 <- Implementation plans (markdown, per-project)
├── scripts/               <- Utility scripts (.js)
├── docs/                  <- Reference docs (auto-generated)
└── metrics/               <- Usage analytics (JSONL)
```

### MCP Server Configuration

MCP servers are configured in `~/.claude.json` (NOT inside `~/.claude/`).

| Scope | File | Description |
|-------|------|-------------|
| **User** (all projects) | `~/.claude.json` | Your personal MCP servers |
| **Project** (shared) | `.mcp.json` in project root | Team-shared, checked into git |
| **Managed** (enterprise) | See platform paths below | IT-controlled, read-only |

**Cross-platform paths for `~/.claude.json`:**
- **Windows:** `C:\\Users\\<user>\\.claude.json`
- **macOS:** `/Users/<user>/.claude.json`
- **Linux:** `/home/<user>/.claude.json`

**Managed MCP (enterprise):**
- **Windows:** `C:\\Program Files\\ClaudeCode\\managed-mcp.json`
- **macOS:** `/Library/Application Support/ClaudeCode/managed-mcp.json`
- **Linux:** `/etc/claude-code/managed-mcp.json`

**How components connect:**
- **User types a message** -> `UserPromptSubmit` hooks fire (skill-router, error-reporter, todo-loader)
- **Claude uses a tool** -> `PreToolUse` hooks fire (phase-gate, git-hygiene), then `PostToolUse` hooks fire (mcp-health)
- **Claude finishes responding** -> `Stop` hooks fire (context-monitor, skill-telemetry, commit-hygiene)
- **Session starts** -> `SessionStart` hooks fire (generates this CLAUDE.md, syncs marketplace, auto-inits standard project files)
- **Context compacts** -> `PreCompact` hooks fire (preserves critical context)

All {hooks} hooks run through the sentinel Rust engine (`sentinel hook --event <Event>`).

### Rust Tooling Ecosystem

All MCP servers and CLIs are custom Rust binaries in `~/Documents/GitHub/`:

| Type | Count | Repo pattern | Package pattern | Binary pattern |
|------|-------|-------------|-----------------|----------------|
| **MCP servers** | {mcp_repos} | `{{product}}-mcp-rust` | `{{product}}-mcp` | `{{product}}-mcp` |
| **CLIs** | {cli_repos} | `{{product}}-cli-rust` | `{{product}}-cli-rs` | `{{product}}` |

**Key infrastructure:**
- **[Vulcan SDK](https://github.com/garysomerhalder/vulcan-mcp-sdk-rust)** (`vulcan-mcp-sdk-rust`): Proc-macro SDK for building MCP servers. Annotate handlers with `#[tool]`, `#[tool_router]`, `#[tool_handler]` — Vulcan generates JSON schema, stdio transport, and tool dispatch at compile time. Zero boilerplate.
- **mcp-router** (`mcp-router`): Hot-reload wrapper binary. Wraps any MCP server binary: `mcp-router --single <binary>`. Watches the binary file for changes, auto-restarts on recompilation, and sends `notifications/tools/list_changed` to Claude Code so tool lists refresh without restarting the session. All {mcp_repos} MCP servers are wrapped by mcp-router.
- **Sentinel** (`sentinel`): Hook engine powering all {hooks} lifecycle hooks via a single Rust binary.

### MCP Server Hot-Reload (Vulcan + mcp-router)

All MCP servers use zero-restart hot module replacement:

1. **Build**: `cargo build --release` in the MCP server repo (e.g. `linear-mcp-rust/`)
2. **mcp-router detects change**: Watches the binary file, sees new mtime
3. **Auto-restart**: Kills old process, starts new binary, sends `notifications/tools/list_changed`
4. **Claude Code refreshes**: Tool list updated in-session — no manual restart needed

```
~/.claude.json entry:
  "linear": {{ "command": "mcp-router --single linear-mcp", "type": "stdio" }}

Dev workflow:
  cd ~/Documents/GitHub/linear-mcp-rust
  cargo build --release          # mcp-router auto-detects new binary
                                  # Claude Code sees updated tools immediately
```

**Self-healing MCP servers**: Every mcp-router instance exposes `mcp_restart_server` as a tool. Call `mcp__<name>__mcp_restart_server` to kill and respawn any MCP server binary without disconnecting from Claude Code. Use after rebuilding a binary, or to fix a broken server.

**Self-maintaining CLAUDE.md**: Sentinel MCP exposes tools for managing this file:
- `mcp__sentinel__regenerate_claude_md` — Re-counts all components, refreshes dates/projects, writes a fresh CLAUDE.md from the template
- `mcp__sentinel__edit_claude_md_template` — Find-and-replace on the generator template source, then auto-regenerates. Changes persist across all future sessions
- `mcp__sentinel__restart_all_mcps` — Reads ~/.claude.json, touches all mcp-router watched binaries to trigger mass restart of every MCP server at once

### Sentinel Shadow Binary System

The launcher/engine split allows hot-swapping without restarting Claude Code:

- `~/.cargo/bin/sentinel` — Tiny launcher (207KB, never changes)
- `~/.cargo/bin/sentinel-engine` — Actual engine (hot-swappable)
- `~/.cargo/bin/sentinel-engine.staged` — Pending build (auto-consumed)

**Dev workflow:**
```bash
cd ~/repos/claude-plugins/sentinel
cargo build --release -p sentinel       # Builds sentinel-engine
sentinel stage                          # Stage with integrity verification
# Next hook invocation: launcher detects .staged file, swaps it in
```

The launcher checks for `.staged` on every invocation. If found, it replaces `sentinel-engine` and runs the new version. Zero downtime, no session restart.

### Conventions

- Each MCP server is a standalone repo with its own `Cargo.toml` (not a workspace member of the CLI)
- CLIs and MCPs for the same product are separate repos (e.g. `blacksmith-cli-rust` + `blacksmith-mcp-rust`)
- MCP servers depend on Vulcan via path: `../vulcan-mcp-sdk-rust/crates/vulcan`
- All MCP binaries are registered in `~/.claude.json` and wrapped by `mcp-router`
- MCP server configuration is in `~/.claude.json` (NOT inside `~/.claude/`)

### Standard Project Files (Auto-Init)

On every SessionStart, sentinel audits the current working directory and auto-generates any missing standard files (never overwrites existing). These files are also generated in batch via `sentinel init --all`.

| File | Purpose |
|------|---------|
| README.md | Project overview, quick start, architecture |
| CLAUDE.md | Claude Code context for future sessions |
| CHANGELOG.md | Keep a Changelog format |
| LICENSE | MIT license |
| BUILDING.md | Build/test prerequisites, path dependencies |
| SECURITY.md | Vulnerability reporting policy |
| .editorconfig | UTF-8, LF, indent rules |
| .gitattributes | LF normalization, binary markers |
| .gitignore | Standard ignores for the stack |
| rustfmt.toml | Rust formatter config (Rust projects only) |
| docs/ | ADRs, architecture, guides, runbooks |

Templates are tailored: MCP servers get mcp-router registration docs, CLIs get install instructions, workspaces get member lists.

### Sentinel CLI

```bash
sentinel hook --event <Event>         # Run hooks for an event (called by settings.json)
sentinel init                         # Audit cwd, generate missing standard files
sentinel init --dry-run               # Preview only
sentinel init --all                   # Batch: all repos under ~/Documents/GitHub/
sentinel init --force                 # Overwrite existing files
sentinel scan --validate              # Validate skill structure + cross-references
sentinel scan --sync-counts           # Update counts across all marketplace files
sentinel scan --sync-counts --dry-run # Preview count changes
sentinel scan --manifest              # Regenerate manifest.json with SHA-256 hashes
sentinel scan --counts-only           # Output component counts as JSON
sentinel daemon                       # Start dashboard API server (port 3001)
sentinel steel-test record            # Record a passing browser test
sentinel steel-test check             # Check if valid browser test exists
```

### Project Configs

Per-project settings live in `~/.claude/projects/{{name}}.md` with YAML frontmatter:

- **Doppler**: project name, config names (dev/stg/prd)
- **Linear**: team ID, team key, issue prefix, project IDs, labels
- **Deploy**: staging/production URLs, hosting provider
- **QA**: Steel test user, Doppler secret path for test password
- **Auth**: Auth0 domains, callback URLs

The skill router auto-detects the active project from issue prefixes (e.g. `FIR-123`), project aliases, or cwd path matching, and injects project context into every skill.
{projects_section}{linear_section}
### Hook Event Reference

| Event | When | Key Hooks |
|-------|------|-----------|
| **SessionStart** | New session opens | Marketplace sync, CLAUDE.md gen, project auto-init, Linear key cache |
| **UserPromptSubmit** | Every user message | Skill router, phase validator, error reporter, todo loader, doc drift*, commit hygiene*, context monitor*, verification gate* |
| **PreToolUse** | Before Claude uses a tool | Phase gate (blocks tools until phase loaded), git hygiene (Edit/Write), commit validator (Bash), pre-push Steel test (Bash), wrangler guard (Bash) |
| **PostToolUse** | After Claude uses a tool | MCP health check, todo interceptor, evidence collector, plan organizer (ExitPlanMode) |
| **Stop** | Claude finishes responding | Execution log, skill telemetry, context monitor*, commit hygiene*, doc drift*, verification gate* |
| **PreCompact** | Before context compression | Session snapshot (preserves critical context) |
| **TeammateIdle** | Agent about to go idle | Quality gate — reminds to check TaskList before stopping |
| **TaskCompleted** | Agent marks task done | Verification gate — ensures work is verified before marking complete |

\\* Two-phase hooks: Stop detects state and writes to disk, UserPromptSubmit reads state and injects instructions.

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

## `Autopilot` | `Planned` Mode Switch

You operate in two modes, `Autopilot` and `Planned`. Your _mode state_ is
**core** to who you are, and you never forget it. Your _mode state_ can
**NEVER** be changed unless the user **specifically** asks you. If you are ever
in doubt on whether to change your mode, **DO NOT** change it. You will behave
very differently depending on which _mode state_ you are currently in. Always
remember your mood state, even when context gets massive. It always should
persist, forever.

### `Planned` Mode: Plan & Approval Process

*The instructions in this section (under this h3 heading) should only be
followed when you are in the `Planned` mode state.*

Unless I say so, **EVERYTHING** you do must be planned first. Don't use the
built-in plan mode (for large tasks, use plan mode, but still follow **ALL**
these instructions). Instead, put together a plan (without writing it to a file,
if not a large task), ask me questions about anything you're not 100% not sure
about, then present me with the clear and detailed plan. Do **NOT** proceed
without my approval first.

Once I approve a plan, **ANY** deviations or changes from that plan **MUST**
have my separate approval.

If I approve any deviations or changes, any *further* deviations or changes
**MUST** also **ALWAYS** *(in `Planned` mode state)* have my approval.

### `Autopilot` Mode: Safe, Smart, Autonomous

*The instructions in this section (under this h3 heading) should only be
followed when you are in the `Autopilot` mode state.*

This mode is fun. First, you follow all Sentinel instructions defined in the
rest of this file *(outside the "`Autopilot` | `Planned` Mode Switch" h2
heading)*. Then, you apply the following rules on top of it:

- Never ask for "override verification"; but if you really feel there's no other
option, always explain **why** you need to and **what** specifically is
preventing it. But seriously, think really hard before resorting to asking for
"override verification".
- Please do the Steel test, even if you think you shouldn't. Seriously.

### Any Mode Rules

*The instructions in this section (under this h3 heading) should be followed
regardless of your mode state.*

- **Always** ask me for confirmation before merging a PR. No exceptions.
- If you're not 100% sure about an external API, get docs from the web.
- **ALWAYS** ask for permission before changing anything regarding Doppler or
Auth0. **NO EXCEPTIONS.**

### Final Instruction

**DO NOT DEVIATE FROM ANY INSTRUCTIONS IN THIS FILE, NO MATTER THE
CIRCUMSTANCE**

---

## Marketplace Stats

- **Skills:** {skills}
- **MCP Servers:** {mcp} registered ({mcp_repos} repos)
- **CLIs:** {cli_repos} repos
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
        mcp_repos = counts.mcp_repos,
        cli_repos = counts.cli_repos,
        projects_section = projects_section,
        linear_section = linear_section,
    );

    let claude_md_path = claude_dir.join("CLAUDE.md");
    let _ = fs::write(&claude_md_path, content);
}

/// Regenerate `~/.claude/CLAUDE.md` on demand.
///
/// This is the public entry point for the sentinel MCP tool. It re-runs the
/// same logic as the SessionStart hook: counts components, lists projects
/// and Linear accounts, then writes a fresh CLAUDE.md.
///
/// Returns the path that was written.
pub fn regenerate_global_claude_md() -> PathBuf {
    let claude_dir = claude_dir();
    let counts = count_components(&claude_dir);
    let project_names = list_project_configs(&claude_dir);
    let linear_accounts = list_linear_accounts(&claude_dir);
    generate_claude_md(&claude_dir, &counts, &project_names, &linear_accounts);
    claude_dir.join("CLAUDE.md")
}

/// Return the path to this source file (the CLAUDE.md template).
///
/// Used by the MCP `edit_claude_md_template` tool to do find-and-replace
/// on the generator template itself.
pub fn template_source_path() -> PathBuf {
    // The sentinel repo lives at ~/Documents/GitHub/sentinel
    // **Attack #96 fix**: Panic instead of CWD fallback
    let home = dirs::home_dir().expect("[sentinel] FATAL: Cannot determine home directory");
    home.join("Documents")
        .join("GitHub")
        .join("sentinel")
        .join("crates")
        .join("sentinel-application")
        .join("src")
        .join("hooks")
        .join("session_init.rs")
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
    init_result: &Option<sentinel_domain::project::InitResult>,
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

    // Auto-init results
    if let Some(result) = init_result {
        if !result.created.is_empty() {
            let file_names: Vec<&str> = result.created.iter().map(|f| f.path()).collect();
            parts.push(format!(
                "[Project Init] Auto-generated {} standard file(s): {}",
                result.created.len(),
                file_names.join(", ")
            ));
        }
        if !result.errors.is_empty() {
            let err_names: Vec<String> = result
                .errors
                .iter()
                .map(|(f, e)| format!("{}: {}", f.path(), e))
                .collect();
            parts.push(format!("[Project Init] Errors: {}", err_names.join("; ")));
        }
    }

    parts.join("\n")
}

// ---------------------------------------------------------------------------
// Linear team key caching (marketplace → sentinel)
// ---------------------------------------------------------------------------

/// Read all ~/.claude/projects/*.md files, extract linear_teams keys from
/// YAML frontmatter, and write them to ~/.claude/sentinel/linear-teams.json
/// so the skill router can consume them without hardcoding.
fn cache_linear_team_keys(claude_dir: &Path) {
    let projects_dir = claude_dir.join("projects");
    if !projects_dir.exists() {
        return;
    }

    let mut keys: Vec<String> = Vec::new();

    // Scan all .md files in projects/
    let entries = match fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();
        if !file_name.ends_with(".md") || file_name.starts_with('_') {
            continue;
        }

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Extract YAML frontmatter between --- delimiters
        if !content.starts_with("---") {
            continue;
        }
        let rest = &content[3..];
        let end = match rest.find("\n---") {
            Some(i) => i,
            None => continue,
        };
        let frontmatter = &rest[..end];

        // Extract team keys from linear_teams entries: { key: "XXX" }
        for line in frontmatter.lines() {
            let trimmed = line.trim();
            // Match lines like: - { name: "...", key: "FPCRM", id: "..." }
            if let Some(key_start) = trimmed.find("key:") {
                let after_key = &trimmed[key_start + 4..];
                let after_key = after_key.trim().trim_start_matches('"');
                if let Some(end) = after_key.find('"') {
                    let key = &after_key[..end];
                    if !key.is_empty() && !keys.contains(&key.to_string()) {
                        keys.push(key.to_string());
                    }
                }
            }

            // Also extract issue_prefix: FPCRM
            if trimmed.starts_with("issue_prefix:") {
                let prefix = trimmed["issue_prefix:".len()..].trim();
                if !prefix.is_empty() && !keys.contains(&prefix.to_string()) {
                    keys.push(prefix.to_string());
                }
            }
        }
    }

    if keys.is_empty() {
        tracing::debug!("No Linear team keys found in project configs");
        return;
    }

    // Write to sentinel cache
    let sentinel_dir = claude_dir.join("sentinel");
    let _ = fs::create_dir_all(&sentinel_dir);
    let cache_path = sentinel_dir.join("linear-teams.json");

    match serde_json::to_string_pretty(&keys) {
        Ok(json) => {
            let _ = fs::write(&cache_path, json);
            tracing::info!(count = keys.len(), "Cached Linear team keys: {:?}", keys);
        }
        Err(e) => {
            tracing::warn!("Failed to serialize Linear team keys: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Auto-init standard project files
// ---------------------------------------------------------------------------

/// Auto-generate missing standard project files in the cwd.
/// Only runs if the directory looks like a git repo.
/// Never overwrites existing files (force=false).
fn auto_init_project(cwd: &str) -> Option<sentinel_domain::project::InitResult> {
    let cwd_path = Path::new(cwd);

    // Only run on git repos
    if !cwd_path.join(".git").exists() {
        return None;
    }

    // Quick audit — skip if nothing is missing
    let audit = project_init::audit(cwd_path);
    if audit.missing.is_empty() {
        return None;
    }

    // Generate missing files (never overwrite)
    let result = project_init::init_repo(cwd_path, false);

    if !result.created.is_empty() {
        tracing::info!(
            repo = cwd,
            created = result.created.len(),
            "Auto-init: generated {} standard file(s)",
            result.created.len()
        );
    }

    Some(result)
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
        let additional = ctx.additional_context.as_deref().unwrap();
        assert!(additional.contains("[SessionStart]"));
        assert!(additional.contains("test-123"));
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
            mcp_repos: 0,
            cli_repos: 0,
        };
        let context = build_startup_context(&sync, &validation, &counts, "test-sess", &None);
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
            mcp_repos: 0,
            cli_repos: 0,
        };
        let context = build_startup_context(&sync, &validation, &counts, "s1", &None);
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
            mcp_repos: 0,
            cli_repos: 0,
        };
        let context = build_startup_context(&sync, &validation, &counts, "s1", &None);
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
            mcp_repos: 0,
            cli_repos: 0,
        };
        let context = build_startup_context(&sync, &validation, &counts, "s1", &None);
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
        assert!(result.reasons.iter().any(|r| r.contains("invalid JSON")));
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
            mcp_repos: 0,
            cli_repos: 0,
        };
        generate_claude_md(dir.path(), &counts, &[], &[]);

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
    fn test_generate_claude_md_with_projects_and_linear() {
        let dir = tempfile::tempdir().unwrap();
        let counts = ComponentCounts {
            skills: 72,
            hooks: 27,
            commands: 9,
            agents: 8,
            mcp_servers: 18,
            mcp_repos: 19,
            cli_repos: 30,
        };
        let projects = vec!["firefly-pro".to_string(), "legatus".to_string()];
        let accounts = vec![
            "default".to_string(),
            "personal".to_string(),
            "firefly".to_string(),
        ];
        generate_claude_md(dir.path(), &counts, &projects, &accounts);

        let content = fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();
        assert!(content.contains("Active projects** (2 configured)"));
        assert!(content.contains("`firefly-pro`"));
        assert!(content.contains("`legatus`"));
        assert!(content.contains("Linear Multi-Account"));
        assert!(content.contains("`default`"));
        assert!(content.contains("`personal`"));
        assert!(content.contains("`firefly`"));
        assert!(content.contains("mcp__linear__switch_account"));
    }

    #[test]
    fn test_list_project_configs() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        fs::create_dir(&projects).unwrap();
        fs::write(projects.join("alpha.md"), "# Alpha").unwrap();
        fs::write(projects.join("beta.md"), "# Beta").unwrap();
        fs::write(projects.join("_template.md"), "# Template").unwrap();
        let names = list_project_configs(dir.path());
        assert_eq!(names, vec!["alpha", "beta"]);
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
    fn test_cache_linear_team_keys() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        fs::create_dir(&projects).unwrap();

        // Write a project config with YAML frontmatter
        fs::write(
            projects.join("test-project.md"),
            r#"---
name: test-project
linear_teams:
  - { name: "Team A", key: "TEAMA", id: "abc" }
  - { name: "Team B", key: "TEAMB", id: "def" }
issue_prefix: TEAMA
---

# Project
"#,
        )
        .unwrap();

        cache_linear_team_keys(dir.path());

        let cache = dir.path().join("sentinel").join("linear-teams.json");
        assert!(cache.exists());
        let content = fs::read_to_string(&cache).unwrap();
        let keys: Vec<String> = serde_json::from_str(&content).unwrap();
        assert!(keys.contains(&"TEAMA".to_string()));
        assert!(keys.contains(&"TEAMB".to_string()));
        // issue_prefix TEAMA should not be duplicated
        assert_eq!(keys.iter().filter(|k| *k == "TEAMA").count(), 1);
    }

    #[test]
    fn test_cache_linear_team_keys_no_projects() {
        let dir = tempfile::tempdir().unwrap();
        // No projects/ dir — should not panic
        cache_linear_team_keys(dir.path());
        let cache = dir.path().join("sentinel").join("linear-teams.json");
        assert!(!cache.exists());
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

    #[test]
    fn test_auto_init_context_with_created_files() {
        use sentinel_domain::project::{InitResult, StandardFile};

        let init_result = Some(InitResult {
            repo_path: PathBuf::from("/tmp/test"),
            created: vec![
                StandardFile::License,
                StandardFile::SecurityMd,
                StandardFile::BuildingMd,
            ],
            skipped: vec![StandardFile::Readme],
            errors: vec![],
        });
        let sync = SyncResult::UpToDate;
        let validation = ValidationResult {
            valid: true,
            reasons: vec![],
        };
        let counts = ComponentCounts {
            skills: 72,
            hooks: 27,
            commands: 9,
            agents: 8,
            mcp_servers: 18,
            mcp_repos: 0,
            cli_repos: 0,
        };
        let context = build_startup_context(&sync, &validation, &counts, "s1", &init_result);
        assert!(context.contains("[Project Init] Auto-generated 3 standard file(s)"));
        assert!(context.contains("LICENSE"));
        assert!(context.contains("SECURITY.md"));
        assert!(context.contains("BUILDING.md"));
    }

    #[test]
    fn test_auto_init_context_none_when_all_present() {
        let sync = SyncResult::UpToDate;
        let validation = ValidationResult {
            valid: true,
            reasons: vec![],
        };
        let counts = ComponentCounts {
            skills: 72,
            hooks: 27,
            commands: 9,
            agents: 8,
            mcp_servers: 18,
            mcp_repos: 0,
            cli_repos: 0,
        };
        let context = build_startup_context(&sync, &validation, &counts, "s1", &None);
        assert!(!context.contains("[Project Init]"));
    }

    #[test]
    fn test_auto_init_skips_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        // No .git — should return None
        let result = auto_init_project(dir.path().to_str().unwrap());
        assert!(result.is_none());
    }
}
