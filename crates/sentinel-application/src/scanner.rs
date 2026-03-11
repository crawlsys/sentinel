//! Marketplace Scanner
//!
//! Scans the filesystem and builds a complete marketplace snapshot.
//! Shared logic used by both `session_init` (for CLAUDE.md generation)
//! and `sentinel scan` CLI command (for dashboard API).
//!
//! Ported from `dashboard/server/scanner.cjs` into Rust.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Dynamic component counts for CLAUDE.md generation and dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                        && !e.file_name().to_string_lossy().starts_with('_')
                })
                .count()
        })
        .unwrap_or(0)
}

/// Count files with a given extension in a directory (non-recursive).
pub fn count_files_with_ext(dir: &Path, ext: &str) -> usize {
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

/// Count MCP servers from `~/.claude.json`.
pub fn count_mcp_servers() -> usize {
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

/// Count Rust repos matching a suffix pattern in `~/Documents/GitHub/`.
pub fn count_repos_with_suffix(suffix: &str) -> usize {
    let gh_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Documents")
        .join("GitHub");

    fs::read_dir(gh_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                        && e.file_name().to_string_lossy().ends_with(suffix)
                })
                .count()
        })
        .unwrap_or(0)
}

/// Count all marketplace components in `~/.claude/`.
pub fn count_components(claude_dir: &Path) -> ComponentCounts {
    let skills = count_subdirs(&claude_dir.join("skills"));
    let hooks = super::hooks::HOOK_NAMES.len();
    let commands = count_files_with_ext(&claude_dir.join("commands"), ".md");
    let agents = count_files_with_ext(&claude_dir.join("agents"), ".md");
    let mcp_servers = count_mcp_servers();
    let mcp_repos = count_repos_with_suffix("-mcp-rust");
    let cli_repos = count_repos_with_suffix("-cli-rust");

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
/// Only matches `@use` in ````skills` fenced code blocks.
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
        ("Architecture", &["ddd-hexagonal", "api-design", "atomic-design"]),
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
                "steel",
                "steel-tester",
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
                        if let Some(arr_content) = val.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
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
    let hooks = scan_hooks();

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
    let counts = ComponentCounts {
        skills: skills.len(),
        hooks: hooks.len(),
        agents: agents.len(),
        commands: commands.len(),
        mcp_servers: mcp_servers.len(),
        mcp_repos: count_repos_with_suffix("-mcp-rust"),
        cli_repos: count_repos_with_suffix("-cli-rust"),
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
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
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
            .and_then(|v| v.as_u64())
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
            version: fm.get("version").cloned().unwrap_or_else(|| "0.0.0".to_string()),
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
fn scan_hooks() -> Vec<Hook> {
    let hooks_toml_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("sentinel")
        .join("config")
        .join("hooks.toml");

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

            let description = if !file.is_empty() {
                let agent_path = agents_dir.join(&file);
                fs::read_to_string(&agent_path)
                    .ok()
                    .map(|content| {
                        let fm = parse_frontmatter(&content);
                        fm.get("description").cloned().unwrap_or_default()
                    })
                    .unwrap_or_default()
            } else {
                String::new()
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
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().map(|ft| ft.is_file()).unwrap_or(false)
                && e.file_name().to_string_lossy().ends_with(".md")
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    command_files.sort();

    command_files
        .iter()
        .map(|file| {
            let content =
                fs::read_to_string(commands_dir.join(file)).unwrap_or_default();
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
                        .and_then(|v| v.as_bool())
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
    let mut results: Vec<ValidationResult> = Vec::new();

    let actual_counts: HashMap<&str, usize> = HashMap::from([
        ("skills", skills.len()),
        ("hooks", hooks.len()),
        ("agents", agents.len()),
        ("mcpServers", mcp_servers.len()),
    ]);

    // --- Category 1: Component Count Consistency ---
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
                    message: format!(
                        "{key}: description says {expected}, filesystem has {actual}"
                    ),
                    expected: Some(expected),
                    actual: Some(actual),
                });
            }
        }
    }

    // --- Category 2: File Existence Cross-Reference ---
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
    let hooks_toml_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("sentinel")
        .join("config")
        .join("hooks.toml");

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
            let name = a
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let file = a
                .get("file")
                .and_then(|v| v.as_str())
                .unwrap_or("");
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

    // --- Category 3: Frontmatter Integrity ---
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

    // --- Category 4: Dependency Graph Validity ---
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

    // --- Category 5: Documentation Count Scanning ---
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

    // --- Summary ---
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(infer_category("unknown-skill"), "Other");
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
}
