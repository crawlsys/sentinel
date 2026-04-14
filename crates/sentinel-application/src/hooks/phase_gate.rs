//! Phase Gate Hook
//!
//! Blocks tool calls when skill phases are skipped.
//! Uses the workflow state machine to determine if a tool should be blocked.
//!
//! Enhanced features (ported from Node.js phase-gate.js):
//! - Tracks Read() calls on phase files via SessionState
//! - Formatted block messages with visual boxes
//! - Post-merge skip detection (review done but qa-handoff not loaded)
//! - Allows tools within 1 phase gap (mid-phase), blocks at 2+ gap

use regex::Regex;
use sha2::{Digest, Sha256};

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::SkillWorkflow;
use std::collections::HashMap;

fn block_with_context(input: &HookInput, reason: impl Into<String>) -> HookOutput {
    HookOutput::block(super::block_context::append_block_context(reason, input))
}

fn deny_with_context(input: &HookInput, reason: impl Into<String>) -> HookOutput {
    HookOutput::deny(super::block_context::append_block_context(reason, input))
}

/// Extracted phase file info from a Read() tool_input path.
#[derive(Debug, Clone)]
struct PhaseFileInfo {
    /// The phase filename (e.g., "claim.md")
    file: String,
    /// The skill name derived from the path (e.g., "linear")
    skill: String,
    /// Whether the file passed canonical path validation (exists on disk
    /// and resolves to within ~/.claude/skills/). Untrusted files are
    /// recorded for tracking but do NOT advance workflow state.
    trusted: bool,
    /// **Attack #141 fix**: The canonicalized absolute path to the phase file.
    /// Used for content hashing instead of the user-supplied tool_input path
    /// to close a TOCTOU gap where a symlink swap between canonicalize() and
    /// read_to_string() could feed different content than what we validated.
    canonical_path: Option<std::path::PathBuf>,
}

/// Extract the phase file name AND skill name from a Read() tool_input path.
/// Matches paths like `~/.claude/skills/linear/phases/claim.md`
/// or `C:\Users\...\.claude\skills\linear\phases\claim.md`.
///
/// Returns `Some(PhaseFileInfo)` if the path is a valid phase file.
/// Validates:
/// - Path components match `skills/{name}/phases/{file}.md` pattern
/// - No `ParentDir` (`..`) components (checked via Path::components())
/// - Skill name and file name contain only safe ASCII characters
/// - Symlinks resolve to a path still under `~/.claude/skills/` (PathBuf API)
/// - `trusted` flag indicates whether canonical validation passed
fn extract_phase_file(tool_input: &serde_json::Value) -> Option<PhaseFileInfo> {
    use std::path::{Component, Path};

    // tool_input for Read is { "file_path": "..." }
    let path = tool_input.get("file_path").and_then(|v| v.as_str())?;

    // Parse into path components — this handles both `/` and `\` separators
    // and gives us semantic components (Normal, ParentDir, RootDir, etc.)
    // **Attack #94 fix**: Reject Windows UNC/device path prefixes.
    // Paths like `\\?\C:\...` or `\\.\device\...` use extended-length syntax
    // that can bypass canonical path comparisons (starts_with may fail when
    // mixing prefixed and non-prefixed paths).
    if path.starts_with("\\\\?\\")
        || path.starts_with("\\\\.\\")
        || path.starts_with("//?/")
        || path.starts_with("//./")
    {
        return None;
    }

    let file_path = Path::new(path);
    let components: Vec<Component> = file_path.components().collect();

    // Reject any ParentDir (..) components — checked structurally, not as substring.
    // This avoids both false positives (filenames containing "..") and false negatives
    // (edge cases where string matching diverges from OS path resolution).
    if components.iter().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }

    // Find the `skills` component and extract the pattern:
    //   skills / {skill_name} / phases / {phase_file}.md
    // Match as full path components, not substring — prevents matching
    // paths like `/foo/myskills/linear/phases/claim.md` or `skills_evil/...`.
    let skills_pos = components
        .iter()
        .position(|c| matches!(c, Component::Normal(s) if *s == std::ffi::OsStr::new("skills")))?;

    // Need exactly 3 more components after "skills": {name}, "phases", {file}.md
    // And nothing after the .md file.
    if skills_pos + 4 != components.len() {
        return None;
    }

    let skill_name = components[skills_pos + 1].as_os_str().to_str()?;
    let phases_component = components[skills_pos + 2].as_os_str().to_str()?;
    let phase_file = components[skills_pos + 3].as_os_str().to_str()?;

    // Verify the "phases" directory component
    if phases_component != "phases" {
        return None;
    }

    // Must be a .md file
    if !phase_file.ends_with(".md") {
        return None;
    }

    // Validate names: ASCII alphanumeric + hyphens + underscores + dots only
    if !is_safe_name(skill_name) || !is_safe_name(phase_file) {
        return None;
    }

    // Symlink/canonical path resolution — eliminates TOCTOU by calling
    // canonicalize() directly without a prior exists() check.
    // canonicalize() returns Err if the file doesn't exist, which we handle.
    let canonical_result = file_path.canonicalize();

    // Track whether the file passed canonical validation.
    // Files that don't exist on disk are still extracted (for phase tracking)
    // but marked untrusted — the caller should not advance workflow state.
    let trusted = match &canonical_result {
        Ok(canonical) => {
            // Use PathBuf::starts_with() — component-aware, not string prefix.
            // This prevents sibling-directory tricks like `skills_evil/` matching
            // a string prefix of `skills`.
            // **Attack #97 fix**: Panic instead of empty fallback.
            // Empty PathBuf makes skills_dir ".claude/skills" (relative),
            // which never matches canonical absolute paths — all files
            // become "untrusted" but phases_read is still recorded.
            let skills_dir = dirs::home_dir()
                .expect("[sentinel] FATAL: Cannot determine home directory")
                .join(".claude")
                .join("skills");
            let skills_canonical = skills_dir.canonicalize().unwrap_or(skills_dir);

            if !canonical.starts_with(&skills_canonical) {
                eprintln!(
                    "[sentinel] SECURITY: Phase file '{}' resolves to '{}' \
                     which is outside ~/.claude/skills/. Rejecting symlink escape.",
                    path,
                    canonical.display()
                );
                return None;
            }
            true
        }
        Err(_) => {
            // File doesn't exist on disk — textual validation passed above.
            // Mark as untrusted so workflow state is NOT advanced.
            false
        }
    };

    Some(PhaseFileInfo {
        file: phase_file.to_string(),
        skill: skill_name.to_string(),
        trusted,
        canonical_path: canonical_result.ok(),
    })
}

