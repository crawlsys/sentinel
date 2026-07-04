//! MCP Registration Guardian — detect + snapshot + heal for `~/.claude.json`.
//!
//! The `mcpServers` block in `~/.claude.json` is the live registry Claude Code
//! reads at session start. It has been lost before (Jun 17-18 corruption /
//! rebuild) and the loss was invisible because every failure mode was coerced
//! to a `0` count. This module is sentinel's control loop over that block:
//!
//! 1. **Detect** — [`crate::scanner::mcp_registry_state`] classifies the live
//!    registry as `Missing` / `Unreadable` / `Tampered` / `Count(n)`.
//! 2. **Snapshot** — when the registry is present and healthy (>=
//!    [`SNAPSHOT_FLOOR`] entries), a dated known-good copy of the `mcpServers`
//!    object is written to `~/.claude/sentinel/state/mcp-registry/` (one per
//!    day, newest [`SNAPSHOT_KEEP`] kept).
//! 3. **Heal** — when the registry is `Missing`, `Tampered`, or empty while
//!    MCP repos exist on disk, the declarative registry from the marketplace
//!    clone's `marketplace.json` (REGISTRY CONTRACT v1) is merged back into
//!    the live config file(s). Falls back to the newest state snapshot when
//!    the marketplace declaration is absent or invalid.
//! 4. **Alert** — callers (session_init) surface a loud banner line and a
//!    channel event; the standing `/reload-plugins` `initialUserMessage`
//!    autoheal in session_init reconnects the healed servers in-session.
//!
//! Architecture split (deliberate): sentinel OWNS detect+snapshot+heal+alert;
//! the marketplace repo is DECLARATIVE ONLY (its `marketplace.json` `mcp[]`
//! entries plus a `retired` tombstone array); the claude-code-handler is
//! zero-touch.
//!
//! ## Registry contract v1 (what the marketplace declares)
//!
//! ```json
//! {
//!   "mcp": [
//!     { "name": "linear",
//!       "command": "mcp-supervisor",
//!       "args": ["mcp-router", "--single", "C:/.../linear-mcp.exe", "--watch", "C:/.../linear-mcp.exe"],
//!       "transport": "stdio",
//!       "env": { "RUST_LOG": "info", "LINEAR_API_KEY": "$doppler:firefly/dev/LINEAR_API_KEY" } }
//!   ],
//!   "retired": ["agents", "skills"]
//! }
//! ```
//!
//! `$doppler:<project>/<config>/<SECRET>` env refs are resolved at heal time by
//! shelling out to `doppler-rs` (falling back to `doppler`). The marketplace
//! repo never carries literal secrets; the healed `~/.claude.json` may (it is
//! local-only). A ref that fails to resolve is omitted with a warning —
//! a degraded registration is better than an absent one.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use sentinel_domain::ports::{EnvPort, ProcessPort};

use crate::scanner::{self, McpRegistryState};

/// Minimum number of live entries for the registry to be considered a sane,
/// snapshot-worthy known-good state.
pub const SNAPSHOT_FLOOR: usize = 10;

/// Number of dated snapshots to retain (newest first).
pub const SNAPSHOT_KEEP: usize = 14;

/// Registration names permanently tombstoned regardless of what a marketplace
/// declaration or an old snapshot carries. `agents` and `skills` were merged
/// into the unified `sentinel-mcp` server (marketplace commit fbd2f90) and
/// must never be healed back. The marketplace's own `retired` array is
/// unioned with this list at heal time.
pub const RETIRED_BUILTIN: &[&str] = &["agents", "skills"];

/// Where the heal registry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealSource {
    /// The marketplace clone's `marketplace.json` `mcp[]` declaration.
    Marketplace,
    /// The newest dated known-good snapshot under `sentinel/state/mcp-registry/`.
    Snapshot,
}

impl std::fmt::Display for HealSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Marketplace => write!(f, "marketplace"),
            Self::Snapshot => write!(f, "snapshot"),
        }
    }
}

/// Result of a heal attempt that actually merged something.
#[derive(Debug)]
pub struct HealOutcome {
    /// Where the desired registry came from.
    pub source: HealSource,
    /// Number of server entries merged.
    pub entries: usize,
    /// Config files the block was merged into.
    pub merged_files: Vec<PathBuf>,
    /// `server.VAR` env refs that failed doppler resolution and were omitted.
    pub unresolved_env: Vec<String>,
}

/// Full report from one guardian pass.
#[derive(Debug)]
pub struct GuardianReport {
    /// Classified state of the live registry before any heal.
    pub state: McpRegistryState,
    /// Number of `*-mcp-rust` repos on disk (context for the zero-count check).
    pub mcp_repos: usize,
    /// True when the registration is Missing/Unreadable/Tampered or empty
    /// while MCP repos exist — callers must surface this loudly.
    pub tripwire: bool,
    /// True when a new dated snapshot was written this pass.
    pub snapshot_written: bool,
    /// Present when a heal merged entries into at least one config file.
    pub heal: Option<HealOutcome>,
    /// Non-fatal problems encountered (doppler failures, unwritable targets…).
    pub warnings: Vec<String>,
}

