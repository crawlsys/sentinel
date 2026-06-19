//! Marketplace Scanner
//!
//! Scans the filesystem and builds a complete marketplace snapshot.
//! Shared logic used by both `session_init` (for CLAUDE.md generation)
//! and `sentinel scan` CLI command (for the local API).
//!
//! Ported from the old marketplace scanner script into Rust.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Dynamic component counts for CLAUDE.md generation and local clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentCounts {
    pub skills: usize,
    pub hooks: usize,
    pub commands: usize,
    pub agents: usize,
    pub mcp_servers: usize,
    pub mcp_repos: usize,
    pub cli_repos: usize,
}

/// A parsed skill from the filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub version: String,
    pub description: String,
    pub icon: String,
    pub priority: Option<u32>,
    pub allowed_tools: Vec<String>,
    pub dependencies: Vec<SkillDependency>,
    pub category: String,
    pub has_sub_modules: bool,
    pub file_path: String,
}

/// A `@use` dependency extracted from a SKILL.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDependency {
    pub skill: String,
    pub patterns: Vec<String>,
}

/// A parsed hook from sentinel hooks.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hook {
    pub name: String,
    pub file: String,
    pub event: String,
    pub matcher: String,
    pub description: String,
    pub depends_on: Vec<String>,
    pub has_api_call: bool,
    pub engine: String,
}

/// A parsed agent from marketplace.json + filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub model: String,
    pub file: String,
    pub description: String,
}

/// A parsed slash command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandDef {
    pub name: String,
    pub description: String,
    pub allowed_tools: Vec<String>,
    pub argument_hint: String,
}

/// An MCP server entry from marketplace.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub transport: String,
    pub optional: bool,
}

/// A dependency edge in the skill graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub from: String,
    pub to: String,
    pub patterns: Vec<String>,
}

/// A single validation check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    pub category: String,
    pub rule: String,
    pub status: String, // "pass", "fail", "warn"
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<usize>,
}

/// Validation report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub timestamp: String,
    pub duration_ms: u64,
    pub passed: usize,
    pub failed: usize,
    pub warned: usize,
    pub results: Vec<ValidationResult>,
}

/// Full marketplace snapshot — matches the JSON contract from scanner.cjs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceSnapshot {
    pub version: String,
    pub description: String,
    pub skills: Vec<Skill>,
    pub hooks: Vec<Hook>,
    pub agents: Vec<Agent>,
    pub commands: Vec<CommandDef>,
    pub mcp_servers: Vec<McpServer>,
    pub dependency_edges: Vec<DependencyEdge>,
    pub validation: ValidationReport,
    pub counts: ComponentCounts,
}

// ---------------------------------------------------------------------------
// Counting functions (extracted from session_init.rs)
// ---------------------------------------------------------------------------

/// Count subdirectories in a path.
pub fn count_subdirs(dir: &Path) -> usize {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_type().is_ok_and(|ft| ft.is_dir())
                    && !e.file_name().to_string_lossy().starts_with('_')
            })
            .count()
    })
}

/// Count files with a given extension in a directory (non-recursive).
pub fn count_files_with_ext(dir: &Path, ext: &str) -> usize {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_type().is_ok_and(|ft| ft.is_file())
                    && e.file_name().to_string_lossy().ends_with(ext)
            })
            .count()
    })
}

/// Count MCP servers from `~/.claude.json`.
///
/// `home_dir` should be the user's home directory (parent of `~/.claude/`).
pub fn count_mcp_servers(home_dir: &Path) -> usize {
    let path = home_dir.join(".claude.json");

    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|json| {
            json.get("mcpServers")
                .and_then(|v| v.as_object())
                .map(serde_json::Map::len)
        })
        .unwrap_or(0)
}

/// Count MCP servers a marketplace repo *declares*, from `<root>/marketplace.json`'s
/// `mcp[]` array.
///
/// Returns `None` when `root_dir` has no readable `marketplace.json` with an `mcp`
/// array (e.g. when scanning `~/.claude/`, which has no such file) — the caller
/// then falls back to the live `~/.claude.json` count via [`count_mcp_servers`].
/// This is what makes `count_components` honest in BOTH contexts: a marketplace
/// clone reports what the marketplace ships; a live `~/.claude` reports what is
/// actually registered.
pub fn count_declared_mcp_servers(root_dir: &Path) -> Option<usize> {
    let content = fs::read_to_string(root_dir.join("marketplace.json")).ok()?;
    let data = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    data.get("mcp")
        .and_then(|v| v.as_array())
        .map(std::vec::Vec::len)
}

/// Count Rust repos matching a suffix pattern in `~/Documents/GitHub/`.
///
/// `home_dir` should be the user's home directory (parent of `~/.claude/`).
pub fn count_repos_with_suffix(home_dir: &Path, suffix: &str) -> usize {
    let gh_dir = home_dir.join("Documents").join("GitHub");

    fs::read_dir(gh_dir).map_or(0, |entries| {
        entries
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_type().is_ok_and(|ft| ft.is_dir())
                    && e.file_name().to_string_lossy().ends_with(suffix)
            })
            .count()
    })
}

/// Count all marketplace components in `~/.claude/`.
///
/// Derives the user's home directory from `claude_dir` (its parent) to locate
/// `~/.claude.json` and `~/Documents/GitHub/` without calling `dirs::home_dir()`.
pub fn count_components(claude_dir: &Path) -> ComponentCounts {
    let home_dir = claude_dir.parent().unwrap_or(claude_dir).to_path_buf();

    let skills = count_subdirs(&claude_dir.join("skills"));
    let hooks = super::hooks::HOOK_NAMES.len();
    let commands = count_files_with_ext(&claude_dir.join("commands"), ".md");
    let agents = count_files_with_ext(&claude_dir.join("agents"), ".md");
    // Prefer the marketplace's own declaration (`marketplace.json` mcp[]) when
    // scanning a marketplace repo; fall back to the live `~/.claude.json` only
    // when no such declaration exists. Without this, scanning a marketplace
    // clone reported 0 MCP servers (the clone has no ~/.claude.json) and
    // `--sync-counts` would write that 0 into the repo's own docs.
    let mcp_servers =
        count_declared_mcp_servers(claude_dir).unwrap_or_else(|| count_mcp_servers(&home_dir));
    let mcp_repos = count_repos_with_suffix(&home_dir, "-mcp-rust");
    let cli_repos = count_repos_with_suffix(&home_dir, "-cli-rust");

    ComponentCounts {
        skills,
        hooks,
        commands,
        agents,
        mcp_servers,
        mcp_repos,
        cli_repos,
    }
}

// ---------------------------------------------------------------------------
// Parsing functions (ported from scanner.cjs)
// ---------------------------------------------------------------------------

/// Parse YAML frontmatter from a SKILL.md / command .md file.
/// Returns key-value pairs. Handles multi-line `>` and `|` values.
pub fn parse_frontmatter(content: &str) -> BTreeMap<String, String> {
    let normalized = content.replace("\r\n", "\n");
    let mut result = BTreeMap::new();

    // Extract frontmatter block between --- markers
    let Some(start) = normalized.find("---\n") else {
        return result;
    };
    let after_first = &normalized[start + 4..];
    let Some(end) = after_first.find("\n---") else {
        return result;
    };
    let yaml = &after_first[..end];

    let mut current_key: Option<String> = None;
    let mut current_value = String::new();

    for line in yaml.lines() {
        // Check for key: value pattern
        if let Some(colon_pos) = line.find(':') {
            let key_part = &line[..colon_pos];
            // Key must be at start of line (no leading whitespace) and contain only word chars/hyphens
            if !key_part.is_empty()
                && !key_part.starts_with(' ')
                && key_part
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            {
                // Save previous key
                if let Some(ref key) = current_key {
                    result.insert(key.clone(), current_value.trim().to_string());
                }
                current_key = Some(key_part.to_string());
                let val = line[colon_pos + 1..].trim();
                if val == ">" || val == "|" {
                    current_value = String::new();
                } else {
                    // Strip surrounding quotes
                    current_value = val
                        .trim_start_matches('"')
                        .trim_end_matches('"')
                        .trim_start_matches('\'')
                        .trim_end_matches('\'')
                        .to_string();
                }
                continue;
            }
        }

        // Continuation line (indented)
        if current_key.is_some() && line.starts_with(' ') {
            if !current_value.is_empty() {
                current_value.push(' ');
            }
            current_value.push_str(line.trim());
        }
    }

    // Save last key
    if let Some(key) = current_key {
        result.insert(key, current_value.trim().to_string());
    }

    result
}