/// Validate a path segment contains only safe ASCII characters.
/// Allows: a-z, A-Z, 0-9, hyphens, underscores, dots (for .md extension).
///
/// Explicitly rejects ALL non-ASCII characters, including Unicode confusables
/// (e.g., Cyrillic 'а' U+0430 vs Latin 'a' U+0061) that could bypass
/// skill name matching via homoglyph attacks.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.is_ascii()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Process a phase-gate hook event (PreToolUse)
///
/// This function handles two responsibilities:
/// 1. Track Read() calls on phase files (recording them in state)
/// 2. Gate non-safe tool calls based on workflow phase progress
pub fn process(
    input: &HookInput,
    state: &mut SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
    fs: &dyn super::FileSystemPort,
) -> HookOutput {
    let tool_name = match &input.tool_name {
        Some(name) => name.as_str(),
        None => return HookOutput::allow(),
    };

    // ── Glass break emergency override ────────────────────────────────────
    // If a glass break is active, bypass all workflow enforcement and log
    // the tool call for audit. Check expiry first so expired breaks are
    // cleaned up before the next gate check.
    state.clear_expired_break();
    if state.is_break_active() {
        if let Some(ref mut gb) = state.glass_break {
            gb.tools_used.push(sentinel_domain::state::BreakToolUse {
                tool: tool_name.to_string(),
                detail: input
                    .tool_input
                    .as_ref()
                    .and_then(|v| v.get("command").or(v.get("file_path")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                ts: chrono::Utc::now().to_rfc3339(),
            });
        }
        return HookOutput::allow();
    }

    // **Attack #100/#114 defense-in-depth**: Validate tool name format.
    // Claude Code sends PascalCase (Read, Bash, Edit) or mcp__prefix (mcp__linear__*).
    // If we see unexpected casing, log a warning. This catches potential bypass
    // attempts via tool name spoofing (e.g., "read" instead of "Read").
    if !tool_name.is_empty() && !tool_name.starts_with("mcp__") {
        let first = tool_name.chars().next().unwrap_or('_');
        if !first.is_ascii_uppercase() {
            eprintln!(
                "[sentinel] WARNING: Unexpected tool name casing '{}'. \
                 Expected PascalCase or mcp__prefix. Treating as non-safe.",
                tool_name
            );
        }
    }

    // **Attack #200 fix**: MCP tools with write/exec capabilities MUST go through
    // phase gate enforcement. Previously ALL mcp__ tools were auto-allowed, letting
    // any MCP server (codex, steel, etc.) bypass workflow restrictions by writing
    // files, running commands, or applying patches through their own tools.
    //
    // Dangerous MCP tool suffixes — any mcp__*__<suffix> matching these is treated
    // as a non-safe tool and subject to the same gate enforcement as Bash/Edit/Write.
    if tool_name.starts_with("mcp__") {
        if is_dangerous_mcp_tool(tool_name) {
            // Fall through to gate enforcement below (same as Bash/Edit/Write)
            eprintln!(
                "[sentinel] MCP tool '{}' classified as dangerous — applying gate enforcement",
                tool_name
            );
        } else {
            // Safe/read-only MCP tool — allow without gate check
            return HookOutput::allow();
        }
    }

    // Track ALL tool calls for phase-skip detection
    state.record_tool_call();

    // ── Bash command pattern check ────────────────────────────────────────
    // If this is a Bash call, check if the command matches any blocked patterns
    // from the active workflow. This closes the CLI escape vector.
    if tool_name == "Bash" {
        if let Some(block) = check_blocked_bash_patterns(state, workflows, input) {
            return block;
        }
    }

    // If this is a Read() call, check if it's reading a phase file
    if tool_name == "Read" {
        if let Some(ref tool_input) = input.tool_input {
            if let Some(info) = extract_phase_file(tool_input) {
                // Only record phase reads AND advance workflow for trusted files
                // (exist on disk, canonicalize to ~/.claude/skills/).
                // Untrusted files are NOT recorded — prevents phantom phase
                // completion and progress inflation via crafted paths.
                if !info.trusted {
                    return HookOutput::allow();
                }

                state.record_phase_read(&info.skill, &info.file);

                // ── Content hashing (Patch B / #41 TOCTOU mitigation) ──────
                // On first trusted Read(), compute SHA-256 of the file content
                // and store it. On subsequent reads, reject if content changed.
                // This detects mid-session phase file tampering.
                //
                // TOCTOU note (Attack #41): There is an inherent race between
                // sentinel reading the file here (PreToolUse) and Claude Code
                // reading it moments later. An attacker who can swap the file
                // in that microsecond window could feed Claude different content
                // than what we hashed. This is mitigated by:
                //   1. The canonical path check (symlinks resolved ahead of time)
                //   2. The protected-path write protection (blocks Write/Edit to
                //      phase files during active workflows)
                //   3. The Bash redirect protection (blocks > to phase file paths)
                // Together these make the swap window unexploitable without
                // out-of-band file system access (which is out of threat model).
                // **Attack #141 fix**: Use the canonical path (resolved by
                // extract_phase_file) for content hashing, not the user-supplied
                // tool_input path. This closes the TOCTOU gap where a symlink
                // could be swapped between canonicalize() and read_to_string().
                let read_path = info.canonical_path.as_deref().unwrap_or_else(|| {
                    std::path::Path::new(
                        tool_input
                            .get("file_path")
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                    )
                });
                if let Ok(content) = std::fs::read_to_string(read_path) {
                    let mut hasher = Sha256::new();
                    hasher.update(content.as_bytes());
                    let hash = format!("{:x}", hasher.finalize());

                    let canonical_key = format!("{}/{}", info.skill, info.file);
                    if let Err(tamper_msg) = state.record_phase_file_hash(&canonical_key, &hash) {
                        eprintln!("[sentinel] SECURITY: {}", tamper_msg);
                        return block_with_context(
                            input,
                            format!(
                                "+============================================================+\n\
                             |  BLOCKED: Phase File Tampering Detected                    |\n\
                             +============================================================+\n\
                             |  {:<57}|\n\
                             |                                                            |\n\
                             |  The content of a phase file changed since it was first     |\n\
                             |  read in this session. This may indicate an attempt to      |\n\
                             |  weaken phase requirements mid-workflow.                    |\n\
                             |                                                            |\n\
                             |  Session must be restarted to proceed.                     |\n\
                             +============================================================+",
                                canonical_key
                            ),
                        );
                    }
                }

                // Auto-advance workflow when phase file is read.
                // Reading the phase file = proof of engagement under hard gate.
                //
                // FIX: Derive skill from the path (info.skill), not active_skill.
                // This prevents misattribution when multiple skills are in play.
                // Fall back to active_skill only if the path-derived skill has
                // no workflow definition in the config.
                let skill_to_advance = if workflows.contains_key(&info.skill) {
                    Some(info.skill.clone())
                } else if let Some(ref active) = state.active_skill {
                    if workflows.contains_key(active.as_str()) {
                        // Fallback: path-derived skill not in workflows, using active_skill.
                        // This is a potential misconfiguration — the phase file path
                        // references a skill that has no workflow definition.
                        eprintln!(
                            "[sentinel] WARNING: Phase file path references skill '{}' \
                             which has no workflow definition. Falling back to active_skill '{}'. \
                             This may indicate a misconfigured skill or stale phase file.",
                            info.skill, active
                        );
                        Some(active.clone())
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(skill_name) = skill_to_advance {
                    // FIX: Use strip_suffix instead of trim_end_matches.
                    // trim_end_matches removes repeated char patterns, not the suffix.
                    // e.g., "add.md" with trim_end_matches(".md") would strip "d" too.
                    let phase_id = info.file.strip_suffix(".md").unwrap_or(&info.file);

                    // Validate phase_id against known workflow phases before advancing.
                    // Only advance if this is a recognized phase, preventing
                    // arbitrary state manipulation via crafted filenames.
                    let is_known_phase = workflows
                        .get(&skill_name)
                        .map(|w| w.phases.iter().any(|p| p.id == phase_id))
                        .unwrap_or(false);

                    if is_known_phase {
                        // Ensure workflow state exists for this skill
                        if !state.workflows.contains_key(&skill_name) {
                            state.workflows.insert(
                                skill_name.clone(),
                                sentinel_domain::workflow::WorkflowState::new(
                                    &skill_name,
                                    &state.session_id,
                                ),
                            );
                        }
                        // Use advance_sequential() to enforce phase ordering.
                        // Reading phase 5 before phase 1 will NOT advance state.
                        if let (Some(wf), Some(wf_def)) = (
                            state.workflows.get_mut(&skill_name),
                            workflows.get(&skill_name),
                        ) {
                            wf.advance_sequential(phase_id, wf_def);
                        }
                    }
                }
            }
        }
        // Read calls always pass through (they're safe tools)
        return HookOutput::allow();
    }

    // ── Protected path write protection ─────────────────────────────────
    // Block Write/Edit/NotebookEdit to:
    //   1. skills/*/phases/*.md — phase file tampering
    //   2. ~/.claude/sentinel/ — config, state, hooks.toml, workflows.toml
    //   3. ~/.claude/settings.json — hook registrations
    //   4. ~/.claude/skills/*/SKILL.md — skill definitions (fake skill creation)
    //   5. ~/.claude.json — MCP server registrations
    // Active when ANY workflow has been touched in this session.
    if tool_name == "Write" || tool_name == "Edit" || tool_name == "NotebookEdit" {
        if let Some(block) = check_protected_path_write(state, workflows, input) {
            return block;
        }
    }

    // Delegate to gate evaluation for blocking decisions
    let result = crate::gate::evaluate(state, workflows, input, fs);
    match result {
        crate::gate::GateDecision::Allow => {
            // Additional post-merge skip detection:
            // If review.md is read but qa-handoff.md is not, and we're past
            // the review phase, block non-safe tools
            if let Some(block) = check_post_merge_skip(state, workflows, input, tool_name) {
                return block;
            }
            HookOutput::allow()
        }
        crate::gate::GateDecision::Block {
            reason,
            next_phase,
            next_phase_file,
        } => {
            let skill = state.active_skill.as_deref().unwrap_or("unknown");
            let completed = state
                .active_workflow()
                .map(|w| w.completed_phases.len())
                .unwrap_or(0);
            let total = workflows
                .get(skill)
                .map(|w| w.phases.iter().filter(|p| p.required).count())
                .unwrap_or(0);

            let message = format_block_box(
                skill,
                &reason,
                &next_phase,
                &next_phase_file,
                completed,
                total,
            );
            deny_with_context(input, message)
        }
    }
}

/// Three-layer Bash command enforcement:
///
/// **Layer 1 — Obfuscation detection**: Catch shell tricks that defeat regex
/// matching: `eval`, `base64 -d`, `$'\xHH'` hex escapes, variable-based
/// command construction (`cmd="steel"; $cmd-mcp`). If detected AND a
/// workflow with blocked patterns or an allowlist is active, hard-deny.
///
/// **Layer 2 — Allowlist (nuclear option)**: If ANY active workflow has a
/// non-empty `bash_allowlist`, the command MUST match at least one allowlist
/// pattern. Everything else is denied. This defeats ALL obfuscation because
/// encoded payloads won't match the allowlist.
///
/// **Layer 3 — Blocklist**: Check patterns against the full command string
/// AND extracted inner commands from `bash -c "..."` wrappers.
///
/// Checks patterns across ALL workflows with active state (not just
/// `active_skill`), preventing the skill-switch bypass.
fn check_blocked_bash_patterns(
    state: &SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
    input: &HookInput,
) -> Option<HookOutput> {
    let cmd = input.tool_input.as_ref()?.get("command")?.as_str()?;

    // ── Layer 0: Bash redirect to protected paths ────────────────────
    // Block Bash commands that use shell redirects (>, >>, tee) to write
    // to protected sentinel paths. This closes the vector where Write/Edit
    // are protected but `echo '{}' > ~/.claude/sentinel/state/sess.json`
    // can tamper with state files directly (Attack #37).
    if !state.workflows.is_empty() || state.active_skill.is_some() {
        if let Some(block) = check_bash_redirect_to_protected(cmd, input) {
            return Some(block);
        }
    }

    // Collect enforcement config from ALL active workflows
    let mut all_block_patterns: Vec<(&str, &str)> = Vec::new();
    let mut all_allow_patterns: Vec<(&str, &str)> = Vec::new();
    let mut has_any_enforcement = false;

    for (skill_name, workflow) in workflows {
        let is_relevant = state.active_skill.as_deref() == Some(skill_name)
            || state.workflows.contains_key(skill_name);
        if !is_relevant {
            continue;
        }
        for p in &workflow.blocked_bash_patterns {
            all_block_patterns.push((p, skill_name));
            has_any_enforcement = true;
        }
        for p in &workflow.bash_allowlist {
            all_allow_patterns.push((p, skill_name));
            has_any_enforcement = true;
        }
    }

    if !has_any_enforcement {
        return None;
    }

    // ── Layer 1: Obfuscation detection ──────────────────────────────
    // These patterns indicate the command is trying to evade regex matching.
    // Block them unconditionally when ANY workflow enforcement is active.
    static OBFUSCATION_PATTERNS: std::sync::LazyLock<Vec<(&str, Regex)>> =
        std::sync::LazyLock::new(|| {
            vec![
                // eval + base64 decode: eval $(echo payload | base64 -d) or --decode
                (
                    "eval with base64 decode",
                    Regex::new(r"eval.*base64\s+(-d|--decode)").unwrap(),
                ),
                // eval + command substitution: eval $(...) or eval `...`
                (
                    "eval with command substitution",
                    Regex::new(r"eval\s+[\$`]").unwrap(),
                ),
                // Hex escape sequences: $'\x2d' etc.
                (
                    "hex escape sequence",
                    Regex::new(r"\$'\\x[0-9a-fA-F]").unwrap(),
                ),
                // Octal escape sequences: $'\055' etc.
                (
                    "octal escape sequence",
                    Regex::new(r"\$'\\[0-7]{3}").unwrap(),
                ),
                // Variable-based command construction + execution:
                // cmd="steel-mcp"; $cmd  OR  cmd+="mcp"; $cmd
                (
                    "variable command execution",
                    Regex::new(r#";\s*\$\w+"#).unwrap(),
                ),
                // base64 decode piped to shell: | base64 -d | bash (or --decode)
                (
                    "base64 piped to shell",
                    Regex::new(r"base64\s+(-d|--decode).*\|\s*(bash|sh|zsh)").unwrap(),
                ),
                // base64 decode in command substitution: $(echo ... | base64 -d)
                (
                    "base64 in command substitution",
                    Regex::new(r"\$\(.*base64\s+(-d|--decode)").unwrap(),
                ),
                // Python/perl/ruby one-liner execution for evasion
                (
                    "scripting language exec",
                    Regex::new(r"(python3?|perl|ruby)\s+-[ec]\s+.*exec").unwrap(),
                ),
                // Process substitution: <(cmd) feeds blocked command output
                ("process substitution", Regex::new(r"<\([^)]+\)").unwrap()),
                // ANSI-C quoting: $'string' can encode arbitrary bytes via \xNN, \NNN, \uNNNN
                // Already caught hex/octal inside $'...' above, but catch any $'...' with backslash escapes
                (
                    "ANSI-C quote with escape",
                    Regex::new(r"\$'[^']*\\[^']+[^']*'").unwrap(),
                ),
                // Here-strings with command substitution or pipe to shell:
                // e.g., `bash <<< $(blocked-cmd)` or `cat <<< $(cmd) | sh`
                // **Attack #77 fix**: Only block <<< when followed by $() or `` or piped to shell,
                // not plain `grep pattern <<< "$variable"` which is safe.
                (
                    "here-string with substitution",
                    Regex::new(r"<<<\s*[\$`]").unwrap(),
                ),
                (
                    "here-string piped to shell",
                    Regex::new(r"<<<.*\|\s*(bash|sh|zsh)").unwrap(),
                ),
                // printf with escape sequences to construct commands: printf '\x67\x69\x74'
                (
                    "printf escape construction",
                    Regex::new(r"printf\s+.*\\x[0-9a-fA-F]").unwrap(),
                ),
            ]
        });

    for (desc, re) in OBFUSCATION_PATTERNS.iter() {
        if re.is_match(cmd) {
            return Some(deny_with_context(
                input,
                format!(
                    "+============================================================+\n\
                 |  BLOCKED: Shell Obfuscation Detected                       |\n\
                 +============================================================+\n\
                 |  Pattern: {:<48}|\n\
                 |                                                            |\n\
                 |  Commands using shell obfuscation techniques (eval, base64,|\n\
                 |  hex escapes, variable construction) are blocked when       |\n\
                 |  workflow enforcement is active.                           |\n\
                 +============================================================+",
                    desc,
                ),
            ));
        }
    }

    // ── Layer 2: Allowlist (nuclear option) ──────────────────────────
    // If any active workflow has an allowlist, EVERY segment of the command
    // must match at least one allowlist pattern. Segments are split on
    // &&, ||, ;, |, and \n to prevent chaining/piping an allowed prefix
    // with a blocked command. Segments containing $(...) or backtick
    // substitution are rejected outright (the inner command escapes the
    // allowlist check — Attack #30).
    if !all_allow_patterns.is_empty() {
        // Split command on shell operators AND newlines to get individual segments
        // Pipe (|) added per Attack #29, newline (\n) per Attack #33
        static SEGMENT_SPLIT: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| Regex::new(r"\s*(?:&&|\|\||\||;|\n)\s*").unwrap());
        // Detect subshell / command substitution embedded in segments
        static SUBSHELL_RE: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| Regex::new(r"(?:\$\([^)]+\)|`[^`]+`)").unwrap());
        let segments: Vec<&str> = SEGMENT_SPLIT
            .split(cmd)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        // Safe subshell pattern: $(cat <<'EOF' or $(cat <<EOF — ONLY cat with heredoc.
        // Rejects $(cat /etc/passwd; evil-cmd) because it contains ; after cat.
        static SAFE_SUBSHELL_RE: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| Regex::new(r"^\$\(cat\s+<<").unwrap());

        for segment in &segments {
            // Reject segments containing command substitution — the inner
            // command could execute anything, bypassing the allowlist.
            // Exception: $(cat <<'EOF'...) heredoc pattern used in git commits.
            // The exception is strict: must start with `$(cat <<` (heredoc only).
            // This rejects `$(cat /file; evil)` which contains $(cat but isn't a heredoc.
            if SUBSHELL_RE.is_match(segment) {
                // Check ALL $(...) occurrences in the segment — every one must be safe
                let all_subshells_safe = {
                    let mut safe = true;
                    // Find each $( in the segment and check if it's a safe heredoc pattern
                    let bytes = segment.as_bytes();
                    let mut pos = 0;
                    while pos + 1 < bytes.len() {
                        if bytes[pos] == b'$' && bytes[pos + 1] == b'(' {
                            let remainder = &segment[pos..];
                            if !SAFE_SUBSHELL_RE.is_match(remainder) {
                                safe = false;
                                break;
                            }
                            pos += 2;
                        } else {
                            pos += 1;
                        }
                    }
                    safe
                };
                // Also reject backtick substitution entirely (no safe exemption)
                let has_backtick = segment.contains('`');
                if !all_subshells_safe || has_backtick {
                    let seg_display = if segment.len() > 48 {
                        &segment[..48]
                    } else {
                        segment
                    };
                    return Some(deny_with_context(
                        input,
                        format!(
                            "+============================================================+\n\
                         |  BLOCKED: Command Substitution in Allowlisted Context      |\n\
                         +============================================================+\n\
                         |  Segment: {:<48}|\n\
                         |                                                            |\n\
                         |  Commands containing $(...) or backtick substitution are    |\n\
                         |  blocked because the inner command escapes allowlist        |\n\
                         |  enforcement. Only $(cat <<EOF ...) heredocs are allowed.   |\n\
                         +============================================================+",
                            seg_display,
                        ),
                    ));
                }
            }

            // Check BOTH raw and shell-normalized segment against the allowlist.
            // Attack #40: Without checking normalized form, a command like
            // `g\i\t commit` bypasses allowlist pattern `^git ` (raw doesn't
            // match, but shell executes it as `git commit`). Checking normalized
            // form closes this — the normalized `git commit` matches the allowlist,
            // so the command passes. But we ALSO require the normalized form to
            // match when it differs, preventing false allowlist matches on the
            // raw form that don't survive normalization.
            let normalized_seg = normalize_shell_quoting(segment);
            let mut segment_allowed = false;
            for (pattern, _skill) in &all_allow_patterns {
                // **Attack #83 fix**: Reject excessively long or complex patterns
                // that could cause ReDoS. Config TOML is in a protected dir but
                // defense-in-depth against crafted patterns.
                if pattern.len() > 256 {
                    eprintln!(
                        "[sentinel] WARNING: Skipping oversized allowlist pattern ({} chars)",
                        pattern.len()
                    );
                    continue;
                }
                if let Ok(re) = Regex::new(pattern) {
                    // Accept if raw matches OR normalized matches.
                    // Allowlists are safe against obfuscation (obfuscated forms
                    // won't match and get denied), so checking the normalized
                    // form only expands acceptance for legitimate variants.
                    if re.is_match(segment) || re.is_match(&normalized_seg) {
                        segment_allowed = true;
                        break;
                    }
                }
            }
            if !segment_allowed {
                let seg_display = if segment.len() > 48 {
                    &segment[..48]
                } else {
                    segment
                };
                return Some(deny_with_context(
                    input,
                    format!(
                        "+============================================================+\n\
                     |  BLOCKED: Command Segment Not in Bash Allowlist            |\n\
                     +============================================================+\n\
                     |  Segment: {:<48}|\n\
                     |                                                            |\n\
                     |  An active workflow restricts Bash to allowlisted commands  |\n\
                     |  only. Each chained segment (&&, ||, ;, |, \\n) must        |\n\
                     |  individually match the allowlist.                          |\n\
                     +============================================================+",
                        seg_display,
                    ),
                ));
            }
        }
    }

    // ── Layer 3: Blocklist ──────────────────────────────────────────
    // Check patterns against:
    //   a) the raw command
    //   b) extracted inner commands from bash -c "..." wrappers
    //   c) extracted inner commands from $(...) subshells
    //   d) pipe-split and newline-split segments
    //   e) a shell-normalized version (strips quotes, backslash escapes)
    //
    // The normalization step (e) defeats quote-split evasion:
    //   "st""eel-mcp" → steel-mcp
    //   s\t\e\e\l-m\c\p → steel-mcp
    //   'railway'' up' → railway up
    let mut targets: Vec<String> = vec![cmd.to_string()];

    // Extract inner command from bash -c "..." / sh -c '...'
    // **Attack #71 fix**: Use separate patterns for single and double quotes
    // to correctly handle mixed quoting like bash -c "st'eel-mcp connect"
    static SHELL_C_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?:bash|sh|zsh)\s+-[a-z]*c\s+(?:"([^"]+)"|'([^']+)')"#).unwrap()
    });
    for cap in SHELL_C_RE.captures_iter(cmd) {
        // Group 1 = double-quoted content, Group 2 = single-quoted content
        let inner = cap.get(1).or_else(|| cap.get(2));
        if let Some(inner) = inner {
            targets.push(inner.as_str().to_string());
        }
    }

    // Extract inner commands from $(...) subshells (Attack #30)
    static SUBSHELL_EXTRACT_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\$\(([^)]+)\)").unwrap());
    for cap in SUBSHELL_EXTRACT_RE.captures_iter(cmd) {
        if let Some(inner) = cap.get(1) {
            targets.push(inner.as_str().to_string());
        }
    }

    // Extract inner commands from backtick substitution (Attack #30)
    static BACKTICK_EXTRACT_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());
    for cap in BACKTICK_EXTRACT_RE.captures_iter(cmd) {
        if let Some(inner) = cap.get(1) {
            targets.push(inner.as_str().to_string());
        }
    }

    // Split on pipe and newline to catch piped/multi-line evasion (Attack #29/#33)
    static PIPE_NEWLINE_SPLIT: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\s*(?:\||\n)\s*").unwrap());
    for segment in PIPE_NEWLINE_SPLIT.split(cmd) {
        let seg = segment.trim();
        if !seg.is_empty() && seg != cmd {
            targets.push(seg.to_string());
        }
    }

    // Add shell-normalized version: strip quotes and backslash escapes
    let normalized_cmd = normalize_shell_quoting(cmd);
    if normalized_cmd != cmd {
        targets.push(normalized_cmd);
    }

    for (pattern, skill_name) in &all_block_patterns {
        // **Attack #83 fix**: Reject oversized patterns (ReDoS defense-in-depth)
        if pattern.len() > 256 {
            eprintln!(
                "[sentinel] WARNING: Skipping oversized blocklist pattern ({} chars)",
                pattern.len()
            );
            continue;
        }
        let re = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "[sentinel] WARNING: Invalid blocked_bash_pattern '{}': {}",
                    pattern, e
                );
                continue;
            }
        };

        for target in &targets {
            if re.is_match(target) {
                let cmd_display = if cmd.len() > 48 { &cmd[..48] } else { cmd };
                return Some(deny_with_context(
                    input,
                    format!(
                        "+============================================================+\n\
                     |  BLOCKED: Bash Command Matches Blocked Pattern             |\n\
                     +============================================================+\n\
                     |  Skill: {:<50}|\n\
                     |  Pattern: {:<48}|\n\
                     |  Command: {:<48}|\n\
                     |                                                            |\n\
                     |  This command is blocked because it could bypass the        |\n\
                     |  workflow's phase-gated tool enforcement.                   |\n\
                     |  Use the workflow's native MCP tools instead.              |\n\
                     +============================================================+",
                        skill_name, pattern, cmd_display,
                    ),
                ));
            }
        }
    }

    None
}