/// Run one guardian pass: detect, snapshot when healthy, heal when compromised.
///
/// * `home_root` — the user's home directory (parent of `.claude.json`).
/// * `claude_dir` — the Claude config/state dir (usually `home_root/.claude`).
/// * `marketplace_repo` — local marketplace clone, when one was discovered.
///
/// Heal policy: `Missing` and `Tampered` are healed (the file is absent or
/// parseable, so a key-preserving merge is safe). `Count(0)` with MCP repos on
/// disk is healed too — an empty block after a config rebuild is the exact
/// loss mode this guardian exists for. `Unreadable` is **alert-only**: merging
/// into a file we cannot parse would clobber unknown user state.
pub fn run(
    process: &dyn ProcessPort,
    env: &dyn EnvPort,
    home_root: &Path,
    claude_dir: &Path,
    marketplace_repo: Option<&Path>,
) -> GuardianReport {
    let state = scanner::mcp_registry_state(home_root);
    let mcp_repos = scanner::count_repos_with_suffix(home_root, "-mcp-rust");
    run_with_state(
        process,
        env,
        home_root,
        claude_dir,
        marketplace_repo,
        state,
        mcp_repos,
        chrono::Local::now().date_naive(),
    )
}

/// Deterministic core of [`run`] — state, repo count, and date injected for tests.
#[allow(clippy::too_many_arguments)]
pub fn run_with_state(
    process: &dyn ProcessPort,
    env: &dyn EnvPort,
    home_root: &Path,
    claude_dir: &Path,
    marketplace_repo: Option<&Path>,
    state: McpRegistryState,
    mcp_repos: usize,
    today: chrono::NaiveDate,
) -> GuardianReport {
    let mut warnings = Vec::new();

    let empty_but_repos_exist = state == McpRegistryState::Count(0) && mcp_repos > 0;
    let tripwire = state.is_compromised() || empty_but_repos_exist;

    // --- Snapshot: registry present and at/above the sane floor ---
    let mut snapshot_written = false;
    if let McpRegistryState::Count(n) = state {
        if n >= SNAPSHOT_FLOOR {
            if let Some(servers) = read_live_registry(home_root) {
                snapshot_written = maybe_snapshot(claude_dir, &servers, today, &mut warnings);
            }
        }
    }

    // --- Heal: Missing / Tampered / empty-while-repos-exist ---
    let healable = matches!(
        state,
        McpRegistryState::Missing | McpRegistryState::Tampered
    ) || empty_but_repos_exist;

    let heal = if healable {
        heal_registry(
            process,
            env,
            home_root,
            claude_dir,
            marketplace_repo,
            &mut warnings,
        )
    } else {
        if state == McpRegistryState::Unreadable {
            warnings.push(
                "~/.claude.json is unreadable/corrupt JSON — heal skipped (a merge would \
                 clobber unknown state); restore manually from a sentinel/state/mcp-registry \
                 snapshot"
                    .to_string(),
            );
        }
        None
    };

    GuardianReport {
        state,
        mcp_repos,
        tripwire,
        snapshot_written,
        heal,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// Live registry access
// ---------------------------------------------------------------------------

/// Read the live `mcpServers` object from `<home_root>/.claude.json`.
fn read_live_registry(home_root: &Path) -> Option<Map<String, Value>> {
    let content = fs::read_to_string(home_root.join(".claude.json")).ok()?;
    let json: Value = serde_json::from_str(&content).ok()?;
    json.get("mcpServers")?.as_object().cloned()
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

/// Directory holding dated known-good registry snapshots.
pub fn snapshot_dir(claude_dir: &Path) -> PathBuf {
    claude_dir
        .join("sentinel")
        .join("state")
        .join("mcp-registry")
}

/// Write today's snapshot if it doesn't already exist (one per day max),
/// then prune to the newest [`SNAPSHOT_KEEP`]. Returns true when a new
/// snapshot file was written.
pub fn maybe_snapshot(
    claude_dir: &Path,
    servers: &Map<String, Value>,
    today: chrono::NaiveDate,
    warnings: &mut Vec<String>,
) -> bool {
    let dir = snapshot_dir(claude_dir);
    if let Err(e) = fs::create_dir_all(&dir) {
        warnings.push(format!("cannot create snapshot dir {}: {e}", dir.display()));
        return false;
    }

    let path = dir.join(format!("registry-{}.json", today.format("%Y%m%d")));
    let mut written = false;
    if !path.exists() {
        match serde_json::to_string_pretty(&Value::Object(servers.clone())) {
            Ok(json) => {
                if let Err(e) = write_atomic(&path, json.as_bytes()) {
                    warnings.push(format!("snapshot write failed: {e}"));
                } else {
                    tracing::info!(path = %path.display(), entries = servers.len(), "MCP registry snapshot written");
                    written = true;
                }
            }
            Err(e) => warnings.push(format!("snapshot serialize failed: {e}")),
        }
    }

    prune_snapshots(&dir);
    written
}

/// Keep only the newest [`SNAPSHOT_KEEP`] `registry-*.json` files.
fn prune_snapshots(dir: &Path) {
    let mut snaps = list_snapshots(dir);
    // list_snapshots returns newest-first; everything past KEEP is pruned.
    for stale in snaps.split_off(SNAPSHOT_KEEP.min(snaps.len())) {
        let _ = fs::remove_file(&stale);
    }
}

/// All `registry-*.json` snapshot files, sorted newest-first by filename
/// (dates are zero-padded YYYYMMDD, so lexical order == chronological order).
fn list_snapshots(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut snaps: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("registry-") && n.ends_with(".json"))
        })
        .collect();
    snaps.sort();
    snaps.reverse();
    snaps
}