/// Extract `@use` dependencies from SKILL.md content.
/// Only matches `@use` in `` ```skills `` fenced code blocks.
pub fn extract_dependencies(content: &str) -> Vec<SkillDependency> {
    let normalized = content.replace("\r\n", "\n");
    let mut deps = Vec::new();

    // Find ```skills blocks
    let skills_block_re = regex::Regex::new(r"```skills\n([\s\S]*?)```").unwrap();
    let use_re = regex::Regex::new(r"@use\s+([\w-]+)\s*\[([^\]]*)\]").unwrap();

    for block_match in skills_block_re.find_iter(&normalized) {
        let block = block_match.as_str();
        for cap in use_re.captures_iter(block) {
            let skill = cap[1].to_string();
            let patterns: Vec<String> = cap[2]
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            deps.push(SkillDependency { skill, patterns });
        }
    }

    deps
}

/// Infer skill category from name.
pub fn infer_category(name: &str) -> String {
    let categories: &[(&str, &[&str])] = &[
        (
            "Core",
            &[
                "explore",
                "plan",
                "research",
                "execute",
                "todo-manager",
                "brainstorming",
                "agent-teams",
            ],
        ),
        (
            "Quality & Testing",
            &[
                "test",
                "review",
                "receiving-code-review",
                "lint",
                "security",
                "performance",
                "quality-gate",
                "reality-check",
                "tech-debt",
                "cleanup",
            ],
        ),
        (
            "Lifecycle",
            &[
                "debug",
                "document",
                "migrate",
                "refactor",
                "git",
                "git-worktree",
                "deploy",
            ],
        ),
        (
            "Architecture",
            &["ddd-hexagonal", "api-design", "atomic-design"],
        ),
        ("UI Frameworks", &["mui", "react", "nextjs", "web-design"]),
        ("Enterprise", &["incident", "estimate", "onboard"]),
        (
            "AI & Integration",
            &[
                "1password",
                "blacksmith",
                "cerebras",
                "cloudflare-tunnel",
                "codex",
                "doppler",
                "edge-cdp",
                "gemini-video",
                "gooddata",
                "linear",
                "loom",
                "internet",
                "sentinel",
                "sequential-thinking",
                "ssh",
                "browserbase",
                "browserbase-tester",
                "tailscale",
                "traffic-light",
            ],
        ),
        (
            "Meta",
            &[
                "skill-router",
                "skill-registry",
                "skill-creator",
                "skill-sync",
                "auto-setup",
                "claude-md",
                "claude-code-guide",
                "mcp-manager",
                "quickstart",
                "telemetry",
                "react-hooks-linter",
                "project-setup",
                "session-resume",
                "chrome-extension-loader",
            ],
        ),
    ];

    for (cat, names) in categories {
        if names.contains(&name) {
            return (*cat).to_string();
        }
    }
    "Other".to_string()
}