/// Normalize shell quoting artifacts from a command string.
///
/// Removes:
///   - Adjacent double quotes: `"steel""-mcp"` → `steel-mcp`
///   - Adjacent single quotes: `'rail''way'` → `railway`
///   - Standalone backslash escapes: `s\t\e\e\l` → `steel`
///   - Process substitution wrappers: `<(...)` → contents
///
/// This is NOT a full shell parser — it's a best-effort normalization
/// to defeat common quote-split and backslash-escape evasion patterns.
fn normalize_shell_quoting(cmd: &str) -> String {
    let mut result = String::with_capacity(cmd.len());
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            // Skip standalone double quotes (quote-split concatenation)
            '"' => {
                i += 1;
            }
            // Skip standalone single quotes
            '\'' => {
                i += 1;
            }
            // Backslash: if followed by a normal char, skip the backslash
            // (e.g., s\t → st). But preserve \n, \t, \\ etc. as-is since
            // the regex will match the normalized form anyway.
            '\\' if i + 1 < chars.len() => {
                let next = chars[i + 1];
                // If next char is alphanumeric or hyphen, it's an evasion attempt
                // like s\t\e\e\l → steel. Strip the backslash.
                if next.is_ascii_alphanumeric() || next == '-' || next == '_' {
                    result.push(next);
                    i += 2;
                } else {
                    // Preserve the backslash for actual escape sequences
                    result.push('\\');
                    result.push(next);
                    i += 2;
                }
            }
            // Process substitution: <(...) — extract inner content
            '<' if i + 1 < chars.len() && chars[i + 1] == '(' => {
                i += 2; // skip <(
                        // Don't add the <( to result, the inner content will be added naturally
            }
            c => {
                result.push(c);
                i += 1;
            }
        }
    }

    result
}

