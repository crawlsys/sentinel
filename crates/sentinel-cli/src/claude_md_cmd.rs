//! CLAUDE.md management commands.
//!
//! Three operations exposed both as CLI subcommands (`sentinel
//! regenerate-claude-md`, `sentinel edit-claude-md-template`, `sentinel
//! restart-all-mcps`) and as MCP tools (`mcp__sentinel__regenerate_claude_md`,
//! `mcp__sentinel__edit_claude_md_template`, `mcp__sentinel__restart_all_mcps`).
//!
//! Both surfaces call the same functions here — keep the shared contract.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use sentinel_application::hooks::session_init;

/// Regenerate `~/.claude/CLAUDE.md` from the compiled template.
///
/// Delegates to `session_init::regenerate_global_claude_md`, which re-counts
/// components, re-reads project configs and Linear accounts, and overwrites
/// the file. Returns the path written plus the new byte count.
pub fn regenerate() -> Result<Value> {
    let path = session_init::regenerate_global_claude_md();
    let bytes = fs::metadata(&path)
        .with_context(|| format!("reading metadata for {}", path.display()))?
        .len();
    Ok(json!({
        "path": path.display().to_string(),
        "bytes": bytes,
    }))
}

/// Apply a find-and-replace to the compiled template source (session_init.rs).
///
/// **Safety contract** — mirrors the `Edit` tool's "old_string must be unique"
/// rule. Refuses to modify the file if:
///   * `find` is empty,
///   * `find` equals `replace` (no-op),
///   * `find` does not appear in the file,
///   * `find` appears more than once (ambiguous replacement).
///
/// After the source edit succeeds, calls [`regenerate`] so the live mirror at
/// `~/.claude/CLAUDE.md` reflects the change. Note: the COMPILED template only
/// updates on the next `cargo build --release -p sentinel` + `sentinel stage`
/// — this function's edit is the persistent source-of-truth change, but
/// future regenerations from the currently-running engine will still use the
/// old compiled text until the binary is swapped.
pub fn edit_template(find: &str, replace: &str) -> Result<Value> {
    if find.is_empty() {
        return Err(anyhow!("`find` must not be empty"));
    }
    if find == replace {
        return Err(anyhow!(
            "`find` and `replace` are identical — nothing to do"
        ));
    }

    let template_path = session_init::template_source_path();
    edit_template_at(&template_path, find, replace)?;

    // Regenerate the live mirror so at least the MD file is in sync even if
    // the user hasn't rebuilt/staged yet.
    let regen = regenerate()?;

    Ok(json!({
        "template_path": template_path.display().to_string(),
        "regenerated": regen,
        "next_step": "Rebuild (`cargo build --release -p sentinel`) and stage \
                      (`sentinel stage`) so future regenerations use the updated template.",
    }))
}

/// Inner helper parameterised on the template path so tests can redirect
/// without needing to touch the real source file.
fn edit_template_at(path: &Path, find: &str, replace: &str) -> Result<()> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading template at {}", path.display()))?;

    let count = content.matches(find).count();
    match count {
        0 => Err(anyhow!(
            "`find` string not present in template at {}",
            path.display()
        )),
        1 => {
            let new_content = content.replacen(find, replace, 1);
            fs::write(path, new_content)
                .with_context(|| format!("writing template at {}", path.display()))?;
            Ok(())
        }
        n => Err(anyhow!(
            "`find` appears {n} times in template — provide a longer, unique \
             substring so the replacement is unambiguous"
        )),
    }
}

/// Trigger a mass restart of every mcp-router-wrapped MCP server.
///
/// Reads `~/.claude.json`, walks `mcpServers.*.command`, and for each entry of
/// the form `mcp-router --single <name>` (or the `.exe` variant), resolves
/// `<name>.exe` (or the plain name on Unix) on `PATH`/`~/.cargo/bin` and bumps
/// its mtime. mcp-router watches the binary file and auto-restarts the server
/// on mtime change, sending `notifications/tools/list_changed` so Claude Code
/// refreshes the tool list in-session without a session restart.
///
/// Servers whose binary cannot be found on disk are reported but do not fail
/// the call — this is an admin action and we want best-effort behaviour.
pub fn restart_all_mcps() -> Result<Value> {
    let config_path = user_claude_json_path()?;
    let extra_dirs: Vec<PathBuf> = dirs::home_dir()
        .map(|h| vec![h.join(".cargo").join("bin")])
        .unwrap_or_default();
    restart_all_mcps_with(&config_path, &extra_dirs)
}