/// Parse sentinel hooks.toml — simple `[[hooks]]` array-of-tables parser.
pub fn parse_hooks_toml(content: &str) -> Vec<Hook> {
    let normalized = content.replace("\r\n", "\n");
    let mut hooks = Vec::new();

    // Split on [[hooks]] headers
    let blocks: Vec<&str> = normalized.split("[[hooks]]").skip(1).collect();

    for block in blocks {
        let mut id = String::new();
        let mut event = String::new();
        let mut description = String::new();
        let mut depends_on: Vec<String> = Vec::new();
        let mut has_api_call = false;
        let mut matcher: Vec<String> = Vec::new();

        for line in block.lines() {
            // Strip comments
            let trimmed = if let Some(pos) = line.find('#') {
                line[..pos].trim()
            } else {
                line.trim()
            };
            if trimmed.is_empty() || trimmed.starts_with('[') {
                continue;
            }

            // Parse key = value
            if let Some(eq_pos) = trimmed.find('=') {
                let key = trimmed[..eq_pos].trim();
                let val = trimmed[eq_pos + 1..].trim();

                match key {
                    "id" | "event" | "description" => {
                        let unquoted = val.trim_matches('"');
                        match key {
                            "id" => id = unquoted.to_string(),
                            "event" => event = unquoted.to_string(),
                            "description" => description = unquoted.to_string(),
                            _ => {}
                        }
                    }
                    "has_api_call" => {
                        has_api_call = val == "true";
                    }
                    "depends_on" | "matcher" => {
                        // Parse TOML array: ["a", "b"] or []
                        if let Some(arr_content) =
                            val.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
                        {
                            let items: Vec<String> = arr_content
                                .split(',')
                                .map(|s| s.trim().trim_matches('"').to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                            match key {
                                "depends_on" => depends_on = items,
                                "matcher" => matcher = items,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if !id.is_empty() {
            hooks.push(Hook {
                name: id.clone(),
                file: format!("sentinel::{id}"),
                event,
                matcher: matcher.join(", "),
                description,
                depends_on,
                has_api_call,
                engine: "sentinel".to_string(),
            });
        }
    }

    hooks
}

// ---------------------------------------------------------------------------
// Full marketplace scan
// ---------------------------------------------------------------------------

/// Scan the entire marketplace directory and return a structured snapshot.
///
/// `root_dir` should be `~/.claude/` (where skills/, agents/, commands/ live)
/// or the marketplace repo root (for repo-level scanning).
pub fn scan_marketplace(root_dir: &Path) -> MarketplaceSnapshot {
    let start = Instant::now();

    // Read marketplace.json
    let mp_json_path = root_dir.join("marketplace.json");
    let mp_data: serde_json::Value = fs::read_to_string(&mp_json_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let version = mp_data
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();
    let description = mp_data
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // --- Skills ---
    let skills = scan_skills(root_dir, &mp_data);

    // --- Hooks ---
    let hooks = scan_hooks(root_dir);

    // --- Agents ---
    let agents = scan_agents(root_dir, &mp_data);

    // --- Commands ---
    let commands = scan_commands(root_dir);

    // --- MCP Servers ---
    let mcp_servers = scan_mcp_servers(&mp_data);

    // --- Dependency Graph ---
    let dependency_edges: Vec<DependencyEdge> = skills
        .iter()
        .flat_map(|skill| {
            skill.dependencies.iter().map(|dep| DependencyEdge {
                from: skill.name.clone(),
                to: dep.skill.clone(),
                patterns: dep.patterns.clone(),
            })
        })
        .collect();

    // --- Validation ---
    let validation = run_validation(
        root_dir,
        &mp_data,
        &skills,
        &hooks,
        &agents,
        &commands,
        &mcp_servers,
        start,
    );

    // --- Counts ---
    let home_dir = root_dir.parent().unwrap_or(root_dir).to_path_buf();

    let counts = ComponentCounts {
        skills: skills.len(),
        hooks: hooks.len(),
        agents: agents.len(),
        commands: commands.len(),
        mcp_servers: mcp_servers.len(),
        mcp_repos: count_repos_with_suffix(&home_dir, "-mcp-rust"),
        cli_repos: count_repos_with_suffix(&home_dir, "-cli-rust"),
    };

    MarketplaceSnapshot {
        version,
        description,
        skills,
        hooks,
        agents,
        commands,
        mcp_servers,
        dependency_edges,
        validation,
        counts,
    }
}

/// Scan skills directory.
fn scan_skills(root_dir: &Path, mp_data: &serde_json::Value) -> Vec<Skill> {
    let skills_dir = root_dir.join("skills");
    let mut skills = Vec::new();

    let Ok(entries) = fs::read_dir(&skills_dir) else {
        return skills;
    };

    let mut skill_dirs: Vec<String> = entries
        .filter_map(std::result::Result::ok)
        .filter(|e| {
            e.file_type().is_ok_and(|ft| ft.is_dir())
                && !e.file_name().to_string_lossy().starts_with('_')
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    skill_dirs.sort();

    let mp_skills = mp_data
        .get("skills")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let sub_module_dirs = [
        "languages",
        "projects",
        "hosts",
        "patterns",
        "types",
        "reference",
        "protocols",
        "routing",
        "orchestration",
    ];

    for name in &skill_dirs {
        let skill_md_path = skills_dir.join(name).join("SKILL.md");
        let skill_md_str = skill_md_path.to_string_lossy().to_string();

        if !skill_md_path.exists() {
            skills.push(Skill {
                name: name.clone(),
                version: "?".to_string(),
                description: "SKILL.md missing".to_string(),
                icon: String::new(),
                priority: None,
                allowed_tools: Vec::new(),
                dependencies: Vec::new(),
                category: "Unknown".to_string(),
                has_sub_modules: false,
                file_path: skill_md_str,
            });
            continue;
        }

        let content = fs::read_to_string(&skill_md_path).unwrap_or_default();
        let fm = parse_frontmatter(&content);
        let deps = extract_dependencies(&content);

        let has_sub_modules = sub_module_dirs
            .iter()
            .any(|d| skills_dir.join(name).join(d).exists());

        let mp_skill = mp_skills
            .iter()
            .find(|s| s.get("name").and_then(|v| v.as_str()) == Some(name.as_str()));

        let priority = mp_skill
            .and_then(|s| s.get("priority"))
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as u32);

        let allowed_tools: Vec<String> = fm
            .get("allowed-tools")
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        skills.push(Skill {
            name: name.clone(),
            version: fm
                .get("version")
                .cloned()
                .unwrap_or_else(|| "0.0.0".to_string()),
            description: fm.get("description").cloned().unwrap_or_default(),
            icon: fm.get("icon").cloned().unwrap_or_default(),
            priority,
            allowed_tools,
            dependencies: deps,
            category: infer_category(name),
            has_sub_modules,
            file_path: skill_md_str,
        });
    }

    skills
}

/// Scan hooks from sentinel hooks.toml.
///
/// `root_dir` is `~/.claude/` — hooks.toml lives at `root_dir/sentinel/config/hooks.toml`.
fn scan_hooks(root_dir: &Path) -> Vec<Hook> {
    let hooks_toml_path = root_dir.join("sentinel").join("config").join("hooks.toml");

    fs::read_to_string(&hooks_toml_path)
        .ok()
        .map(|content| parse_hooks_toml(&content))
        .unwrap_or_default()
}

/// Scan agents from marketplace.json + filesystem.
fn scan_agents(root_dir: &Path, mp_data: &serde_json::Value) -> Vec<Agent> {
    let agents_dir = root_dir.join("agents");
    let mp_agents = mp_data
        .get("agents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    mp_agents
        .iter()
        .map(|a| {
            let name = a
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let model = a
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let file = a
                .get("file")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let description = if file.is_empty() {
                String::new()
            } else {
                let agent_path = agents_dir.join(&file);
                fs::read_to_string(&agent_path)
                    .ok()
                    .map(|content| {
                        let fm = parse_frontmatter(&content);
                        fm.get("description").cloned().unwrap_or_default()
                    })
                    .unwrap_or_default()
            };

            Agent {
                name,
                model,
                file,
                description,
            }
        })
        .collect()
}

/// Scan slash commands from filesystem.
fn scan_commands(root_dir: &Path) -> Vec<CommandDef> {
    let commands_dir = root_dir.join("commands");
    let Ok(entries) = fs::read_dir(&commands_dir) else {
        return Vec::new();
    };

    let mut command_files: Vec<String> = entries
        .filter_map(std::result::Result::ok)
        .filter(|e| {
            e.file_type().is_ok_and(|ft| ft.is_file())
                && e.file_name().to_string_lossy().ends_with(".md")
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    command_files.sort();

    command_files
        .iter()
        .map(|file| {
            let content = fs::read_to_string(commands_dir.join(file)).unwrap_or_default();
            let fm = parse_frontmatter(&content);

            let allowed_tools: Vec<String> = fm
                .get("allowed-tools")
                .map(|s| {
                    s.split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                })
                .unwrap_or_default();

            CommandDef {
                name: file.trim_end_matches(".md").to_string(),
                description: fm.get("description").cloned().unwrap_or_default(),
                allowed_tools,
                argument_hint: fm.get("argument-hint").cloned().unwrap_or_default(),
            }
        })
        .collect()
}

/// Scan MCP servers from marketplace.json.
fn scan_mcp_servers(mp_data: &serde_json::Value) -> Vec<McpServer> {
    mp_data
        .get("mcp")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|m| McpServer {
                    name: m
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    command: m
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    transport: m
                        .get("transport")
                        .and_then(|v| v.as_str())
                        .unwrap_or("stdio")
                        .to_string(),
                    optional: m
                        .get("optional")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Validation (ported from scanner.cjs runValidation)
// ---------------------------------------------------------------------------

/// Run all validation rules and return a report.
#[allow(clippy::too_many_arguments)]
fn run_validation(
    root_dir: &Path,
    mp_data: &serde_json::Value,
    skills: &[Skill],
    hooks: &[Hook],
    agents: &[Agent],
    _commands: &[CommandDef],
    mcp_servers: &[McpServer],
    start: Instant,
) -> ValidationReport {
    let actual_counts: HashMap<&str, usize> = HashMap::from([
        ("skills", skills.len()),
        ("hooks", hooks.len()),
        ("agents", agents.len()),
        ("mcpServers", mcp_servers.len()),
    ]);

    let mut results = Vec::new();
    validate_count_consistency(mp_data, &actual_counts, &mut results);
    validate_file_cross_references(root_dir, mp_data, skills, hooks, &mut results);
    validate_frontmatter(skills, &mut results);
    validate_dependency_graph(skills, &mut results);
    validate_doc_counts(root_dir, &actual_counts, &mut results);
    validate_skill_banners(root_dir, skills, &mut results);

    let duration_ms = start.elapsed().as_millis() as u64;
    let passed = results.iter().filter(|r| r.status == "pass").count();
    let failed = results.iter().filter(|r| r.status == "fail").count();
    let warned = results.iter().filter(|r| r.status == "warn").count();

    ValidationReport {
        timestamp: chrono::Utc::now().to_rfc3339(),
        duration_ms,
        passed,
        failed,
        warned,
        results,
    }
}

/// Category 1: check that counts in marketplace.json description match filesystem actuals.
fn validate_count_consistency(
    mp_data: &serde_json::Value,
    actual_counts: &HashMap<&str, usize>,
    results: &mut Vec<ValidationResult>,
) {
    let desc = mp_data
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let count_patterns: &[(&str, &str)] = &[
        ("skills", r"(\d+)\s+skills"),
        ("agents", r"(\d+)\s+agents"),
        ("hooks", r"(\d+)\s+hooks"),
        ("mcpServers", r"(\d+)\s+MCP\s+servers"),
    ];

    for &(key, pattern) in count_patterns {
        let re = regex::Regex::new(pattern).unwrap();
        if let Some(cap) = re.captures(desc) {
            let expected: usize = cap[1].parse().unwrap_or(0);
            let actual = actual_counts.get(key).copied().unwrap_or(0);
            if actual == expected {
                results.push(ValidationResult {
                    category: "Count Consistency".to_string(),
                    rule: format!("marketplace.json description {key}"),
                    status: "pass".to_string(),
                    message: format!("{key}: {actual} matches description"),
                    expected: None,
                    actual: None,
                });
            } else {
                results.push(ValidationResult {
                    category: "Count Consistency".to_string(),
                    rule: format!("marketplace.json description {key}"),
                    status: "fail".to_string(),
                    message: format!("{key}: description says {expected}, filesystem has {actual}"),
                    expected: Some(expected),
                    actual: Some(actual),
                });
            }
        }
    }
}

/// Category 2: cross-reference registered skills/hooks/agents against the filesystem.
fn validate_file_cross_references(
    root_dir: &Path,
    mp_data: &serde_json::Value,
    skills: &[Skill],
    hooks: &[Hook],
    results: &mut Vec<ValidationResult>,
) {
    let registered_skill_names: HashSet<String> = mp_data
        .get("skills")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.get("name").and_then(|v| v.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let actual_skill_names: HashSet<String> = skills.iter().map(|s| s.name.clone()).collect();

    // Every dir in skills/ → registered
    for name in &actual_skill_names {
        if registered_skill_names.contains(name) {
            results.push(ValidationResult {
                category: "File Cross-Reference".to_string(),
                rule: format!("skill dir {name} registered"),
                status: "pass".to_string(),
                message: format!("{name} is registered in marketplace.json"),
                expected: None,
                actual: None,
            });
        } else {
            results.push(ValidationResult {
                category: "File Cross-Reference".to_string(),
                rule: format!("skill dir {name} registered"),
                status: "fail".to_string(),
                message: format!("{name} exists on disk but NOT in marketplace.json"),
                expected: None,
                actual: None,
            });
        }
    }

    // Every registered skill → has SKILL.md
    for name in &registered_skill_names {
        let skill_md = root_dir.join("skills").join(name).join("SKILL.md");
        if skill_md.exists() {
            results.push(ValidationResult {
                category: "File Cross-Reference".to_string(),
                rule: format!("registered skill {name} exists"),
                status: "pass".to_string(),
                message: format!("{name}/SKILL.md exists"),
                expected: None,
                actual: None,
            });
        } else {
            results.push(ValidationResult {
                category: "File Cross-Reference".to_string(),
                rule: format!("registered skill {name} exists"),
                status: "fail".to_string(),
                message: format!("{name}/SKILL.md missing from filesystem"),
                expected: None,
                actual: None,
            });
        }
    }

    // Sentinel hooks.toml
    let hooks_toml_path = root_dir.join("sentinel").join("config").join("hooks.toml");
    if hooks_toml_path.exists() {
        results.push(ValidationResult {
            category: "File Cross-Reference".to_string(),
            rule: "sentinel hooks.toml exists".to_string(),
            status: "pass".to_string(),
            message: format!("hooks.toml found with {} hooks", hooks.len()),
            expected: None,
            actual: None,
        });
    } else {
        results.push(ValidationResult {
            category: "File Cross-Reference".to_string(),
            rule: "sentinel hooks.toml exists".to_string(),
            status: "fail".to_string(),
            message: "hooks.toml not found — sentinel engine not configured".to_string(),
            expected: None,
            actual: None,
        });
    }

    // Every hook has required fields
    for h in hooks {
        if !h.name.is_empty() && !h.event.is_empty() {
            results.push(ValidationResult {
                category: "File Cross-Reference".to_string(),
                rule: format!("hook {} valid", h.name),
                status: "pass".to_string(),
                message: format!("{} ({}) registered in sentinel", h.name, h.event),
                expected: None,
                actual: None,
            });
        } else {
            results.push(ValidationResult {
                category: "File Cross-Reference".to_string(),
                rule: "hook entry incomplete".to_string(),
                status: "fail".to_string(),
                message: "Hook missing id or event in hooks.toml".to_string(),
                expected: None,
                actual: None,
            });
        }
    }

    // Every agent in marketplace.json → file exists
    let agents_dir = root_dir.join("agents");
    if let Some(mp_agents) = mp_data.get("agents").and_then(|v| v.as_array()) {
        for a in mp_agents {
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let file = a.get("file").and_then(|v| v.as_str()).unwrap_or("");
            if !file.is_empty() && agents_dir.join(file).exists() {
                results.push(ValidationResult {
                    category: "File Cross-Reference".to_string(),
                    rule: format!("agent {name} exists"),
                    status: "pass".to_string(),
                    message: format!("{file} exists"),
                    expected: None,
                    actual: None,
                });
            } else {
                results.push(ValidationResult {
                    category: "File Cross-Reference".to_string(),
                    rule: format!("agent {name} exists"),
                    status: "fail".to_string(),
                    message: format!("{file} missing from agents/"),
                    expected: None,
                    actual: None,
                });
            }
        }
    }
}

/// Category 3: check that each skill has a valid semver version and a unique priority.
fn validate_frontmatter(skills: &[Skill], results: &mut Vec<ValidationResult>) {
    let semver_re = regex::Regex::new(r"^\d+\.\d+\.\d+$").unwrap();
    let mut priority_map: HashMap<u32, String> = HashMap::new();

    for skill in skills {
        if skill.version == "?" || skill.version.is_empty() {
            results.push(ValidationResult {
                category: "Frontmatter".to_string(),
                rule: format!("{} has version", skill.name),
                status: "fail".to_string(),
                message: format!("{} missing version in frontmatter", skill.name),
                expected: None,
                actual: None,
            });
        } else if !semver_re.is_match(&skill.version) {
            results.push(ValidationResult {
                category: "Frontmatter".to_string(),
                rule: format!("{} valid semver", skill.name),
                status: "warn".to_string(),
                message: format!(
                    "{} version \"{}\" is not valid semver",
                    skill.name, skill.version
                ),
                expected: None,
                actual: None,
            });
        }

        if let Some(priority) = skill.priority {
            if let Some(existing) = priority_map.get(&priority) {
                results.push(ValidationResult {
                    category: "Frontmatter".to_string(),
                    rule: format!("priority {priority} unique"),
                    status: "fail".to_string(),
                    message: format!(
                        "Priority {priority} shared by {existing} and {}",
                        skill.name
                    ),
                    expected: None,
                    actual: None,
                });
            }
            priority_map.insert(priority, skill.name.clone());
        }
    }
}

/// Category 4: verify every skill dependency resolves to an existing skill (no dangling refs or
/// self-references).
fn validate_dependency_graph(skills: &[Skill], results: &mut Vec<ValidationResult>) {
    let all_skill_names: HashSet<&str> = skills.iter().map(|s| s.name.as_str()).collect();

    for skill in skills {
        for dep in &skill.dependencies {
            if !all_skill_names.contains(dep.skill.as_str()) {
                results.push(ValidationResult {
                    category: "Dependencies".to_string(),
                    rule: format!("{} → {}", skill.name, dep.skill),
                    status: "fail".to_string(),
                    message: format!(
                        "{} depends on {} which doesn't exist",
                        skill.name, dep.skill
                    ),
                    expected: None,
                    actual: None,
                });
            } else if dep.skill == skill.name {
                results.push(ValidationResult {
                    category: "Dependencies".to_string(),
                    rule: format!("{} self-reference", skill.name),
                    status: "warn".to_string(),
                    message: format!("{} references itself", skill.name),
                    expected: None,
                    actual: None,
                });
            }
        }
    }
}

/// Category 5: scan CLAUDE.md and README.md for stale component count mentions.
fn validate_doc_counts(
    root_dir: &Path,
    actual_counts: &HashMap<&str, usize>,
    results: &mut Vec<ValidationResult>,
) {
    let docs_to_scan = ["CLAUDE.md", "README.md"];
    let total_count_patterns: &[(&str, &str, usize)] = &[
        ("skills", r"(\d+)\s+skills", 10),
        ("hooks", r"(\d+)\s+hooks", 10),
        ("agents", r"(\d+)\s+agents", 3),
        ("mcpServers", r"(\d+)\s+MCP\s+servers", 3),
    ];

    for doc_file in &docs_to_scan {
        let file_path = root_dir.join(doc_file);
        let Ok(content) = fs::read_to_string(&file_path) else {
            continue;
        };

        for &(key, pattern, min_value) in total_count_patterns {
            let re = regex::Regex::new(pattern).unwrap();
            for cap in re.captures_iter(&content) {
                let found: usize = cap[1].parse().unwrap_or(0);
                if found < min_value {
                    continue;
                }
                if let Some(&actual) = actual_counts.get(key) {
                    if found != actual {
                        results.push(ValidationResult {
                            category: "Documentation".to_string(),
                            rule: format!("{doc_file} {key} count"),
                            status: "fail".to_string(),
                            message: format!(
                                "{doc_file} says \"{}\" but actual is {actual}",
                                &cap[0]
                            ),
                            expected: Some(actual),
                            actual: Some(found),
                        });
                    }
                }
            }
        }
    }
}

/// Category 6: warn for any skill whose SKILL.md lacks an `## Activation Banner` section.
///
/// `skill_router` falls back to a bare detection message for skills without a banner, so
/// this is flagged as `warn` (not `fail`) to track migration progress without breaking CI.
fn validate_skill_banners(root_dir: &Path, skills: &[Skill], results: &mut Vec<ValidationResult>) {
    for skill in skills {
        let skill_md = root_dir.join("skills").join(&skill.name).join("SKILL.md");
        let Ok(content) = fs::read_to_string(&skill_md) else {
            continue;
        };
        if content.contains("## Activation Banner") {
            results.push(ValidationResult {
                category: "Skill Banner".to_string(),
                rule: format!("{} has activation banner", skill.name),
                status: "pass".to_string(),
                message: format!("{}/SKILL.md has `## Activation Banner` section", skill.name),
                expected: None,
                actual: None,
            });
        } else {
            results.push(ValidationResult {
                category: "Skill Banner".to_string(),
                rule: format!("{} has activation banner", skill.name),
                status: "warn".to_string(),
                message: format!(
                    "{}/SKILL.md missing `## Activation Banner` — router will fall back to a bare detection message",
                    skill.name
                ),
                expected: None,
                actual: None,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Extended counts (includes non-sentinel local counts for sync-counts)
// ---------------------------------------------------------------------------

/// Extended counts including local-only filesystem counts not tracked by sentinel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtendedCounts {
    #[serde(flatten)]
    pub core: ComponentCounts,
    pub scripts: usize,
    pub docs: usize,
    pub templates: usize,
    pub browserbase_tools: usize,
}

/// Count extended marketplace components (core + local extras).
pub fn count_extended(root_dir: &Path) -> ExtendedCounts {
    let core = count_components(root_dir);
    let scripts = count_files_with_ext(&root_dir.join("scripts"), ".js");
    let docs = count_files_with_ext(&root_dir.join("docs"), ".md");
    let templates = count_all_non_hidden(&root_dir.join("templates"));
    let browserbase_tools = parse_browserbase_tools(root_dir);

    ExtendedCounts {
        core,
        scripts,
        docs,
        templates,
        browserbase_tools,
    }
}

/// Count all non-hidden entries in a directory.
fn count_all_non_hidden(dir: &Path) -> usize {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter_map(std::result::Result::ok)
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .count()
    })
}

/// Parse Browserbase tool count from marketplace.json MCP description.
fn parse_browserbase_tools(root_dir: &Path) -> usize {
    let mp_path = root_dir.join("marketplace.json");
    let Ok(content) = fs::read_to_string(&mp_path) else {
        return 48; // default
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) else {
        return 48;
    };

    data.get("mcp")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|m| matches!(m.get("name").and_then(|n| n.as_str()), Some("browserbase")))
        })
        .and_then(|entry| entry.get("description").and_then(|d| d.as_str()))
        .and_then(|desc| {
            let re = regex::Regex::new(r"(\d+)\s*tools").ok()?;
            re.captures(desc).and_then(|cap| cap[1].parse().ok())
        })
        .unwrap_or(48)
}

// ---------------------------------------------------------------------------
// Sync counts — port of sync-counts.js
// ---------------------------------------------------------------------------

/// Report from a sync-counts operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncCountsReport {
    pub files_changed: Vec<String>,
    pub dry_run: bool,
}

/// Synchronize component counts across all marketplace text files.
///
/// Walks all `.md`, `.json`, `.js`, `.cjs`, `.ts`, `.toml` files under `root_dir`,
/// applies universal regex replacements for count patterns, plus targeted
/// replacements for specific files (marketplace.json, README.md, CLAUDE.md, etc.).
pub fn sync_counts(root_dir: &Path, dry_run: bool) -> SyncCountsReport {
    let ext = count_extended(root_dir);
    let c = &ext.core;

    let mut changed_files: Vec<String> = Vec::new();

    apply_universal_patterns(root_dir, c, &ext, dry_run, &mut changed_files);
    apply_targeted_updates(root_dir, c, &ext, dry_run, &mut changed_files);

    changed_files.sort();
    changed_files.dedup();

    SyncCountsReport {
        files_changed: changed_files,
        dry_run,
    }
}

/// Build the universal (pattern, replacement) list applied to every text file.
///
/// The skills pattern is returned separately because it requires a closure
/// (capture-group prefix preservation) that doesn't fit the plain `&str` replacement API.
fn build_universal_patterns(
    c: &ComponentCounts,
    ext: &ExtendedCounts,
) -> Vec<(regex::Regex, String)> {
    // Note: regex crate doesn't support lookahead, so MCP server pattern
    // uses a simple match. The targeted updates handle specific files.
    vec![
        (
            regex::Regex::new(r"All \d+ hooks").unwrap(),
            format!("All {} hooks", c.hooks),
        ),
        (
            regex::Regex::new(r"\d+ hooks \(sentinel").unwrap(),
            format!("{} hooks (sentinel", c.hooks),
        ),
        (
            regex::Regex::new(r"\d+ MCP servers\b").unwrap(),
            format!("{} MCP servers", c.mcp_servers),
        ),
        (
            regex::Regex::new(r"Browserbase MCP \(\d+ tools\)").unwrap(),
            format!("Browserbase MCP ({} tools)", ext.browserbase_tools),
        ),
        (
            regex::Regex::new(r"browser automation \(\d+ MCP tools\)").unwrap(),
            format!("browser automation ({} MCP tools)", ext.browserbase_tools),
        ),
    ]
}

/// Walk every text file under `root_dir` and apply universal count-replacement patterns.
fn apply_universal_patterns(
    root_dir: &Path,
    c: &ComponentCounts,
    ext: &ExtendedCounts,
    dry_run: bool,
    changed_files: &mut Vec<String>,
) {
    let universal = build_universal_patterns(c, ext);

    // Skills pattern needs special handling (preserve prefix)
    let skills_re =
        regex::Regex::new(r"((?:All |all |ALL |\(|marketplace \())\d+( skills)").unwrap();

    let skip_dirs: HashSet<&str> = ["node_modules", ".git", "target", "dist", ".next"]
        .iter()
        .copied()
        .collect();
    let text_exts: HashSet<&str> = ["md", "json", "js", "cjs", "ts", "toml"]
        .iter()
        .copied()
        .collect();

    walk_text_files(root_dir, &skip_dirs, &text_exts, &mut |path| {
        let rel = path.strip_prefix(root_dir).unwrap_or(path);
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        // Skip changelogs
        if rel_str.starts_with("CHANGELOG") {
            return;
        }

        let Ok(content) = fs::read_to_string(path) else {
            return;
        };
        let original = content.clone();
        let mut result = content;

        for (re, replacement) in &universal {
            if replacement.is_empty() {
                continue; // Skip the skills placeholder
            }
            result = re.replace_all(&result, replacement.as_str()).to_string();
        }

        // Apply skills pattern with prefix preservation
        result = skills_re
            .replace_all(&result, |caps: &regex::Captures| {
                format!("{}{}{}", &caps[1], c.skills, &caps[2])
            })
            .to_string();

        if result != original {
            if !dry_run {
                let _ = fs::write(path, &result);
            }
            changed_files.push(rel_str);
        }
    });
}

/// Apply targeted regex replacements to specific well-known files.
///
/// Each file gets a hand-crafted set of patterns that capture file-specific
/// structure (section headings, JSON keys, table rows, etc.) where the universal
/// walk would either miss or over-replace.
fn apply_targeted_updates(
    root_dir: &Path,
    c: &ComponentCounts,
    ext: &ExtendedCounts,
    dry_run: bool,
    changed_files: &mut Vec<String>,
) {
    let desc_line = format!(
        "{} skills + {} agents + 1 sentinel engine ({} hooks) + {} MCP servers ({} repos) + {} CLIs + hot-reload via Vulcan mcp-router for the full software development lifecycle",
        c.skills, c.agents, c.hooks, c.mcp_servers, c.mcp_repos, c.cli_repos
    );

    // marketplace.json description
    targeted_update(
        root_dir,
        "marketplace.json",
        dry_run,
        changed_files,
        &[(
            r#""description":\s*"[^"]*""#,
            &format!(r#""description": "{desc_line}""#),
        )],
    );

    // .claude-plugin/plugin.json
    targeted_update(
        root_dir,
        ".claude-plugin/plugin.json",
        dry_run,
        changed_files,
        &[(
            r#""description":\s*"[^"]*""#,
            &format!(r#""description": "{desc_line}""#),
        )],
    );

    // .claude-plugin/marketplace.json
    targeted_update(
        root_dir,
        ".claude-plugin/marketplace.json",
        dry_run,
        changed_files,
        &[(
            r#""summary":\s*"[^"]*""#,
            &format!(
                r#""summary": "Complete dev lifecycle: {} skills, {} agents, {} hooks (sentinel engine), {} MCP servers ({} repos), and {} CLIs""#,
                c.skills, c.agents, c.hooks, c.mcp_servers, c.mcp_repos, c.cli_repos
            ),
        )],
    );

    // README.md
    targeted_update(
        root_dir,
        "README.md",
        dry_run,
        changed_files,
        &[
            (
                r"> \*\*\d+ skills \+ \d+ agents \+ 1 sentinel engine \(\d+ hooks\) \+ \d+ MCP servers\*\*",
                &format!(
                    "> **{} skills + {} agents + 1 sentinel engine ({} hooks) + {} MCP servers**",
                    c.skills, c.agents, c.hooks, c.mcp_servers
                ),
            ),
            (
                r"## Available Skills \(\d+ Total\)",
                &format!("## Available Skills ({} Total)", c.skills),
            ),
            (
                r"## Custom Agents \(\d+ Total\)",
                &format!("## Custom Agents ({} Total)", c.agents),
            ),
            (
                r"## Hooks \(\d+ Total",
                &format!("## Hooks ({} Total", c.hooks),
            ),
            (
                r"## MCP Integrations \(\d+ Total\)",
                &format!("## MCP Integrations ({} Total)", c.mcp_servers),
            ),
            (
                r"## Scripts \(\d+ Total\)",
                &format!("## Scripts ({} Total)", ext.scripts),
            ),
            (
                r"skills/\s+# \d+ skill directories.*",
                &format!("skills/                   # {} skill directories", c.skills),
            ),
            (
                r"agents/\s+# \d+ agent definitions.*",
                &format!(
                    "agents/                   # {} agent definitions (.md)",
                    c.agents
                ),
            ),
            (
                r"commands/\s+# \d+ slash commands.*",
                &format!(
                    "commands/                 # {} slash commands (.md)",
                    c.commands
                ),
            ),
            (
                r"scripts/\s+# \d+ utility scripts?.*",
                &format!(
                    "scripts/                  # {} utility scripts (.js)",
                    ext.scripts
                ),
            ),
            (
                r"templates/\s+# \d+ skill scaffolding.*",
                &format!(
                    "templates/                # {} skill scaffolding templates",
                    ext.templates
                ),
            ),
            (
                r"docs/\s+# \d+ documentation pages.*",
                &format!(
                    "docs/                     # {} documentation pages",
                    ext.docs
                ),
            ),
        ],
    );

    // CLAUDE.md (repo-level)
    targeted_update(
        root_dir,
        "CLAUDE.md",
        dry_run,
        changed_files,
        &[
            (
                r"> \*\*\d+ skills \+ \d+ agents \+ 1 sentinel engine \(\d+ hooks\) \+ \d+ MCP servers\*\*",
                &format!(
                    "> **{} skills + {} agents + 1 sentinel engine ({} hooks) + {} MCP servers**",
                    c.skills, c.agents, c.hooks, c.mcp_servers
                ),
            ),
            (
                r"skills/\s+<- \d+ skill directories.*",
                &format!(
                    "skills/                <- {} skill directories (SKILL.md each)",
                    c.skills
                ),
            ),
            (
                r"commands/\s+<- \d+ slash commands.*",
                &format!(
                    "commands/              <- {} slash commands (.md files)",
                    c.commands
                ),
            ),
            (
                r"agents/\s+<- \d+ agent definitions.*",
                &format!(
                    "agents/                <- {} agent definitions (.md files)",
                    c.agents
                ),
            ),
            (r"\| Skills \| \d+", &format!("| Skills | {}", c.skills)),
            (r"\| Hooks \| \d+", &format!("| Hooks | {}", c.hooks)),
            (r"\| Agents \| \d+", &format!("| Agents | {}", c.agents)),
            (
                r"\| MCP Servers \| \d+",
                &format!("| MCP Servers | {}", c.mcp_servers),
            ),
            (
                r"\| Commands \| \d+",
                &format!("| Commands | {}", c.commands),
            ),
            (
                r"\| Scripts \| \d+",
                &format!("| Scripts | {}", ext.scripts),
            ),
            (r"\| Docs \| \d+", &format!("| Docs | {}", ext.docs)),
            (
                r"\| Templates \| \d+",
                &format!("| Templates | {}", ext.templates),
            ),
        ],
    );

    // docs/marketplace-architecture.md
    targeted_update(
        root_dir,
        "docs/marketplace-architecture.md",
        dry_run,
        changed_files,
        &[(
            r"ecosystem of \d+ skills, \d+ agents, \d+ hooks, \d+ commands, and \d+ MCP servers",
            &format!(
                "ecosystem of {} skills, {} agents, {} hooks, {} commands, and {} MCP servers",
                c.skills, c.agents, c.hooks, c.commands, c.mcp_servers
            ),
        )],
    );
}

/// Apply targeted regex replacements to a specific file.
fn targeted_update(
    root_dir: &Path,
    rel_path: &str,
    dry_run: bool,
    changed_files: &mut Vec<String>,
    patterns: &[(&str, &str)],
) {
    let file_path = root_dir.join(rel_path);
    let Ok(content) = fs::read_to_string(&file_path) else {
        return;
    };
    let original = content.clone();
    let mut result = content;

    for &(pattern, replacement) in patterns {
        if let Ok(re) = regex::Regex::new(pattern) {
            result = re.replace_all(&result, replacement).to_string();
        }
    }

    if result != original {
        if !dry_run {
            let _ = fs::write(&file_path, &result);
        }
        let rel = rel_path.replace('\\', "/");
        if !changed_files.contains(&rel) {
            changed_files.push(rel);
        }
    }
}

/// Walk text files recursively, skipping specified directories.
fn walk_text_files(
    dir: &Path,
    skip_dirs: &HashSet<&str>,
    text_exts: &HashSet<&str>,
    callback: &mut dyn FnMut(&Path),
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.filter_map(std::result::Result::ok) {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            if !skip_dirs.contains(name_str.as_ref()) {
                walk_text_files(&entry.path(), skip_dirs, text_exts, callback);
            }
        } else if entry.file_type().is_ok_and(|ft| ft.is_file()) {
            if let Some(ext) = entry.path().extension().and_then(|e| e.to_str()) {
                if text_exts.contains(ext) {
                    callback(&entry.path());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Manifest generation — port of generate-manifest.js
// ---------------------------------------------------------------------------

/// A single file entry in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub hash: String,
    pub size: u64,
}

/// The generated manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub generated: String,
    pub repo: String,
    pub branch: String,
    pub file_count: usize,
    pub files: Vec<ManifestEntry>,
}

/// Generate a manifest.json with SHA-256 hashes for all syncable files.
pub fn generate_manifest(root_dir: &Path) -> Manifest {
    let sync_dirs: &[(&str, Option<&[&str]>)] = &[
        ("commands", Some(&[".md"])),
        ("skills", None), // all files
        ("agents", Some(&[".md"])),
        ("scripts", Some(&[".js"])),
        ("templates", Some(&[".md", ".template"])),
        ("docs", Some(&[".md"])),
    ];

    let exclude_patterns = [".git/", "node_modules/", ".DS_Store", "Thumbs.db"];

    let mut files: Vec<ManifestEntry> = Vec::new();

    for &(dir_name, extensions) in sync_dirs {
        let dir_path = root_dir.join(dir_name);
        if !dir_path.exists() {
            continue;
        }
        walk_manifest_files(
            &dir_path,
            dir_name,
            extensions,
            &exclude_patterns,
            &mut files,
        );
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));

    let file_count = files.len();

    let manifest = Manifest {
        version: 1,
        generated: chrono::Utc::now().to_rfc3339(),
        repo: "legatus-ai/claude-code-marketplace".to_string(),
        branch: "main".to_string(),
        file_count,
        files,
    };

    // Write manifest.json
    let out_path = root_dir.join("manifest.json");
    if let Ok(json) = serde_json::to_string_pretty(&manifest) {
        let _ = fs::write(&out_path, json + "\n");
    }

    manifest
}

/// Recursively walk a directory and collect manifest entries.
fn walk_manifest_files(
    dir: &Path,
    base_path: &str,
    extensions: Option<&[&str]>,
    exclude_patterns: &[&str],
    files: &mut Vec<ManifestEntry>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.filter_map(std::result::Result::ok) {
        let full_path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let rel_path = format!("{base_path}/{name_str}");

        // Check exclusions
        if exclude_patterns
            .iter()
            .any(|p| rel_path.contains(p) || name_str.ends_with(p))
        {
            continue;
        }

        if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            walk_manifest_files(&full_path, &rel_path, extensions, exclude_patterns, files);
        } else if entry.file_type().is_ok_and(|ft| ft.is_file()) {
            // Check extension filter
            if let Some(exts) = extensions {
                let has_ext = exts.iter().any(|ext| name_str.ends_with(ext));
                if !has_ext {
                    continue;
                }
            }

            if let Ok(content) = fs::read(&full_path) {
                let mut hasher = Sha256::new();
                hasher.update(&content);
                let hash = format!("{:x}", hasher.finalize());

                files.push(ManifestEntry {
                    path: rel_path.replace('\\', "/"),
                    hash,
                    size: content.len() as u64,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ---------------------------------------------------------------------------
    // Fixture helpers
    // ---------------------------------------------------------------------------

    /// Build a minimal marketplace fixture directory under a TempDir.
    ///
    /// Returns `(TempDir, root_path)` — caller must hold `TempDir` alive.
    fn build_fixture_dir() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        // marketplace.json
        fs::write(
            root.join("marketplace.json"),
            r#"{
  "version": "1.0.0",
  "description": "2 skills + 1 agents + 1 sentinel engine (2 hooks) + 1 MCP servers",
  "skills": [
    {"name": "alpha", "priority": 1},
    {"name": "beta",  "priority": 2}
  ],
  "agents": [
    {"name": "my-agent", "model": "claude-3-5-sonnet", "file": "my-agent.md"}
  ],
  "mcp": [
    {"name": "linear", "command": "mcp-router --single linear-mcp", "transport": "stdio", "optional": false}
  ]
}"#,
        )
        .unwrap();

        // skills/alpha/SKILL.md
        let alpha_dir = root.join("skills").join("alpha");
        fs::create_dir_all(&alpha_dir).unwrap();
        fs::write(
            alpha_dir.join("SKILL.md"),
            r#"---
name: alpha
version: 1.2.3
description: Alpha skill
icon: A
---
## Activation Banner

```skills
@use beta [helper_fn]
```
"#,
        )
        .unwrap();

        // skills/beta/SKILL.md
        let beta_dir = root.join("skills").join("beta");
        fs::create_dir_all(&beta_dir).unwrap();
        fs::write(
            beta_dir.join("SKILL.md"),
            r#"---
name: beta
version: 2.0.0
description: Beta skill
icon: B
---
## Activation Banner
"#,
        )
        .unwrap();

        // agents/my-agent.md
        let agents_dir = root.join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(
            agents_dir.join("my-agent.md"),
            r#"---
description: My test agent
---
"#,
        )
        .unwrap();

        // sentinel/config/hooks.toml
        let hooks_cfg = root.join("sentinel").join("config");
        fs::create_dir_all(&hooks_cfg).unwrap();
        fs::write(
            hooks_cfg.join("hooks.toml"),
            r#"[[hooks]]
id = "skill_router"
event = "UserPromptSubmit"
description = "Route to skills"
depends_on = []
has_api_call = false
matcher = []

[[hooks]]
id = "git_hygiene"
event = "PreToolUse"
description = "Git checks"
depends_on = []
has_api_call = false
matcher = ["Edit", "Write"]
"#,
        )
        .unwrap();

        (tmp, root)
    }

    // ---------------------------------------------------------------------------
    // GOLDEN TESTS — scan_marketplace / run_validation
    // ---------------------------------------------------------------------------

    /// Golden: snapshot shape + key field values from scan_marketplace.
    #[test]
    fn golden_scan_marketplace_shape() {
        let (_tmp, root) = build_fixture_dir();
        let snap = scan_marketplace(&root);

        // Version + description come from marketplace.json
        assert_eq!(snap.version, "1.0.0");
        assert!(snap.description.contains("2 skills"));

        // Skills
        assert_eq!(snap.skills.len(), 2);
        let alpha = snap.skills.iter().find(|s| s.name == "alpha").unwrap();
        assert_eq!(alpha.version, "1.2.3");
        assert_eq!(alpha.description, "Alpha skill");
        assert_eq!(alpha.priority, Some(1));
        assert_eq!(alpha.has_sub_modules, false);
        // alpha depends on beta
        assert_eq!(alpha.dependencies.len(), 1);
        assert_eq!(alpha.dependencies[0].skill, "beta");
        assert_eq!(alpha.dependencies[0].patterns, vec!["helper_fn"]);

        let beta = snap.skills.iter().find(|s| s.name == "beta").unwrap();
        assert_eq!(beta.version, "2.0.0");
        assert!(beta.dependencies.is_empty());

        // Hooks
        assert_eq!(snap.hooks.len(), 2);
        let router = snap
            .hooks
            .iter()
            .find(|h| h.name == "skill_router")
            .unwrap();
        assert_eq!(router.event, "UserPromptSubmit");
        assert_eq!(router.engine, "sentinel");
        let hygiene = snap.hooks.iter().find(|h| h.name == "git_hygiene").unwrap();
        assert_eq!(hygiene.matcher, "Edit, Write");

        // Agents
        assert_eq!(snap.agents.len(), 1);
        assert_eq!(snap.agents[0].name, "my-agent");
        assert_eq!(snap.agents[0].description, "My test agent");

        // MCP servers
        assert_eq!(snap.mcp_servers.len(), 1);
        assert_eq!(snap.mcp_servers[0].name, "linear");
        assert_eq!(snap.mcp_servers[0].transport, "stdio");
        assert_eq!(snap.mcp_servers[0].optional, false);

        // Dependency edges
        assert_eq!(snap.dependency_edges.len(), 1);
        assert_eq!(snap.dependency_edges[0].from, "alpha");
        assert_eq!(snap.dependency_edges[0].to, "beta");

        // Counts
        assert_eq!(snap.counts.skills, 2);
        assert_eq!(snap.counts.hooks, 2);
        assert_eq!(snap.counts.agents, 1);
        assert_eq!(snap.counts.mcp_servers, 1);
    }

    /// Golden: validation rules fired + pass/fail/warn counts from run_validation.
    #[test]
    fn golden_validation_categories_and_counts() {
        let (_tmp, root) = build_fixture_dir();
        let snap = scan_marketplace(&root);
        let v = &snap.validation;

        // No failures expected with our well-formed fixture
        assert_eq!(
            v.failed,
            0,
            "Unexpected failures: {:?}",
            v.results
                .iter()
                .filter(|r| r.status == "fail")
                .collect::<Vec<_>>()
        );

        // Count Consistency: marketplace.json description mentions "2 skills", "2 hooks",
        // "1 agents", "1 MCP servers" — all match actuals, so all should pass.
        let count_checks: Vec<_> = v
            .results
            .iter()
            .filter(|r| r.category == "Count Consistency")
            .collect();
        assert!(
            !count_checks.is_empty(),
            "Expected Count Consistency checks"
        );
        assert!(
            count_checks.iter().all(|r| r.status == "pass"),
            "Some Count Consistency checks failed: {:?}",
            count_checks
        );

        // File Cross-Reference: alpha + beta are both registered + have SKILL.md
        let cross_ref: Vec<_> = v
            .results
            .iter()
            .filter(|r| r.category == "File Cross-Reference")
            .collect();
        assert!(!cross_ref.is_empty());
        assert!(
            cross_ref.iter().all(|r| r.status == "pass"),
            "Some File Cross-Reference checks failed: {:?}",
            cross_ref
        );

        // Frontmatter: alpha=1.2.3 and beta=2.0.0 are valid semver → no fails
        let fm_checks: Vec<_> = v
            .results
            .iter()
            .filter(|r| r.category == "Frontmatter")
            .collect();
        // No version failures (both are valid semver)
        assert!(
            fm_checks.iter().all(|r| r.status != "fail"),
            "Unexpected Frontmatter failures: {:?}",
            fm_checks
        );

        // Dependencies: alpha→beta is valid (beta exists); no self-reference
        let dep_checks: Vec<_> = v
            .results
            .iter()
            .filter(|r| r.category == "Dependencies")
            .collect();
        // alpha→beta is valid — no results emitted for valid cross-deps (only fail/warn emitted)
        assert!(
            dep_checks.iter().all(|r| r.status != "fail"),
            "Unexpected dep failures: {:?}",
            dep_checks
        );

        // Skill Banner: both alpha and beta have "## Activation Banner"
        let banner_checks: Vec<_> = v
            .results
            .iter()
            .filter(|r| r.category == "Skill Banner")
            .collect();
        assert_eq!(banner_checks.len(), 2);
        assert!(
            banner_checks.iter().all(|r| r.status == "pass"),
            "Expected all banner checks to pass: {:?}",
            banner_checks
        );

        // Summary counts
        assert!(v.passed > 0);
        assert_eq!(v.failed, 0);
    }

    /// Golden: validation correctly detects a missing SKILL.md (fail status).
    #[test]
    fn golden_validation_missing_skill_md_emits_fail() {
        let (_tmp, root) = build_fixture_dir();
        // Register a third skill in marketplace.json that doesn't exist on disk
        fs::write(
            root.join("marketplace.json"),
            r#"{
  "version": "1.0.0",
  "description": "2 skills + 1 agents + 1 sentinel engine (2 hooks) + 1 MCP servers",
  "skills": [
    {"name": "alpha", "priority": 1},
    {"name": "beta",  "priority": 2},
    {"name": "ghost", "priority": 3}
  ],
  "agents": [
    {"name": "my-agent", "model": "claude-3-5-sonnet", "file": "my-agent.md"}
  ],
  "mcp": [
    {"name": "linear", "command": "mcp-router --single linear-mcp", "transport": "stdio", "optional": false}
  ]
}"#,
        )
        .unwrap();

        let snap = scan_marketplace(&root);
        let v = &snap.validation;

        // Should have at least one fail for "ghost" not existing on disk
        let ghost_fail = v
            .results
            .iter()
            .find(|r| r.status == "fail" && r.message.contains("ghost"));
        assert!(
            ghost_fail.is_some(),
            "Expected a fail for ghost skill missing from disk"
        );
    }

    /// Golden: validation detects duplicate priorities (fail status).
    #[test]
    fn golden_validation_duplicate_priority_emits_fail() {
        let (_tmp, root) = build_fixture_dir();
        // Give alpha and beta the same priority
        fs::write(
            root.join("marketplace.json"),
            r#"{
  "version": "1.0.0",
  "description": "2 skills + 1 agents + 1 sentinel engine (2 hooks) + 1 MCP servers",
  "skills": [
    {"name": "alpha", "priority": 5},
    {"name": "beta",  "priority": 5}
  ],
  "agents": [
    {"name": "my-agent", "model": "claude-3-5-sonnet", "file": "my-agent.md"}
  ],
  "mcp": [
    {"name": "linear", "command": "mcp-router --single linear-mcp", "transport": "stdio", "optional": false}
  ]
}"#,
        )
        .unwrap();

        let snap = scan_marketplace(&root);
        let v = &snap.validation;
        let dup_fail = v
            .results
            .iter()
            .find(|r| r.status == "fail" && r.category == "Frontmatter" && r.message.contains("5"));
        assert!(
            dup_fail.is_some(),
            "Expected a fail for duplicate priority 5"
        );
    }

    /// Golden: validation emits warn for skill missing activation banner.
    #[test]
    fn golden_validation_missing_banner_emits_warn() {
        let (_tmp, root) = build_fixture_dir();
        // Overwrite alpha's SKILL.md without an activation banner
        fs::write(
            root.join("skills").join("alpha").join("SKILL.md"),
            r#"---
name: alpha
version: 1.2.3
description: Alpha skill
icon: A
---
# No banner here
"#,
        )
        .unwrap();

        let snap = scan_marketplace(&root);
        let v = &snap.validation;
        let warn = v.results.iter().find(|r| {
            r.category == "Skill Banner" && r.status == "warn" && r.rule.contains("alpha")
        });
        assert!(
            warn.is_some(),
            "Expected a warn for alpha missing activation banner"
        );
    }

    /// Golden: validation emits warn for invalid semver version.
    #[test]
    fn golden_validation_invalid_semver_emits_warn() {
        let (_tmp, root) = build_fixture_dir();
        fs::write(
            root.join("skills").join("alpha").join("SKILL.md"),
            r#"---
name: alpha
version: not-semver
description: Alpha skill
icon: A
---
## Activation Banner
"#,
        )
        .unwrap();

        let snap = scan_marketplace(&root);
        let v = &snap.validation;
        let warn = v.results.iter().find(|r| {
            r.category == "Frontmatter" && r.status == "warn" && r.message.contains("not-semver")
        });
        assert!(
            warn.is_some(),
            "Expected a warn for invalid semver in alpha"
        );
    }

    // ---------------------------------------------------------------------------
    // GOLDEN TESTS — sync_counts
    // ---------------------------------------------------------------------------

    /// Golden: sync_counts dry_run returns changed files without writing.
    #[test]
    fn golden_sync_counts_dry_run_no_writes() {
        let (_tmp, root) = build_fixture_dir();

        // Write a README.md with a stale count
        fs::write(
            root.join("README.md"),
            "# Test\n\n> **99 skills + 1 agents + 1 sentinel engine (2 hooks) + 1 MCP servers**\n",
        )
        .unwrap();

        let report = sync_counts(&root, true /* dry_run */);
        assert!(report.dry_run);

        // In dry_run mode, nothing on disk should change
        let readme_after = fs::read_to_string(root.join("README.md")).unwrap();
        assert!(
            readme_after.contains("99 skills"),
            "dry_run must not write: content should still say 99 skills"
        );

        // But the report should flag README.md as something that would change
        assert!(
            report.files_changed.iter().any(|f| f.contains("README")),
            "Expected README.md in files_changed: {:?}",
            report.files_changed
        );
    }

    /// Golden: sync_counts live mode rewrites stale count in README.md.
    #[test]
    fn golden_sync_counts_live_rewrites_readme() {
        let (_tmp, root) = build_fixture_dir();

        fs::write(
            root.join("README.md"),
            "# Test\n\n> **99 skills + 1 agents + 1 sentinel engine (2 hooks) + 1 MCP servers**\n",
        )
        .unwrap();

        let report = sync_counts(&root, false /* live */);
        assert!(!report.dry_run);

        // File should now have the correct count (2 skills from fixture)
        let readme_after = fs::read_to_string(root.join("README.md")).unwrap();
        assert!(
            readme_after.contains("2 skills"),
            "Expected 2 skills after live sync, got: {readme_after}"
        );
        assert!(
            !readme_after.contains("99 skills"),
            "Old count should be gone"
        );
    }

    /// Golden: sync_counts updates marketplace.json description.
    #[test]
    fn golden_sync_counts_updates_marketplace_description() {
        let (_tmp, root) = build_fixture_dir();

        // Overwrite marketplace.json description with wrong counts
        fs::write(
            root.join("marketplace.json"),
            r#"{
  "version": "1.0.0",
  "description": "99 skills + 1 agents + 1 sentinel engine (99 hooks) + 99 MCP servers (99 repos) + 99 CLIs + hot-reload via Vulcan mcp-router for the full software development lifecycle",
  "skills": [
    {"name": "alpha", "priority": 1},
    {"name": "beta",  "priority": 2}
  ],
  "agents": [
    {"name": "my-agent", "model": "claude-3-5-sonnet", "file": "my-agent.md"}
  ],
  "mcp": [
    {"name": "linear", "command": "mcp-router --single linear-mcp", "transport": "stdio", "optional": false}
  ]
}"#,
        )
        .unwrap();

        sync_counts(&root, false);

        let mp_after = fs::read_to_string(root.join("marketplace.json")).unwrap();
        // Skills should be updated to 2 (the actual count from fixture)
        assert!(
            mp_after.contains("2 skills"),
            "marketplace.json description should have 2 skills: {mp_after}"
        );
        assert!(
            !mp_after.contains("99 skills"),
            "Old count should be gone from marketplace.json"
        );
    }

    /// Golden: sync_counts reports no changes for a file whose content already matches.
    #[test]
    fn golden_sync_counts_no_changes_when_current() {
        let (_tmp, root) = build_fixture_dir();
        // Write a README that has NO count patterns — sync_counts should leave it alone.
        fs::write(
            root.join("README.md"),
            "# Test project\n\nNo counts here.\n",
        )
        .unwrap();
        let report = sync_counts(&root, false);
        // README.md has no count patterns → should NOT appear in changed_files
        assert!(
            !report.files_changed.iter().any(|f| f.contains("README")),
            "README.md with no count patterns should not be touched, got: {:?}",
            report.files_changed
        );
    }

    // ---------------------------------------------------------------------------
    // Existing tests preserved below
    // ---------------------------------------------------------------------------

    #[test]
    fn test_parse_frontmatter_basic() {
        let content = r#"---
name: test-skill
version: 1.0.0
description: A test skill
icon: "🧪"
---

# Content here
"#;
        let fm = parse_frontmatter(content);
        assert_eq!(fm.get("name").unwrap(), "test-skill");
        assert_eq!(fm.get("version").unwrap(), "1.0.0");
        assert_eq!(fm.get("description").unwrap(), "A test skill");
    }

    #[test]
    fn test_parse_frontmatter_multiline() {
        let content = r#"---
name: test
description: >
  This is a long
  multi-line description
version: 2.0.0
---
"#;
        let fm = parse_frontmatter(content);
        assert_eq!(fm.get("name").unwrap(), "test");
        assert!(fm.get("description").unwrap().contains("long"));
        assert!(fm.get("description").unwrap().contains("multi-line"));
        assert_eq!(fm.get("version").unwrap(), "2.0.0");
    }

    #[test]
    fn test_extract_dependencies() {
        let content = r#"
## Dependencies

```skills
@use explore [context_gather, impact_analysis]
@use plan [task_breakdown]
```

Some text here.

```skills
@use test [unit_tests]
```
"#;
        let deps = extract_dependencies(content);
        assert_eq!(deps.len(), 3);
        assert_eq!(deps[0].skill, "explore");
        assert_eq!(deps[0].patterns, vec!["context_gather", "impact_analysis"]);
        assert_eq!(deps[1].skill, "plan");
        assert_eq!(deps[2].skill, "test");
    }

    #[test]
    fn test_infer_category() {
        assert_eq!(infer_category("explore"), "Core");
        assert_eq!(infer_category("test"), "Quality & Testing");
        assert_eq!(infer_category("debug"), "Lifecycle");
        assert_eq!(infer_category("linear"), "AI & Integration");
        assert_eq!(infer_category("unregistered-skill"), "Other");
    }

    #[test]
    fn test_parse_hooks_toml() {
        let content = r#"
[[hooks]]
id = "skill_router"
event = "UserPromptSubmit"
description = "Route to skills"
depends_on = []
has_api_call = false
matcher = []

[[hooks]]
id = "git_hygiene"
event = "PreToolUse"
description = "Git checks"
depends_on = ["skill_router"]
has_api_call = false
matcher = ["Edit", "Write"]
"#;
        let hooks = parse_hooks_toml(content);
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].name, "skill_router");
        assert_eq!(hooks[0].event, "UserPromptSubmit");
        assert_eq!(hooks[1].name, "git_hygiene");
        assert_eq!(hooks[1].matcher, "Edit, Write");
        assert_eq!(hooks[1].depends_on, vec!["skill_router"]);
    }

    #[test]
    fn count_declared_mcp_servers_reads_marketplace_json_and_none_when_absent() {
        let tmp = std::env::temp_dir().join(format!("sen-mcp-decl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // No marketplace.json yet -> None (caller falls back to ~/.claude.json).
        assert_eq!(super::count_declared_mcp_servers(&tmp), None);

        // marketplace.json with a 3-entry mcp[] -> Some(3), independent of any
        // live ~/.claude.json.
        fs::write(
            tmp.join("marketplace.json"),
            r#"{"mcp":[{"name":"a"},{"name":"b"},{"name":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(super::count_declared_mcp_servers(&tmp), Some(3));

        // A marketplace.json with no mcp key -> None (not Some(0)): nothing
        // declared means "fall back", not "zero servers".
        fs::write(tmp.join("marketplace.json"), r#"{"name":"x"}"#).unwrap();
        assert_eq!(super::count_declared_mcp_servers(&tmp), None);

        let _ = fs::remove_dir_all(&tmp);
    }
}