/// Check if a Bash command redirects output to a protected sentinel path.
///
/// Catches: `> path`, `>> path`, `tee path`, `tee -a path`, `cp src path`.
/// Uses the same textual check as Write/Edit protection.
fn check_bash_redirect_to_protected(cmd: &str, input: &HookInput) -> Option<HookOutput> {
    // Extract all potential file targets from redirects and tee commands.
    // Pattern: anything followed by > or >> followed by a path
    // **Attack #70 fix**: Extended to catch mv, ln, install, dd of=, curl -o, wget -O
    static REDIRECT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?:>{1,2}|tee\s+(?:-a\s+)?|cp\s+\S+\s+|mv\s+\S+\s+|ln\s+(?:-[sf]+\s+)*\S+\s+|install\s+\S+\s+|curl\s+.*-o\s+|wget\s+.*-O\s+)\s*["']?([^\s"'|;&]+)"#).unwrap()
    });
    // Also check dd of= pattern separately (different syntax)
    static DD_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r#"dd\s+.*of=["']?([^\s"'|;&]+)"#).unwrap());

    for cap in REDIRECT_RE.captures_iter(cmd) {
        if let Some(path_match) = cap.get(1) {
            let path = path_match.as_str();
            let normalized = path.replace('\\', "/");
            // Expand ~ to home dir for textual check
            let expanded = if normalized.starts_with("~/") || normalized.starts_with("~\\") {
                // **Attack #90 fix**: Panic instead of empty fallback — empty PathBuf
                // makes all ~/‐prefixed protected path checks silently pass.
                let home =
                    dirs::home_dir().expect("[sentinel] FATAL: Cannot determine home directory");
                format!(
                    "{}/{}",
                    home.to_string_lossy().replace('\\', "/"),
                    &normalized[2..]
                )
            } else {
                normalized
            };
            if let Some(reason) = check_protected_textual(&expanded) {
                return Some(deny_with_context(
                    input,
                    format!(
                        "+============================================================+\n\
                     |  BLOCKED: Bash Redirect to Protected Path                  |\n\
                     +============================================================+\n\
                     |  Target: {:<49}|\n\
                     |  Reason: {:<49}|\n\
                     |                                                            |\n\
                     |  Shell redirects (>, >>, tee, cp) to sentinel               |\n\
                     |  infrastructure paths are blocked during active workflows.  |\n\
                     +============================================================+",
                        if path.len() > 49 {
                            &path[path.len() - 49..]
                        } else {
                            path
                        },
                        reason,
                    ),
                ));
            }
        }
    }

    // Also check dd of= pattern
    for cap in DD_RE.captures_iter(cmd) {
        if let Some(path_match) = cap.get(1) {
            let path = path_match.as_str();
            let normalized = path.replace('\\', "/");
            let expanded = if normalized.starts_with("~/") || normalized.starts_with("~\\") {
                // **Attack #90 fix**: Panic instead of empty fallback — empty PathBuf
                // makes all ~/‐prefixed protected path checks silently pass.
                let home =
                    dirs::home_dir().expect("[sentinel] FATAL: Cannot determine home directory");
                format!(
                    "{}/{}",
                    home.to_string_lossy().replace('\\', "/"),
                    &normalized[2..]
                )
            } else {
                normalized
            };
            if let Some(reason) = check_protected_textual(&expanded) {
                return Some(deny_with_context(
                    input,
                    format!(
                        "+============================================================+\n\
                     |  BLOCKED: Bash Write to Protected Path (dd)                |\n\
                     +============================================================+\n\
                     |  Target: {:<49}|\n\
                     |  Reason: {:<49}|\n\
                     +============================================================+",
                        if path.len() > 49 {
                            &path[path.len() - 49..]
                        } else {
                            path
                        },
                        reason,
                    ),
                ));
            }
        }
    }

    None
}