// ---------------------------------------------------------------------------
// Heal
// ---------------------------------------------------------------------------

/// Build the desired registry and merge it into the live config file(s).
fn heal_registry(
    process: &dyn ProcessPort,
    env: &dyn EnvPort,
    home_root: &Path,
    claude_dir: &Path,
    marketplace_repo: Option<&Path>,
    warnings: &mut Vec<String>,
) -> Option<HealOutcome> {
    let mut unresolved_env = Vec::new();
    let mut retired: Vec<String> = RETIRED_BUILTIN.iter().map(ToString::to_string).collect();

    // 1. Preferred source: marketplace declaration (REGISTRY CONTRACT v1).
    let mut source = HealSource::Marketplace;
    let mut desired = marketplace_repo.and_then(|repo| {
        desired_from_marketplace(
            process,
            repo,
            &mut retired,
            &mut unresolved_env,
            warnings,
        )
    });

    // 2. Fallback: newest known-good snapshot.
    if desired.as_ref().is_none_or(Map::is_empty) {
        source = HealSource::Snapshot;
        desired = desired_from_snapshot(claude_dir, &retired, warnings);
    }

    let desired = match desired {
        Some(map) if !map.is_empty() => map,
        _ => {
            warnings.push(
                "heal skipped: no marketplace mcp[] declaration and no usable state snapshot"
                    .to_string(),
            );
            return None;
        }
    };

    // 3. Merge into the global config, and the session config when distinct.
    let mut targets = vec![home_root.join(".claude.json")];
    if let Some(config_dir) = env.var("CLAUDE_CONFIG_DIR") {
        let session_config = PathBuf::from(config_dir).join(".claude.json");
        if !targets.contains(&session_config) {
            targets.push(session_config);
        }
    }

    let mut merged_files = Vec::new();
    for target in targets {
        match merge_registry_into_file(&target, &desired, &retired) {
            Ok(()) => merged_files.push(target),
            Err(e) => warnings.push(format!("merge into {} failed: {e}", target.display())),
        }
    }

    if merged_files.is_empty() {
        return None;
    }

    tracing::warn!(
        entries = desired.len(),
        source = %source,
        files = merged_files.len(),
        "MCP guardian healed lost registration block"
    );

    Some(HealOutcome {
        source,
        entries: desired.len(),
        merged_files,
        unresolved_env,
    })
}