/// Inner implementation with explicit config path + extra resolver dirs so
/// tests can point at a fixture without racing `dirs::home_dir` (which on
/// Windows bypasses the `HOME`/`USERPROFILE` env and reads the user profile
/// via the FOLDERID_Profile API).
fn restart_all_mcps_with(config_path: &Path, extra_dirs: &[PathBuf]) -> Result<Value> {
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let parsed: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {} as JSON", config_path.display()))?;

    let servers = parsed
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("no `mcpServers` object in {}", config_path.display()))?;

    let mut touched: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();

    for (name, entry) in servers {
        let Some(binary_stem) = mcp_router_single_target(entry) else {
            skipped.push(json!({
                "server": name,
                "reason": "not an mcp-router --single <binary> entry",
            }));
            continue;
        };

        match resolve_binary(&binary_stem, extra_dirs) {
            Some(path) => match bump_mtime(&path) {
                Ok(()) => touched.push(json!({
                    "server": name,
                    "binary": binary_stem,
                    "path": path.display().to_string(),
                })),
                Err(e) => skipped.push(json!({
                    "server": name,
                    "binary": binary_stem,
                    "reason": format!("mtime bump failed: {e}"),
                })),
            },
            None => skipped.push(json!({
                "server": name,
                "binary": binary_stem,
                "reason": "binary not found on PATH or in ~/.cargo/bin",
            })),
        }
    }

    Ok(json!({
        "touched": touched,
        "skipped": skipped,
        "touched_count": touched.len(),
        "skipped_count": skipped.len(),
    }))
}

/// Path to `~/.claude.json`. Returns an error if $HOME/$USERPROFILE is unset.
fn user_claude_json_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;
    Ok(home.join(".claude.json"))
}

/// Inspect an `mcpServers` entry and, if its command is
/// `mcp-router --single <binary>` (with or without `.exe`), return `<binary>`.
///
/// Handles both shapes observed in `~/.claude.json`:
///   * `"command": "mcp-router --single linear-mcp"` (single string)
///   * `"command": "mcp-router", "args": ["--single", "linear-mcp"]` (split)
fn mcp_router_single_target(entry: &Value) -> Option<String> {
    let command = entry.get("command").and_then(|v| v.as_str())?;

    // Split the `command` field on whitespace first. If args are embedded,
    // we'll see `["mcp-router", "--single", "<name>"]`.
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut words: Vec<&str> = tokens;

    // If `args` array is present, append its strings so we can scan them too.
    let args: Vec<&str> = entry
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    words.extend(args);

    let is_router = words
        .first()
        .is_some_and(|w| strip_exe(w) == "mcp-router");
    if !is_router {
        return None;
    }

    let mut iter = words.iter().skip(1);
    while let Some(tok) = iter.next() {
        if *tok == "--single" {
            if let Some(next) = iter.next() {
                return Some((*next).to_string());
            }
        }
    }
    None
}

/// Resolve `<stem>` to a full path on disk.
///
/// If `stem` is already an absolute path that exists, return it verbatim
/// (covers `~/.claude.json` entries that pre-resolve the binary path inside
/// `--single`). Otherwise probe, in order: `PATH` entries + `extra_dirs`.
/// On Windows we try `<stem>.exe` first.
fn resolve_binary(stem: &str, extra_dirs: &[PathBuf]) -> Option<PathBuf> {
    let as_path = Path::new(stem);
    if as_path.is_absolute() && as_path.is_file() {
        return Some(as_path.to_path_buf());
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_env) {
            candidates.push(dir);
        }
    }
    candidates.extend(extra_dirs.iter().cloned());

    let exts: &[&str] = if cfg!(windows) { &["exe", ""] } else { &[""] };

    for dir in candidates {
        for ext in exts {
            let file = if ext.is_empty() {
                dir.join(stem)
            } else {
                dir.join(format!("{stem}.{ext}"))
            };
            if file.is_file() {
                return Some(file);
            }
        }
    }
    None
}

/// Bump the mtime of `path` to `now` so mcp-router's file watcher fires.
fn bump_mtime(path: &Path) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} for mtime bump", path.display()))?;
    file.set_modified(SystemTime::now())
        .with_context(|| format!("setting mtime on {}", path.display()))?;
    Ok(())
}