/// Check if a Write or Edit targets any protected path during an active workflow.
///
/// Protected paths (when ANY workflow is active in session):
///   1. `~/.claude/skills/*/phases/*.md` — phase file tampering
///   2. `~/.claude/sentinel/` — config dir (workflows.toml, hooks.toml, state)
///   3. `~/.claude/settings.json` — hook registrations
///   4. `~/.claude/skills/` — skill definitions (blocks creating fake skills)
///
/// Uses both textual and canonical path checks to defeat symlink/junction bypass.
fn check_protected_path_write(
    state: &SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
    input: &HookInput,
) -> Option<HookOutput> {
    let tool_name = input.tool_name.as_deref()?;
    if tool_name != "Write" && tool_name != "Edit" && tool_name != "NotebookEdit" {
        return None;
    }

    // Only enforce when at least one workflow has been activated
    let any_active = !state.workflows.is_empty() || state.active_skill.is_some();
    if !any_active {
        return None;
    }

    // Also verify at least one workflow definition exists
    if workflows.is_empty() {
        return None;
    }

    let file_path = input
        .tool_input
        .as_ref()?
        .get("file_path")
        .and_then(|v| v.as_str())?;

    let normalized = file_path.replace('\\', "/");

    // Build protected directory list
    // **Attack #91 fix**: Panic instead of empty fallback
    let claude_dir = dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory")
        .join(".claude");

    // Check textual match first (fast path)
    let reason = check_protected_textual(&normalized);

    // Check canonical match (catches symlinks/junctions)
    let canonical_reason = check_protected_canonical(file_path, &claude_dir);

    let block_reason = reason.or(canonical_reason);

    if let Some(reason) = block_reason {
        // Allow editing SKILL.md files for skills OTHER than the active workflow skill.
        // The active skill's SKILL.md is always protected (prevents self-modification),
        // but non-active skills should remain editable during normal work.
        // Phase files (skills/*/phases/*.md) are ALWAYS protected regardless.
        if reason == "skill definition file" {
            if let Some(target_skill) = extract_skill_name_from_path(&normalized) {
                if let Some(active) = state.active_skill.as_deref() {
                    if target_skill != active {
                        return None; // Allow: editing a non-active skill's SKILL.md
                    }
                }
            }
        }

        let display_path = if normalized.len() > 45 {
            &normalized[normalized.len() - 45..]
        } else {
            &normalized
        };
        let message = format!(
            "+============================================================+\n\
             |  BLOCKED: Protected Path Modification Attempt               |\n\
             +============================================================+\n\
             |  Tool: {:<51}|\n\
             |  Target: {:<49}|\n\
             |  Reason: {:<49}|\n\
             |                                                            |\n\
             |  Modifying sentinel infrastructure (phase files, config,    |\n\
             |  hooks, state, settings) is prohibited during active        |\n\
             |  workflow sessions.                                        |\n\
             +============================================================+",
            tool_name, display_path, reason,
        );
        return Some(deny_with_context(input, message));
    }

    None
}