/// Parse the marketplace clone's `marketplace.json` into the desired
/// `mcpServers` map per REGISTRY CONTRACT v1. Extends `retired` with the
/// declaration's own `retired` array. Returns `None` when the file is absent,
/// invalid, or declares no usable entries.
fn desired_from_marketplace(
    process: &dyn ProcessPort,
    repo: &Path,
    retired: &mut Vec<String>,
    unresolved_env: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Option<Map<String, Value>> {
    let path = repo.join("marketplace.json");
    let content = fs::read_to_string(&path).ok()?;
    let data: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("{} is invalid JSON: {e}", path.display()));
            return None;
        }
    };

    if let Some(list) = data.get("retired").and_then(Value::as_array) {
        for name in list.iter().filter_map(Value::as_str) {
            if !retired.iter().any(|r| r == name) {
                retired.push(name.to_string());
            }
        }
    }

    let entries = data.get("mcp")?.as_array()?;
    let mut doppler_cache: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    let mut desired = Map::new();

    for entry in entries {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        if retired.iter().any(|r| r == name) {
            tracing::debug!(name, "skipping retired MCP registration");
            continue;
        }
        let Some(command) = entry.get("command").and_then(Value::as_str) else {
            warnings.push(format!(
                "marketplace mcp entry '{name}' has no command — skipped"
            ));
            continue;
        };

        let args = entry.get("args").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
        let transport = entry
            .get("transport")
            .and_then(Value::as_str)
            .unwrap_or("stdio");

        let mut resolved_env = Map::new();
        if let Some(env_map) = entry.get("env").and_then(Value::as_object) {
            for (var, value) in env_map {
                match value.as_str() {
                    Some(s) if s.starts_with("$doppler:") => {
                        let resolved = doppler_cache
                            .entry(s.to_string())
                            .or_insert_with(|| resolve_doppler_ref(process, s))
                            .clone();
                        if let Some(secret) = resolved {
                            resolved_env.insert(var.clone(), Value::String(secret));
                        } else {
                            // Degraded is better than absent: keep the entry,
                            // omit the variable, warn loudly.
                            unresolved_env.push(format!("{name}.{var}"));
                            warnings.push(format!(
                                "could not resolve {s} for {name}.{var} — env var omitted"
                            ));
                        }
                    }
                    _ => {
                        resolved_env.insert(var.clone(), value.clone());
                    }
                }
            }
        }

        let mut server = Map::new();
        server.insert("command".to_string(), Value::String(command.to_string()));
        server.insert("args".to_string(), args);
        server.insert("type".to_string(), Value::String(transport.to_string()));
        server.insert("env".to_string(), Value::Object(resolved_env));
        desired.insert(name.to_string(), Value::Object(server));
    }

    Some(desired)
}

/// Load the newest known-good snapshot, dropping retired names.
fn desired_from_snapshot(
    claude_dir: &Path,
    retired: &[String],
    warnings: &mut Vec<String>,
) -> Option<Map<String, Value>> {
    let dir = snapshot_dir(claude_dir);
    for snap in list_snapshots(&dir) {
        let Ok(content) = fs::read_to_string(&snap) else {
            continue;
        };
        match serde_json::from_str::<Value>(&content) {
            Ok(Value::Object(mut map)) => {
                for name in retired {
                    map.remove(name);
                }
                if map.is_empty() {
                    continue;
                }
                tracing::info!(path = %snap.display(), "healing from state snapshot");
                return Some(map);
            }
            _ => warnings.push(format!("snapshot {} is invalid — skipped", snap.display())),
        }
    }
    None
}

/// Resolve a `$doppler:<project>/<config>/<SECRET>` reference by shelling out
/// to the doppler CLI (`doppler-rs` preferred, `doppler` fallback).
fn resolve_doppler_ref(process: &dyn ProcessPort, reference: &str) -> Option<String> {
    let rest = reference.strip_prefix("$doppler:")?;
    let mut parts = rest.splitn(3, '/');
    let project = parts.next()?;
    let config = parts.next()?;
    let secret = parts.next()?;
    if project.is_empty() || config.is_empty() || secret.is_empty() {
        return None;
    }

    for bin in ["doppler-rs", "doppler"] {
        let result = process.run(
            bin,
            &["secrets", "get", secret, "--plain", "-p", project, "-c", config],
            None,
        );
        if let Ok(out) = result {
            let value = out.stdout.trim();
            if out.success && !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Merge the desired `mcpServers` entries into one config file, preserving
/// every other key. Atomic tmp+rename write guarded by an exclusive advisory
/// lock on a sidecar lock file.
///
/// Semantics: existing non-retired registrations are kept, desired entries are
/// inserted/overwritten, retired names are removed, and any
/// `_mcpServers_disabled` tamper marker is dropped (its content is superseded
/// by the declarative heal).
fn merge_registry_into_file(
    path: &Path,
    desired: &Map<String, Value>,
    retired: &[String],
) -> Result<(), String> {
    let _lock = FileLock::acquire(path);

    let mut root = if path.exists() {
        let content =
            fs::read_to_string(path).map_err(|e| format!("read failed: {e}"))?;
        match serde_json::from_str::<Value>(&content) {
            Ok(Value::Object(map)) => map,
            // Never merge over a file we cannot faithfully round-trip.
            _ => return Err("existing file is not a JSON object — refusing to merge".to_string()),
        }
    } else {
        Map::new()
    };

    root.remove("_mcpServers_disabled");

    let servers = root
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !servers.is_object() {
        *servers = Value::Object(Map::new());
    }
    if let Some(map) = servers.as_object_mut() {
        for (name, entry) in desired {
            map.insert(name.clone(), entry.clone());
        }
        for name in retired {
            map.remove(name);
        }
    }

    let json = serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|e| format!("serialize failed: {e}"))?;
    write_atomic(path, json.as_bytes()).map_err(|e| format!("write failed: {e}"))
}

/// Atomic write: tmp file in the same directory + rename over the target.
fn write_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Append the tmp suffix to the FULL filename rather than
    // `with_extension`, which would replace the final extension
    // (`.claude.json` -> `.claude.sentinel-tmp`) and could collide with an
    // unrelated `foo.sentinel-tmp` sitting next to a `foo.claude.json`.
    // Appending keeps the tmp name unique-by-construction per target.
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".sentinel-tmp");
    let tmp = PathBuf::from(tmp_name);
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(content)?;
        file.sync_all()?;
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
    // NIT (rename durability): the tmp file's contents are fsync'd above, but
    // the parent-directory entry created by the rename is NOT fsync'd, so on a
    // crash immediately after `rename` the directory metadata may not be
    // durable yet. Fixing this would require opening a directory handle and
    // flushing it — on Windows that needs `FILE_FLAG_BACKUP_SEMANTICS` and a
    // raw `FlushFileBuffers`, which is `unsafe`. This workspace forbids unsafe
    // (`unsafe_code = "forbid"`), and heals are rare, best-effort, and re-run
    // every session start, so the durability gap is accepted rather than
    // reaching for unsafe FFI. Documented deliberately.
}