fn strip_exe(w: &str) -> &str {
    w.strip_suffix(".exe").unwrap_or(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests that touch process env must serialise across cargo's parallel
    // test harness.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn edit_template_refuses_empty_find() {
        let err = edit_template("", "x").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn edit_template_refuses_noop() {
        let err = edit_template("same", "same").unwrap_err();
        assert!(err.to_string().contains("identical"));
    }

    #[test]
    fn edit_template_at_applies_unique_replacement() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "hello WORLD goodbye").unwrap();
        edit_template_at(tmp.path(), "WORLD", "PLANET").unwrap();
        let after = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(after, "hello PLANET goodbye");
    }

    #[test]
    fn edit_template_at_refuses_non_unique() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "abc abc abc").unwrap();
        let err = edit_template_at(tmp.path(), "abc", "xyz").unwrap_err();
        assert!(err.to_string().contains("appears 3 times"));
    }

    #[test]
    fn edit_template_at_refuses_missing() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "nothing to see").unwrap();
        let err = edit_template_at(tmp.path(), "absent-token", "x").unwrap_err();
        assert!(err.to_string().contains("not present"));
    }

    #[test]
    fn mcp_router_single_target_parses_inline_command() {
        let v = json!({ "command": "mcp-router --single linear-mcp" });
        assert_eq!(mcp_router_single_target(&v).as_deref(), Some("linear-mcp"));
    }

    #[test]
    fn mcp_router_single_target_parses_split_args() {
        let v = json!({
            "command": "mcp-router",
            "args": ["--single", "linear-mcp"]
        });
        assert_eq!(mcp_router_single_target(&v).as_deref(), Some("linear-mcp"));
    }

    #[test]
    fn mcp_router_single_target_parses_exe_suffix() {
        let v = json!({ "command": "mcp-router.exe --single doppler-mcp" });
        assert_eq!(
            mcp_router_single_target(&v).as_deref(),
            Some("doppler-mcp")
        );
    }

    #[test]
    fn mcp_router_single_target_rejects_non_router() {
        let v = json!({ "command": "node server.js" });
        assert_eq!(mcp_router_single_target(&v), None);
    }

    #[test]
    fn mcp_router_single_target_rejects_missing_single_flag() {
        let v = json!({ "command": "mcp-router serve" });
        assert_eq!(mcp_router_single_target(&v), None);
    }

    #[test]
    fn restart_all_mcps_errors_when_config_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("nonexistent.claude.json");
        let err = restart_all_mcps_with(&config_path, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("reading"), "expected read error, got: {msg}");
    }

    #[test]
    fn restart_all_mcps_touches_found_binaries_and_skips_missing() {
        // ENV_LOCK guards the PATH mutation below so parallel tests don't
        // race. Binary resolution still uses fixture dirs, not real $HOME.
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner()); // recover poisoned lock

        let tmp = tempfile::TempDir::new().unwrap();
        let prev_path = std::env::var_os("PATH");

        // Stage one real binary in a fake bin dir; leave another one absent.
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let present_name = if cfg!(windows) { "real-mcp.exe" } else { "real-mcp" };
        let present_path = bin_dir.join(present_name);
        std::fs::write(&present_path, b"#!/bin/sh\n").unwrap();

        // Isolate PATH to JUST the fixture dir so no real system binary
        // can accidentally match (e.g. a globally-installed ghost-mcp).
        std::env::set_var("PATH", &bin_dir);

        let config = json!({
            "mcpServers": {
                "real":    { "command": "mcp-router --single real-mcp" },
                "missing": { "command": "mcp-router --single ghost-mcp" },
                "other":   { "command": "node /opt/app.js" }
            }
        });
        let config_path = tmp.path().join(".claude.json");
        std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

        // Stamp an old mtime on the real binary so we can observe the bump.
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&present_path)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let result = restart_all_mcps_with(&config_path, &[]).unwrap();

        // Restore PATH before any assertion that could panic — poisoning is
        // handled by the `into_inner` fallback above but this keeps the env
        // clean for neighbouring tests.
        match prev_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }

        let touched = result["touched"].as_array().unwrap();
        let skipped = result["skipped"].as_array().unwrap();
        assert_eq!(touched.len(), 1, "expected 1 touched, got {touched:?}");
        assert_eq!(touched[0]["server"], "real");
        assert_eq!(skipped.len(), 2, "expected 2 skipped, got {skipped:?}");

        let new_mtime = std::fs::metadata(&present_path).unwrap().modified().unwrap();
        assert!(new_mtime > old, "mtime should have advanced");
    }
}