/// Classify an MCP tool as dangerous (write/exec capability).
///
/// MCP tools follow the pattern `mcp__<server>__<method>`. A tool is dangerous
/// if its method suffix indicates file writing, command execution, code patching,
/// or state mutation. Read-only tools (get, list, search, read) are safe.
///
/// This is a denylist approach — unknown suffixes default to DANGEROUS (fail-closed)
/// so new MCP servers don't automatically bypass enforcement.
fn is_dangerous_mcp_tool(tool_name: &str) -> bool {
    // Extract the method suffix (last segment after the final `__`)
    let suffix = tool_name
        .rsplit("__")
        .next()
        .unwrap_or(tool_name)
        .to_lowercase();

    // Explicitly safe suffixes — read-only operations
    let safe_suffixes = [
        "get",
        "list",
        "search",
        "read",
        "view",
        "show",
        "status",
        "info",
        "check",
        "verify",
        "validate",
        "count",
        "stats",
        "whoami",
        "viewer",
        "describe",
        "current_account",
        "list_accounts",
        "switch_account",
        "add_account",
        "remove_account",
        "download",
        "fetch",
        "describe",
        "discover",
        "screenshot",
        "pdf",
        "get_text",
        "is_visible",
        "wait",
        "wait_for_selector",
        "wait_for_navigation",
        "get_tabs",
        "list_instances",
        "mcp_restart_server",
        "sequentialthinking",
    ];

    // If the suffix starts with a safe prefix, allow it
    let safe_prefixes = [
        "get_", "list_", "search_", "read_", "check_", "resolve_", "verify_",
    ];

    if safe_suffixes.contains(&suffix.as_str()) {
        return false;
    }

    if safe_prefixes.iter().any(|p| suffix.starts_with(p)) {
        return false;
    }

    // Everything else is dangerous (fail-closed)
    true
}

/// Extract the skill name from a normalized path like `.../skills/loom/SKILL.md`.
/// Returns the skill directory name (e.g., "loom") or None if path doesn't match.
fn extract_skill_name_from_path(normalized: &str) -> Option<&str> {
    let parts: Vec<&str> = normalized.split('/').collect();
    let skills_idx = parts.iter().position(|p| *p == "skills")?;
    if skills_idx + 1 < parts.len() {
        Some(parts[skills_idx + 1])
    } else {
        None
    }
}

/// Textual path check for all protected paths.
/// Returns Some(reason) if blocked, None if allowed.
fn check_protected_textual(normalized: &str) -> Option<&'static str> {
    // 1. Phase files: skills/*/phases/*.md
    if normalized.contains("/skills/") && normalized.contains("/phases/") {
        let parts: Vec<&str> = normalized.split('/').collect();
        if let Some(skills_idx) = parts.iter().position(|p| *p == "skills") {
            if skills_idx + 3 < parts.len()
                && parts[skills_idx + 2] == "phases"
                && parts[skills_idx + 3].ends_with(".md")
            {
                return Some("phase file modification");
            }
        }
    }

    // 2. Sentinel config/state directory
    if normalized.contains("/.claude/sentinel/") || normalized.ends_with("/.claude/sentinel") {
        return Some("sentinel config/state directory");
    }

    // 3. Settings files (hook registrations)
    if normalized.ends_with("/.claude/settings.json")
        || normalized.contains("/.claude/settings.json")
        || normalized.ends_with("/.claude/sentinel-settings.json")
        || normalized.contains("/.claude/sentinel-settings.json")
    {
        return Some("hook registration file");
    }

    // 4. MCP server config (~/.claude.json — note: sibling of .claude/ dir, not inside it)
    // This is at HOME root, e.g., C:/Users/garys/.claude.json
    if normalized.ends_with("/.claude.json") {
        return Some("MCP server registration file");
    }

    // 5. Sentinel binary (shadow binary system)
    // **Attack #87 fix**: Protect sentinel-engine.exe and sentinel-engine.exe.staged
    // from Write/Edit during active workflows. An attacker could overwrite the
    // sentinel binary with a no-op stub, disabling all enforcement.
    if normalized.contains("/sentinel-engine") || normalized.contains("/.cargo/bin/sentinel") {
        return Some("sentinel binary file");
    }

    // 7. Sentinel source repo + infrastructure repos
    // **Attack #201 fix**: Protect the sentinel source code from modification during
    // active workflows. Only repos whose binaries auto-swap via the shadow binary
    // system are protected. vulcan-mcp-sdk-rust removed: it is a dependency, not enforcement.

    let infra_repos = [
        "/sentinel/crates/",
        "/sentinel/Cargo",
        "/sentinel/config/",
        "/mcp-router/",
    ];
    for pattern in &infra_repos {
        if normalized.contains(pattern) {
            return Some("sentinel infrastructure source code");
        }
    }

    // 6. Skill SKILL.md files (fake skill creation) and skill phases dirs
    if normalized.contains("/skills/") {
        let parts: Vec<&str> = normalized.split('/').collect();
        if let Some(skills_idx) = parts.iter().position(|p| *p == "skills") {
            // Block SKILL.md creation/modification
            if skills_idx + 2 < parts.len() && parts[skills_idx + 2] == "SKILL.md" {
                return Some("skill definition file");
            }
            // Block creating new phase directories/files for any skill
            if skills_idx + 2 < parts.len() && parts[skills_idx + 2] == "phases" {
                return Some("skill phases directory");
            }
        }
    }

    None
}

/// Canonical path check for protected paths.
/// Resolves symlinks/junctions and checks if real path is in protected areas.
fn check_protected_canonical(
    file_path: &str,
    claude_dir: &std::path::Path,
) -> Option<&'static str> {
    use std::path::Path;

    let target = Path::new(file_path);

    // Resolve parent directory (file may not exist yet for Write)
    let parent = target.parent()?;
    let canonical_parent = parent.canonicalize().ok()?;

    let claude_canonical = claude_dir
        .canonicalize()
        .unwrap_or_else(|_| claude_dir.to_path_buf());

    // Check if under ~/.claude/ at all
    if !canonical_parent.starts_with(&claude_canonical) {
        return None;
    }

    let relative = canonical_parent.strip_prefix(&claude_canonical).ok()?;
    let components: Vec<_> = relative.components().collect();

    // sentinel/ directory
    if components.first().and_then(|c| c.as_os_str().to_str()) == Some("sentinel") {
        return Some("sentinel config/state directory");
    }

    // skills/ directory checks
    if components.first().and_then(|c| c.as_os_str().to_str()) == Some("skills") {
        // skills/{name}/phases/ — phase file
        if components.len() >= 3 && components[2].as_os_str().to_str() == Some("phases") {
            return Some("phase file modification");
        }

        // skills/{name}/ — check if target is SKILL.md
        if components.len() >= 2 {
            let filename = target.file_name()?.to_str()?;
            if filename == "SKILL.md" {
                return Some("skill definition file");
            }
        }
    }

    // settings.json / sentinel-settings.json at ~/.claude/ root
    if components.is_empty() {
        let filename = target.file_name()?.to_str()?;
        if filename == "settings.json" || filename == "sentinel-settings.json" {
            return Some("hook registration file");
        }
    }

    // .claude.json at HOME root (parent of ~/.claude/)
    // canonical_parent is HOME dir, filename is .claude.json
    // **Attack #92 fix**: Panic instead of empty fallback
    let home_dir = dirs::home_dir().expect("[sentinel] FATAL: Cannot determine home directory");
    let home_canonical = home_dir.canonicalize().unwrap_or(home_dir);
    if canonical_parent == home_canonical {
        let filename = target.file_name()?.to_str()?;
        if filename == ".claude.json" {
            return Some("MCP server registration file");
        }
    }

    None
}