/// Exclusive advisory lock on a `<file>.sentinel-lock` sidecar, released on drop.
///
/// The sidecar (rather than the target itself) is locked because the atomic
/// rename in [`write_atomic`] must replace the target — Windows cannot rename
/// over a file the same process holds open+locked.
struct FileLock {
    file: Option<fs::File>,
}

impl FileLock {
    fn acquire(target: &Path) -> Self {
        // Fail-open by design: if the sidecar lockfile can't be opened or
        // locked (permissions, read-only FS, fs2 error), we proceed WITHOUT a
        // lock rather than block or abort the heal. The lock only serializes
        // concurrent sentinel heals against each other; losing it degrades to
        // last-writer-wins between sentinel processes, which is acceptable for
        // a rare, idempotent, best-effort heal. Guarding the registry is more
        // important than guaranteeing mutual exclusion for it.
        let lock_path = lock_path_for(target);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .ok();
        if let Some(f) = &file {
            // Block until the lock is available — heals are rare and short.
            // Fully-qualified fs2 call: std::fs::File grew inherent lock
            // methods in newer toolchains, which would otherwise shadow the
            // trait method and leave the import "unused".
            let _ = fs2::FileExt::lock_exclusive(f);
        }
        Self { file }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Some(f) = self.file.take() {
            let _ = fs2::FileExt::unlock(&f);
        }
    }
}