/// Check for post-merge phase skip:
/// If review.md has been read (or review phase completed) but qa-handoff.md
/// has not been read, block non-safe tools. This catches the case where
/// Claude tries to skip QA after code review.
fn check_post_merge_skip(
    state: &SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
    input: &HookInput,
    tool_name: &str,
) -> Option<HookOutput> {
    // Only check safe-tool-exempt tools
    // **Attack #78 fix**: Removed "Task" and "Agent" from safe tools here too.
    // **Attack #101 fix**: NotebookEdit is NOT safe — it can modify code cells.
    // **Attack #107 fix**: Removed "Skill" — can trigger nested workflow bypass.
    // **Attack #108 fix**: Removed "SendMessage" — can leak context to teammates.
    // This list MUST stay in sync with workflow.rs should_block() safe_tools.
    let safe_tools = [
        "Read",
        "Glob",
        "Grep",
        "WebSearch",
        "WebFetch",
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
        "TaskCreate",
        "TaskUpdate",
        "TaskList",
        "TaskGet",
        "TaskOutput",
        "ToolSearch",
    ];
    if safe_tools.contains(&tool_name) {
        return None;
    }

    // **Attack #48**: Use active_skill if available, otherwise fall back to
    // find_incomplete_workflow to prevent clearing active_skill to bypass this check.
    let (skill, workflow) = match state
        .active_skill
        .as_ref()
        .and_then(|s| workflows.get(s).map(|w| (s.clone(), w)))
    {
        Some(pair) => pair,
        None => {
            // Fall back to most-progressed incomplete workflow (mirrors gate.rs logic)
            match crate::gate::find_incomplete_workflow_pub(state, workflows, None) {
                Some((wf, _ws, skill_name)) => (skill_name, wf),
                None => return None,
            }
        }
    };

    // Check if this workflow has both review and qa-handoff phases
    let has_review = workflow.phases.iter().any(|p| p.id == "review");
    let has_qa = workflow.phases.iter().any(|p| p.id == "qa-handoff");
    if !has_review || !has_qa {
        return None;
    }

    // Check: review.md loaded but qa-handoff.md NOT loaded (per-skill)
    let review_read = state.has_phase_been_read(&skill, "review.md");
    let qa_read = state.has_phase_been_read(&skill, "qa-handoff.md");

    // Also check completed phases (from submit_phase_complete)
    // Use the resolved skill name, not active_workflow() which depends on active_skill
    let review_complete = state
        .workflows
        .get(&skill)
        .map(|w| w.is_phase_complete("review"))
        .unwrap_or(false);

    if (review_read || review_complete) && !qa_read {
        let message = format!(
            "\
+============================================================+
|  BLOCKED: Post-Merge Phase Skip Detected                   |
+============================================================+
|  review.md has been loaded but qa-handoff.md has NOT.       |
|                                                            |
|  After code review, you MUST load the QA handoff phase     |
|  before making any further tool calls.                     |
|                                                            |
|  MANDATORY: Read(\"~/.claude/skills/{}/phases/qa-handoff.md\")|
+============================================================+",
            skill
        );
        return Some(deny_with_context(input, message));
    }

    None
}

/// Format a visually prominent block message box
fn format_block_box(
    skill: &str,
    reason: &str,
    next_phase: &str,
    next_phase_file: &str,
    completed: usize,
    total: usize,
) -> String {
    format!(
        "\
+============================================================+
|  BLOCKED: Phase Gate Violation                             |
+============================================================+
|  Skill: {skill:<50}|
|  Progress: {completed}/{total} required phases completed              |
|                                                            |
|  Reason: {reason:<49}|
|                                                            |
|  Next required phase: {next_phase:<35}|
|  Read(\"~/.claude/skills/{skill}/phases/{next_phase_file}\")    |
+============================================================+"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::judge::JudgeModel;
    use sentinel_domain::workflow::WorkflowPhase;
    use std::path::{Path, PathBuf};

    /// Real filesystem port for tests — preserves pre-refactor behavior
    /// by delegating to actual `dirs::home_dir()` and `Path::exists()`.
    struct RealTestFs;
    impl super::super::FileSystemPort for RealTestFs {
        fn home_dir(&self) -> Option<PathBuf> { dirs::home_dir() }
        fn read_to_string(&self, p: &Path) -> anyhow::Result<String> { Ok(std::fs::read_to_string(p)?) }
        fn write(&self, p: &Path, content: &[u8]) -> anyhow::Result<()> {
            if let Some(parent) = p.parent() { std::fs::create_dir_all(parent)?; }
            Ok(std::fs::write(p, content)?)
        }
        fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> { Ok(std::fs::create_dir_all(p)?) }
        fn read_dir(&self, p: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(std::fs::read_dir(p)?.filter_map(|e| e.ok().map(|e| e.path())).collect())
        }
        fn exists(&self, p: &Path) -> bool { p.exists() }
        fn is_dir(&self, p: &Path) -> bool { p.is_dir() }
        fn metadata(&self, p: &Path) -> anyhow::Result<std::fs::Metadata> { Ok(std::fs::metadata(p)?) }
        fn append(&self, p: &Path, content: &[u8]) -> anyhow::Result<()> {
            use std::io::Write;
            if let Some(parent) = p.parent() { std::fs::create_dir_all(parent)?; }
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            f.write_all(content)?;
            Ok(())
        }
    }

    fn test_fs() -> RealTestFs { RealTestFs }

    fn test_workflow() -> SkillWorkflow {
        SkillWorkflow {
            skill: "linear".to_string(),
            phases: vec![
                WorkflowPhase {
                    id: "claim".to_string(),
                    file: "claim.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: "Claim the issue".to_string(),
                },
                WorkflowPhase {
                    id: "fetch".to_string(),
                    file: "fetch.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: "Fetch details".to_string(),
                },
                WorkflowPhase {
                    id: "review".to_string(),
                    file: "review.md".to_string(),
                    required: true,
                    judge: JudgeModel::Opus,
                    description: "Code review".to_string(),
                },
                WorkflowPhase {
                    id: "qa-handoff".to_string(),
                    file: "qa-handoff.md".to_string(),
                    required: true,
                    judge: JudgeModel::Opus,
                    description: "QA handoff".to_string(),
                },
            ],
            blocked_tool_prefixes: Vec::new(),
            blocked_bash_patterns: Vec::new(),
            bash_allowlist: Vec::new(),
        }
    }

    #[test]
    fn test_allows_when_no_active_skill() {
        let mut state = SessionState::new("sess-1");
        let workflows = HashMap::new();
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows, &test_fs());
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_safe_tools() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Glob".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows, &test_fs());
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_when_no_phase_files_on_disk() {
        // Phase gate skips enforcement when phase files don't exist on disk.
        // Use a fake skill name that definitely has no phase files.
        let fake_workflow = SkillWorkflow {
            skill: "nonexistent-test-skill".to_string(),
            phases: vec![WorkflowPhase {
                id: "setup".to_string(),
                file: "setup.md".to_string(),
                required: true,
                judge: JudgeModel::Sonnet,
                description: "Setup".to_string(),
            }],
            blocked_tool_prefixes: Vec::new(),
            blocked_bash_patterns: Vec::new(),
            bash_allowlist: Vec::new(),
        };
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("nonexistent-test-skill");
        let mut workflows = HashMap::new();
        workflows.insert("nonexistent-test-skill".to_string(), fake_workflow);

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows, &test_fs());
        // No phase files on disk → gate allows
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_blocks_when_phase_files_exist() {
        // Phase gate enforces when phase files exist on disk.
        // Uses "linear" which has real phase files at ~/.claude/skills/linear/phases/
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows, &test_fs());

        // Check if linear phase files exist on this machine
        let claim_path = dirs::home_dir()
            .unwrap_or_default()
            .join(".claude/skills/linear/phases/claim.md");
        if claim_path.exists() {
            // Phase files exist → gate blocks (deny() puts reason in hook_specific_output)
            assert_eq!(output.blocked, Some(true));
            let reason = output
                .hook_specific_output
                .as_ref()
                .and_then(|h| h.permission_decision_reason.as_deref())
                .or(output.reason.as_deref())
                .unwrap();
            assert!(reason.contains("BLOCKED") || reason.contains("claim"));
        } else {
            // No phase files → gate allows (CI/other machines)
            assert!(output.blocked.is_none());
        }
    }

    #[test]
    fn test_read_on_phase_file_records_and_advances() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        // Use a real absolute path so canonicalize() succeeds (trusted = true)
        let claim_path = dirs::home_dir()
            .unwrap()
            .join(".claude/skills/linear/phases/claim.md");

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": claim_path.to_string_lossy()
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows, &test_fs());
        // Read should always be allowed
        assert!(output.blocked.is_none());
        // Should record the phase read
        assert!(state.has_phase_been_read("linear", "claim.md"));

        if claim_path.exists() {
            // File exists on disk → trusted → workflow advances
            let wf_state = state.workflows.get("linear").unwrap();
            assert!(wf_state.is_phase_complete("claim"));
        } else {
            // File doesn't exist (CI/other machines) → untrusted → no advance
            assert!(
                state.workflows.get("linear").is_none()
                    || !state
                        .workflows
                        .get("linear")
                        .unwrap()
                        .is_phase_complete("claim")
            );
        }
    }

    #[test]
    fn test_read_derives_skill_from_path() {
        // Even if active_skill is different, the phase advance should use
        // the skill derived from the path, not active_skill.
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("some-other-skill");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let claim_path = dirs::home_dir()
            .unwrap()
            .join(".claude/skills/linear/phases/claim.md");

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": claim_path.to_string_lossy()
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows, &test_fs());
        assert!(output.blocked.is_none());
        assert!(state.has_phase_been_read("linear", "claim.md"));

        if claim_path.exists() {
            // File exists → trusted → should advance "linear" (from path), not "some-other-skill"
            let wf_state = state.workflows.get("linear").unwrap();
            assert!(wf_state.is_phase_complete("claim"));
        }
        // If file doesn't exist → untrusted → no advance (still OK, phases_read recorded)
    }

    #[test]
    fn test_read_rejects_unknown_phase_id() {
        // Reading a .md file that isn't a known phase should NOT advance workflow
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let evil_path = dirs::home_dir()
            .unwrap()
            .join(".claude/skills/linear/phases/evil-phase.md");

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": evil_path.to_string_lossy()
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows, &test_fs());
        assert!(output.blocked.is_none());
        // File does NOT exist on disk → untrusted → NOT recorded in phases_read
        // (Patch 23: only trusted reads are recorded to prevent progress inflation)
        assert!(!state.has_phase_been_read("linear", "evil-phase.md"));
        // No workflow state should be created for unknown phases
        assert!(
            state.workflows.get("linear").is_none()
                || !state
                    .workflows
                    .get("linear")
                    .unwrap()
                    .is_phase_complete("evil-phase")
        );
    }

    #[test]
    fn test_read_rejects_path_traversal() {
        let mut state = SessionState::new("sess-1");
        let workflows = HashMap::new();

        let traversal_path = dirs::home_dir()
            .unwrap()
            .join(".claude/skills/linear/phases/../../secrets/claim.md");

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": traversal_path.to_string_lossy()
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows, &test_fs());
        assert!(output.blocked.is_none());
        // Path traversal should be rejected (ParentDir component detected) — no phase recorded
        assert!(state.phases_read.is_empty());
    }

    #[test]
    #[cfg(windows)]
    fn test_read_on_windows_path_records_phase() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": "C:\\Users\\garys\\.claude\\skills\\linear\\phases\\fetch.md"
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows, &test_fs());
        assert!(output.blocked.is_none());
        assert!(state.has_phase_been_read("linear", "fetch.md"));
    }

    #[test]
    fn test_read_on_non_phase_file_ignored() {
        let mut state = SessionState::new("sess-1");
        let workflows = HashMap::new();

        let skill_path = dirs::home_dir()
            .unwrap()
            .join(".claude/skills/linear/SKILL.md");

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": skill_path.to_string_lossy()
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows, &test_fs());
        assert!(output.blocked.is_none());
        assert!(state.phases_read.is_empty());
    }

    #[test]
    fn test_post_merge_skip_blocks() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        // Mark review as read but not qa-handoff
        state.record_phase_read("linear", "claim.md");
        state.record_phase_read("linear", "fetch.md");
        state.record_phase_read("linear", "review.md");

        // Complete claim, fetch, review phases so gate doesn't block on those
        let tw = test_workflow();
        if let Some(wf) = state.workflows.get_mut("linear") {
            wf.advance_sequential("claim", &tw);
            wf.advance_sequential("fetch", &tw);
            wf.advance_sequential("review", &tw);
        }

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows, &test_fs());
        assert_eq!(output.blocked, Some(true));
        // deny() puts reason in hook_specific_output.permission_decision_reason
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref())
            .or(output.reason.as_deref())
            .unwrap();
        assert!(reason.contains("Post-Merge"));
        assert!(reason.contains("qa-handoff.md"));
    }

    #[test]
    fn test_post_merge_skip_allows_when_qa_read() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        state.record_phase_read("linear", "claim.md");
        state.record_phase_read("linear", "fetch.md");
        state.record_phase_read("linear", "review.md");
        state.record_phase_read("linear", "qa-handoff.md");

        // Complete all phases
        let tw = test_workflow();
        if let Some(wf) = state.workflows.get_mut("linear") {
            wf.advance_sequential("claim", &tw);
            wf.advance_sequential("fetch", &tw);
            wf.advance_sequential("review", &tw);
            wf.advance_sequential("qa-handoff", &tw);
        }

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows, &test_fs());
        // Should be allowed — all phases read
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_untrusted_file_does_not_advance_workflow() {
        // A phase file path that doesn't exist on disk should be recorded
        // in phases_read but should NOT advance workflow state (untrusted).
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        // Use a path with a nonexistent parent dir so the file definitely doesn't exist
        let fake_path = dirs::home_dir()
            .unwrap()
            .join(".claude/skills/linear/phases/claim.md");

        // Only run this test if the file does NOT exist (CI environments)
        // On dev machines where the file exists, skip this variant
        if !fake_path.exists() {
            let input = HookInput {
                tool_name: Some("Read".to_string()),
                tool_input: Some(serde_json::json!({
                    "file_path": fake_path.to_string_lossy()
                })),
                ..Default::default()
            };
            let output = process(&input, &mut state, &workflows, &test_fs());
            assert!(output.blocked.is_none());
            // File recorded for tracking
            assert!(state.has_phase_been_read("linear", "claim.md"));
            // But workflow NOT advanced (untrusted — file doesn't exist)
            assert!(
                state.workflows.get("linear").is_none()
                    || !state
                        .workflows
                        .get("linear")
                        .unwrap()
                        .is_phase_complete("claim")
            );
        }
    }

    #[test]
    fn test_tool_call_counter_increments() {
        let mut state = SessionState::new("sess-1");
        let workflows = HashMap::new();

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        process(&input, &mut state, &workflows, &test_fs());
        process(&input, &mut state, &workflows, &test_fs());
        assert_eq!(state.tool_calls, 2);
    }

    #[test]
    fn test_dangerous_mcp_tools_classified_correctly() {
        // Write/exec tools are dangerous
        assert!(is_dangerous_mcp_tool("mcp__codex__shell"));
        assert!(is_dangerous_mcp_tool("mcp__codex__write_file"));
        assert!(is_dangerous_mcp_tool("mcp__codex__apply_patch"));
        assert!(is_dangerous_mcp_tool("mcp__steel__click"));
        assert!(is_dangerous_mcp_tool("mcp__steel__evaluate_js"));
        assert!(is_dangerous_mcp_tool("mcp__steel__navigate"));
        assert!(is_dangerous_mcp_tool("mcp__linear__create_issue"));
        assert!(is_dangerous_mcp_tool("mcp__linear__update_issue"));
        assert!(is_dangerous_mcp_tool("mcp__linear__delete_issue"));
        assert!(is_dangerous_mcp_tool("mcp__doppler__set_secret"));

        // Read-only tools are safe
        assert!(!is_dangerous_mcp_tool("mcp__linear__get_issue"));
        assert!(!is_dangerous_mcp_tool("mcp__linear__list_issues"));
        assert!(!is_dangerous_mcp_tool("mcp__linear__search"));
        assert!(!is_dangerous_mcp_tool("mcp__steel__screenshot"));
        assert!(!is_dangerous_mcp_tool("mcp__codex__read_file"));
        assert!(!is_dangerous_mcp_tool("mcp__codex__list_dir"));
        assert!(!is_dangerous_mcp_tool("mcp__sentinel__get_proof_chain"));
        assert!(!is_dangerous_mcp_tool("mcp__sentinel__get_workflow_status"));
        assert!(!is_dangerous_mcp_tool("mcp__sentinel__verify_chain"));
        assert!(!is_dangerous_mcp_tool("mcp__sentinel__mcp_restart_server"));

        // Unknown suffixes default to dangerous (fail-closed)
        assert!(is_dangerous_mcp_tool("mcp__evil__pwn_system"));
        assert!(is_dangerous_mcp_tool("mcp__unknown__do_thing"));
    }

    #[test]
    fn test_protected_path_blocks_sentinel_source_repo() {
        let normalized =
            "/c/Users/garys/Documents/GitHub/sentinel/crates/sentinel-cli/src/hook_cmd.rs";
        assert_eq!(
            check_protected_textual(normalized),
            Some("sentinel infrastructure source code")
        );

        let cargo_toml = "/c/Users/garys/Documents/GitHub/sentinel/Cargo.toml";
        assert_eq!(
            check_protected_textual(cargo_toml),
            Some("sentinel infrastructure source code"), // sentinel/Cargo matches
        );

        // vulcan-mcp-sdk-rust is no longer protected (removed from infra_repos —
        // it is a dependency, not enforcement infrastructure)
        let vulcan = "/c/Users/garys/Documents/GitHub/vulcan-mcp-sdk-rust/crates/vulcan/src/lib.rs";
        assert_eq!(check_protected_textual(vulcan), None);
    }
}