fn lock_path_for(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map_or_else(|| "config".into(), std::ffi::OsStr::to_os_string);
    name.push(".sentinel-lock");
    target.with_file_name(name)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{StubEnv, StubProcess};
    use sentinel_domain::port_errors::ProcessError;
    use sentinel_domain::ports::ProcessOutput;
    use tempfile::TempDir;

    /// ProcessPort stub that resolves doppler refs to `resolved:<secret-name>`
    /// but only when invoked as the given binary.
    struct DopplerProcess {
        binary: &'static str,
    }

    impl ProcessPort for DopplerProcess {
        fn run(
            &self,
            command: &str,
            args: &[&str],
            _cwd: Option<&str>,
        ) -> Result<ProcessOutput, ProcessError> {
            if command == self.binary && args.first() == Some(&"secrets") {
                return Ok(ProcessOutput {
                    success: true,
                    stdout: format!("resolved:{}\n", args[2]),
                    stderr: String::new(),
                });
            }
            Ok(ProcessOutput {
                success: false,
                stdout: String::new(),
                stderr: "not found".to_string(),
            })
        }

        fn spawn_detached(&self, _: &str, _: &[&str]) -> Result<(), ProcessError> {
            Ok(())
        }
    }

    /// ProcessPort stub where every command fails (doppler CLI missing).
    struct FailingProcess;
    impl ProcessPort for FailingProcess {
        fn run(&self, _: &str, _: &[&str], _: Option<&str>) -> Result<ProcessOutput, ProcessError> {
            Err(ProcessError::backend("no such binary"))
        }
        fn spawn_detached(&self, _: &str, _: &[&str]) -> Result<(), ProcessError> {
            Ok(())
        }
    }

    fn write_marketplace(repo: &Path, json: &str) {
        std::fs::create_dir_all(repo).unwrap();
        std::fs::write(repo.join("marketplace.json"), json).unwrap();
    }

    const CONTRACT_FIXTURE: &str = r#"{
        "mcp": [
            {
                "name": "linear",
                "command": "mcp-supervisor",
                "args": ["mcp-router", "--single", "C:/x/linear-mcp.exe", "--watch", "C:/x/linear-mcp.exe"],
                "transport": "stdio",
                "env": {
                    "RUST_LOG": "info",
                    "LINEAR_API_KEY": "$doppler:firefly/dev/LINEAR_API_KEY"
                }
            },
            {
                "name": "agents",
                "command": "mcp-supervisor",
                "args": ["mcp-router", "--single", "C:/x/agents-mcp.exe"],
                "transport": "stdio",
                "env": {}
            },
            {
                "name": "doppler",
                "command": "mcp-supervisor",
                "args": [],
                "transport": "stdio",
                "env": {}
            }
        ],
        "retired": ["agents", "skills"]
    }"#;

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn today() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 7, 4).unwrap()
    }

    // -- heal: marketplace fixture -> merged .claude.json --------------------

    #[test]
    fn heal_missing_registry_from_marketplace_resolves_doppler() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude_dir = home.join(".claude");
        let repo = home.join("marketplace");
        write_marketplace(&repo, CONTRACT_FIXTURE);

        let report = run_with_state(
            &DopplerProcess { binary: "doppler-rs" },
            &StubEnv::new(),
            home,
            &claude_dir,
            Some(&repo),
            McpRegistryState::Missing,
            5,
            today(),
        );

        assert!(report.tripwire);
        let heal = report.heal.expect("heal must run for Missing");
        assert_eq!(heal.source, HealSource::Marketplace);
        assert_eq!(heal.entries, 2, "retired 'agents' must be tombstoned");
        assert!(heal.unresolved_env.is_empty());

        let json = read_json(&home.join(".claude.json"));
        let servers = json["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 2);
        assert!(servers.contains_key("linear"));
        assert!(servers.contains_key("doppler"));
        assert!(!servers.contains_key("agents"), "retired name healed back");
        assert_eq!(servers["linear"]["command"], "mcp-supervisor");
        assert_eq!(servers["linear"]["type"], "stdio");
        assert_eq!(servers["linear"]["args"][1], "--single");
        assert_eq!(servers["linear"]["env"]["RUST_LOG"], "info");
        assert_eq!(
            servers["linear"]["env"]["LINEAR_API_KEY"], "resolved:LINEAR_API_KEY",
            "$doppler ref must be resolved via the CLI"
        );
    }

    #[test]
    fn heal_falls_back_to_doppler_binary_when_doppler_rs_missing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let repo = home.join("marketplace");
        write_marketplace(&repo, CONTRACT_FIXTURE);

        let report = run_with_state(
            &DopplerProcess { binary: "doppler" },
            &StubEnv::new(),
            home,
            &home.join(".claude"),
            Some(&repo),
            McpRegistryState::Missing,
            5,
            today(),
        );

        let json = read_json(&home.join(".claude.json"));
        assert_eq!(
            json["mcpServers"]["linear"]["env"]["LINEAR_API_KEY"],
            "resolved:LINEAR_API_KEY"
        );
        assert!(report.heal.unwrap().unresolved_env.is_empty());
    }

    #[test]
    fn heal_omits_env_var_when_doppler_cli_missing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let repo = home.join("marketplace");
        write_marketplace(&repo, CONTRACT_FIXTURE);

        let report = run_with_state(
            &FailingProcess,
            &StubEnv::new(),
            home,
            &home.join(".claude"),
            Some(&repo),
            McpRegistryState::Missing,
            5,
            today(),
        );

        let heal = report.heal.expect("heal must still land, degraded");
        assert_eq!(heal.unresolved_env, vec!["linear.LINEAR_API_KEY"]);

        let json = read_json(&home.join(".claude.json"));
        let env = json["mcpServers"]["linear"]["env"].as_object().unwrap();
        assert_eq!(
            env.get("LINEAR_API_KEY"),
            None,
            "unresolved $doppler ref must be omitted, never written literally"
        );
        assert_eq!(env["RUST_LOG"], "info", "non-secret env stays literal");
        assert!(report.warnings.iter().any(|w| w.contains("LINEAR_API_KEY")));
    }

    #[test]
    fn heal_preserves_other_keys_and_existing_servers() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let repo = home.join("marketplace");
        write_marketplace(&repo, CONTRACT_FIXTURE);

        // Tampered file: _mcpServers_disabled marker + unrelated keys +
        // a pre-existing registration not in the marketplace.
        std::fs::write(
            home.join(".claude.json"),
            r#"{
                "numStartups": 42,
                "oauthAccount": {"email": "g@example.com"},
                "_mcpServers_disabled": {"linear": {}},
                "mcpServers": {"custom-local": {"command": "custom.exe"}, "agents": {"command": "old"}}
            }"#,
        )
        .unwrap();

        let report = run_with_state(
            &DopplerProcess { binary: "doppler-rs" },
            &StubEnv::new(),
            home,
            &home.join(".claude"),
            Some(&repo),
            McpRegistryState::Tampered,
            5,
            today(),
        );
        assert!(report.heal.is_some());

        let json = read_json(&home.join(".claude.json"));
        assert_eq!(json["numStartups"], 42, "unrelated keys preserved");
        assert_eq!(json["oauthAccount"]["email"], "g@example.com");
        assert!(
            json.get("_mcpServers_disabled").is_none(),
            "tamper marker must be removed on heal"
        );
        let servers = json["mcpServers"].as_object().unwrap();
        assert!(servers.contains_key("custom-local"), "existing entry kept");
        assert!(servers.contains_key("linear"));
        assert!(!servers.contains_key("agents"), "retired removed on merge");
    }

    #[test]
    fn heal_merges_into_session_config_dir_too() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let repo = home.join("marketplace");
        write_marketplace(&repo, CONTRACT_FIXTURE);
        let session_dir = home.join("session-env").join("claude7");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join(".claude.json"), r#"{"foo": 1}"#).unwrap();

        let env = StubEnv::with(&[("CLAUDE_CONFIG_DIR", session_dir.to_str().unwrap())]);
        let report = run_with_state(
            &DopplerProcess { binary: "doppler-rs" },
            &env,
            home,
            &home.join(".claude"),
            Some(&repo),
            McpRegistryState::Missing,
            5,
            today(),
        );

        let heal = report.heal.unwrap();
        assert_eq!(heal.merged_files.len(), 2, "global + session config");

        let session = read_json(&session_dir.join(".claude.json"));
        assert_eq!(session["foo"], 1);
        assert!(session["mcpServers"]["linear"].is_object());
    }

    #[test]
    fn heal_falls_back_to_snapshot_when_marketplace_invalid() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude_dir = home.join(".claude");
        let repo = home.join("marketplace");
        write_marketplace(&repo, "{ not json ");

        // Seed a known-good snapshot (which still carries a retired name).
        let dir = snapshot_dir(&claude_dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("registry-20260701.json"),
            r#"{"linear": {"command": "mcp-supervisor", "args": [], "type": "stdio", "env": {}},
                "skills": {"command": "old-skills"}}"#,
        )
        .unwrap();

        let report = run_with_state(
            &FailingProcess,
            &StubEnv::new(),
            home,
            &claude_dir,
            Some(&repo),
            McpRegistryState::Missing,
            5,
            today(),
        );

        let heal = report.heal.expect("snapshot fallback must heal");
        assert_eq!(heal.source, HealSource::Snapshot);
        assert_eq!(heal.entries, 1, "retired 'skills' dropped from snapshot");

        let json = read_json(&home.join(".claude.json"));
        assert!(json["mcpServers"]["linear"].is_object());
        assert!(json["mcpServers"].get("skills").is_none());
    }

    #[test]
    fn heal_skipped_when_no_source_available() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let report = run_with_state(
            &StubProcess,
            &StubEnv::new(),
            home,
            &home.join(".claude"),
            None,
            McpRegistryState::Missing,
            5,
            today(),
        );
        assert!(report.tripwire);
        assert!(report.heal.is_none());
        assert!(!home.join(".claude.json").exists(), "nothing written");
        assert!(report.warnings.iter().any(|w| w.contains("heal skipped")));
    }

    #[test]
    fn unreadable_registry_alerts_but_never_heals() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        std::fs::write(home.join(".claude.json"), "{ corrupt").unwrap();
        let repo = home.join("marketplace");
        write_marketplace(&repo, CONTRACT_FIXTURE);

        let report = run_with_state(
            &DopplerProcess { binary: "doppler-rs" },
            &StubEnv::new(),
            home,
            &home.join(".claude"),
            Some(&repo),
            McpRegistryState::Unreadable,
            5,
            today(),
        );

        assert!(report.tripwire);
        assert!(report.heal.is_none(), "must not merge over corrupt JSON");
        assert_eq!(std::fs::read_to_string(home.join(".claude.json")).unwrap(), "{ corrupt");
        assert!(report.warnings.iter().any(|w| w.contains("unreadable")));
    }

    #[test]
    fn empty_registry_with_repos_trips_and_heals() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        std::fs::write(home.join(".claude.json"), r#"{"mcpServers": {}, "keep": true}"#).unwrap();
        let repo = home.join("marketplace");
        write_marketplace(&repo, CONTRACT_FIXTURE);

        let report = run_with_state(
            &DopplerProcess { binary: "doppler-rs" },
            &StubEnv::new(),
            home,
            &home.join(".claude"),
            Some(&repo),
            McpRegistryState::Count(0),
            42,
            today(),
        );
        assert!(report.tripwire);
        assert!(report.heal.is_some());
        let json = read_json(&home.join(".claude.json"));
        assert_eq!(json["keep"], true);
        assert!(json["mcpServers"]["linear"].is_object());
    }

    #[test]
    fn empty_registry_without_repos_is_calm() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        std::fs::write(home.join(".claude.json"), r#"{"mcpServers": {}}"#).unwrap();

        let report = run_with_state(
            &StubProcess,
            &StubEnv::new(),
            home,
            &home.join(".claude"),
            None,
            McpRegistryState::Count(0),
            0,
            today(),
        );
        assert!(!report.tripwire);
        assert!(report.heal.is_none());
    }

    // -- snapshots ------------------------------------------------------------

    fn healthy_registry_json(n: usize) -> String {
        let mut servers = Map::new();
        for i in 0..n {
            servers.insert(
                format!("srv{i}"),
                serde_json::json!({"command": "mcp-supervisor", "args": [], "type": "stdio", "env": {}}),
            );
        }
        serde_json::json!({ "mcpServers": servers }).to_string()
    }

    #[test]
    fn healthy_registry_writes_one_snapshot_per_day() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude_dir = home.join(".claude");
        std::fs::write(home.join(".claude.json"), healthy_registry_json(12)).unwrap();

        let first = run_with_state(
            &StubProcess,
            &StubEnv::new(),
            home,
            &claude_dir,
            None,
            McpRegistryState::Count(12),
            42,
            today(),
        );
        assert!(!first.tripwire);
        assert!(first.snapshot_written);
        let snap = snapshot_dir(&claude_dir).join("registry-20260704.json");
        assert!(snap.exists());
        let content: Value = read_json(&snap);
        assert_eq!(content.as_object().unwrap().len(), 12);

        // Same day again: no second write.
        let second = run_with_state(
            &StubProcess,
            &StubEnv::new(),
            home,
            &claude_dir,
            None,
            McpRegistryState::Count(12),
            42,
            today(),
        );
        assert!(!second.snapshot_written);
    }

    #[test]
    fn registry_below_floor_is_not_snapshotted() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude_dir = home.join(".claude");
        std::fs::write(home.join(".claude.json"), healthy_registry_json(3)).unwrap();

        let report = run_with_state(
            &StubProcess,
            &StubEnv::new(),
            home,
            &claude_dir,
            None,
            McpRegistryState::Count(3),
            42,
            today(),
        );
        assert!(!report.snapshot_written);
        assert!(!snapshot_dir(&claude_dir).exists());
    }

    #[test]
    fn snapshot_rotation_keeps_newest_fourteen() {
        let tmp = TempDir::new().unwrap();
        let claude_dir = tmp.path().join(".claude");
        let dir = snapshot_dir(&claude_dir);
        std::fs::create_dir_all(&dir).unwrap();
        for day in 1..=20 {
            std::fs::write(dir.join(format!("registry-202606{day:02}.json")), "{}").unwrap();
        }

        let mut warnings = Vec::new();
        let servers = Map::new();
        let written = maybe_snapshot(
            &claude_dir,
            &servers,
            chrono::NaiveDate::from_ymd_opt(2026, 6, 21).unwrap(),
            &mut warnings,
        );
        assert!(written);

        let remaining = list_snapshots(&dir);
        assert_eq!(remaining.len(), SNAPSHOT_KEEP);
        // Newest survives, oldest pruned.
        assert!(dir.join("registry-20260621.json").exists());
        assert!(dir.join("registry-20260620.json").exists());
        assert!(!dir.join("registry-20260607.json").exists());
        assert!(!dir.join("registry-20260601.json").exists());
    }

    // -- doppler ref parsing ----------------------------------------------------

    #[test]
    fn malformed_doppler_ref_is_unresolved() {
        assert_eq!(
            resolve_doppler_ref(&DopplerProcess { binary: "doppler-rs" }, "$doppler:only-two/parts"),
            None
        );
        assert_eq!(
            resolve_doppler_ref(&DopplerProcess { binary: "doppler-rs" }, "not-a-ref"),
            None
        );
        assert_eq!(
            resolve_doppler_ref(&DopplerProcess { binary: "doppler-rs" }, "$doppler://x"),
            None
        );
    }

    #[test]
    fn doppler_ref_passes_project_config_secret() {
        struct AssertingProcess;
        impl ProcessPort for AssertingProcess {
            fn run(
                &self,
                command: &str,
                args: &[&str],
                _cwd: Option<&str>,
            ) -> Result<ProcessOutput, ProcessError> {
                assert_eq!(command, "doppler-rs");
                assert_eq!(
                    args,
                    ["secrets", "get", "MY_KEY", "--plain", "-p", "proj", "-c", "dev"]
                );
                Ok(ProcessOutput {
                    success: true,
                    stdout: "s3cret\n".to_string(),
                    stderr: String::new(),
                })
            }
            fn spawn_detached(&self, _: &str, _: &[&str]) -> Result<(), ProcessError> {
                Ok(())
            }
        }
        assert_eq!(
            resolve_doppler_ref(&AssertingProcess, "$doppler:proj/dev/MY_KEY"),
            Some("s3cret".to_string())
        );
    }
}
