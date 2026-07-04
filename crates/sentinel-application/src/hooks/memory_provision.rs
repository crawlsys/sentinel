//! Memory Provisioning — verify + (idempotently) register the memory subsystem
//! at `SessionStart`.
//!
//! ## Why this hook exists
//! Sentinel's memory hooks (`memory_inject`, `memory_discipline`,
//! `memory_turn_capture`, …) all silently no-op when the `memory` CLI /
//! `memory-mcp` are absent or unregistered. Enforcement was therefore
//! structurally gated behind provisioning that *did not exist* — nothing ever
//! wrote `~/.claude.json` or checked that the engine was installed. This hook
//! fills that gap: at `SessionStart` it
//!
//!   1. **verifies** the `memory` + `memory-mcp` binaries are present
//!      (warn-with-install-guidance if not — it NEVER installs/compiles), and
//!   2. **provisions the Qdrant credential** — verifies `~/.qdrant/config.json`
//!      and, when it is missing / invalid / expired (JWT `exp` decoded), fetches
//!      a fresh `cluster_url` + `api_key` from Doppler (network call bounded by
//!      `doppler_timeout_secs`), re-validates the FETCHED pair the same way,
//!      and writes it (atomic + `.bak`, owner-only 0600 on unix). With
//!      `require_remote = true` (default) a credential that cannot be
//!      provisioned WITHHOLDS registration — and REMOVES a previously
//!      registered `memory` entry so the guarantee holds on re-runs — and
//!      surfaces a console error: memory does not go active without a working
//!      remote store, and
//!   3. **registers** the `memory` MCP server into `~/.claude.json`
//!      idempotently and safely (atomic write + `.bak` backup, all existing
//!      keys/servers preserved), only when the engine binaries actually exist
//!      (the launcher command is PATH-resolved), and
//!   4. writes a **readiness marker** at
//!      `~/.claude/sentinel/state/memory-provision.json` that
//!      a future `memory_discipline` enforcement gate consults to decide whether its
//!      `Enforce` tier is allowed to bite (warn-first: enforcement is INERT
//!      until provisioning has actually succeeded).
//!
//! ## Org / remote mirror
//! When a remote mirror is configured — `MEMORY_REMOTE_URL` (env, takes
//! precedence) or `org_mirror_url` in the config TOML — the registered MCP
//! entry's `env` map carries `MEMORY_REMOTE_URL=<url>` + `MEMORY_MIRROR_ORG=1`,
//! and the launcher command is the mirror launcher. Absent → Qdrant-only,
//! mirror off.
//!
//! ## Safety
//! The hook NEVER denies and NEVER runs `cargo install` / downloads / compiles.
//! The `~/.claude.json` mutation is split into a PURE, IO-free merge
//! ([`merge_memory_server`]) and a thin IO wrapper that: refuses to clobber a
//! missing/invalid file, backs the original up to `~/.claude.json.bak`, and
//! writes atomically (temp file + rename) with pretty 2-space JSON.

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use sentinel_domain::events::{HookEnvelope, HookEvent, HookInput, HookOutput};

use super::{EnvPort, FileSystemPort, HookContext, ProcessPort};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// User-facing hook name shown in the injected envelope.
const HOOK_LABEL: &str = "Memory Provisioning";

/// The MCP server key written under `mcpServers` in `~/.claude.json`.
const MEMORY_SERVER_KEY: &str = "memory";

/// Launcher command for the Qdrant-only memory MCP server (PATH-resolved;
/// `~/.cargo/bin` + `~/.local/bin` are conventionally on PATH).
const MEMORY_COMMAND: &str = "memory-mcp";

/// Launcher command used when an org/remote mirror is configured. The mirror
/// launcher wraps `memory-mcp` and dual-writes to Qdrant + the remote store.
const MEMORY_MIRROR_COMMAND: &str = "memory-mcp-mirror";

/// Exact, copy-pasteable install guidance surfaced when a binary is missing.
/// The hook NEVER runs this itself — provisioning verifies + advises only.
const INSTALL_HINT: &str = "install the memory engine, then restart the session: \
    `cargo install --git https://github.com/legatus-ai/memory memory memory-mcp` \
    (or from a local checkout: `cd ~/Documents/GitHub/memory && cargo install --path . --bins`). \
    Until both `memory` and `memory-mcp` are on PATH, all memory hooks no-op and \
    memory-discipline enforcement stays inert.";

// ---------------------------------------------------------------------------
// Config (shipped defaults + operator override) — IO-light
// ---------------------------------------------------------------------------

/// Shipped baseline, embedded at compile time. Operators override at
/// `~/.claude/sentinel/config/memory-provision.toml`.
const SHIPPED_DEFAULTS: &str = include_str!("../../../../config/memory-provision-defaults.toml");

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryProvisionConfig {
    /// Master switch. `false` → the hook is a no-op (allow).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Register the `memory` MCP server into `~/.claude.json` when missing.
    #[serde(default = "default_true")]
    pub register_mcp: bool,
    /// Optional org/remote mirror endpoint. `MEMORY_REMOTE_URL` (env) wins.
    #[serde(default)]
    pub org_mirror_url: String,
    /// Memory does not activate without a working Qdrant credential. When
    /// `true` (default), a missing/expired credential that cannot be fetched
    /// from Doppler withholds MCP registration and surfaces a console error.
    #[serde(default = "default_true")]
    pub require_remote: bool,
    /// Doppler project holding the Qdrant secrets.
    #[serde(default = "default_doppler_project")]
    pub doppler_project: String,
    /// Doppler config (environment) within that project.
    #[serde(default = "default_doppler_config")]
    pub doppler_config: String,
    /// Doppler secret name for the Qdrant cluster URL.
    #[serde(default = "default_url_secret")]
    pub qdrant_url_secret: String,
    /// Doppler secret name for the Qdrant API key.
    #[serde(default = "default_key_secret")]
    pub qdrant_key_secret: String,
    /// Wall-clock deadline (seconds) for the `doppler secrets get` network
    /// call. On expiry the child is killed and the fetch is treated as a
    /// failure (withhold path), so an unreachable Doppler API cannot hang
    /// `SessionStart` indefinitely.
    #[serde(default = "default_doppler_timeout_secs")]
    pub doppler_timeout_secs: u64,
}

const fn default_true() -> bool {
    true
}
const fn default_doppler_timeout_secs() -> u64 {
    10
}
fn default_doppler_project() -> String {
    "legatus".to_string()
}
fn default_doppler_config() -> String {
    "dev".to_string()
}
fn default_url_secret() -> String {
    "QDRANT_URL".to_string()
}
fn default_key_secret() -> String {
    "QDRANT_API_KEY".to_string()
}

impl Default for MemoryProvisionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            register_mcp: true,
            org_mirror_url: String::new(),
            require_remote: true,
            doppler_project: default_doppler_project(),
            doppler_config: default_doppler_config(),
            qdrant_url_secret: default_url_secret(),
            qdrant_key_secret: default_key_secret(),
            doppler_timeout_secs: default_doppler_timeout_secs(),
        }
    }
}

impl MemoryProvisionConfig {
    fn from_toml_or_default(s: &str) -> Self {
        toml::from_str(s).unwrap_or_else(|e| {
            warn!(error = %e, "memory-provision TOML parse failed; using defaults");
            Self::default()
        })
    }
}

/// Load shipped defaults, then (if present) replace wholesale with the operator
/// override file — mirrors the `spec_challenge` override behavior.
fn load_config(ctx: &HookContext<'_>) -> MemoryProvisionConfig {
    let mut cfg = MemoryProvisionConfig::from_toml_or_default(SHIPPED_DEFAULTS);
    if let Some(home) = ctx.fs.home_dir() {
        let path = home
            .join(".claude")
            .join("sentinel")
            .join("config")
            .join("memory-provision.toml");
        if let Ok(content) = ctx.fs.read_to_string(&path) {
            cfg = MemoryProvisionConfig::from_toml_or_default(&content);
            info!(path = %path.display(), "loaded memory-provision operator override");
        }
    }
    cfg
}

/// Resolve the org/remote mirror URL: `MEMORY_REMOTE_URL` env (non-empty) wins
/// over the TOML `org_mirror_url` (non-empty); otherwise `None` (mirror off).
fn resolve_remote_url(ctx: &HookContext<'_>, cfg: &MemoryProvisionConfig) -> Option<String> {
    if let Some(env_url) = ctx.env.var("MEMORY_REMOTE_URL") {
        let t = env_url.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    let t = cfg.org_mirror_url.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

// ---------------------------------------------------------------------------
// Binary verification — IO (filesystem existence checks)
// ---------------------------------------------------------------------------

/// True when `path` is an actually-runnable binary: a regular file with an
/// execute bit on unix (a plain data file at the right path must not count as
/// "installed"); a regular file elsewhere (no POSIX exec bits).
fn is_executable_file(fs: &dyn FileSystemPort, path: &Path) -> bool {
    let Ok(meta) = fs.metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// True when an executable named `name` is found in one of the conventional
/// install locations (`~/.cargo/bin`, `~/.local/bin`), the dev release build,
/// or any directory on `$PATH` — the registered launcher command is
/// PATH-resolved by Claude Code, so PATH is authoritative for "installed".
/// Mirrors `memory_turn_capture::memory_bin`'s probe order, generalized.
fn find_binary(fs: &dyn FileSystemPort, env: &dyn EnvPort, name: &str) -> bool {
    let Some(home) = fs.home_dir() else {
        return false;
    };
    let candidates = [
        home.join(".cargo").join("bin").join(name),
        home.join(".cargo").join("bin").join(format!("{name}.exe")),
        home.join(".local").join("bin").join(name),
        home.join(".local").join("bin").join(format!("{name}.exe")),
        home.join("Documents")
            .join("GitHub")
            .join("memory")
            .join("target")
            .join("release")
            .join(name),
        home.join("Documents")
            .join("GitHub")
            .join("memory")
            .join("target")
            .join("release")
            .join(format!("{name}.exe")),
    ];
    if candidates.iter().any(|p| is_executable_file(fs, p)) {
        return true;
    }
    // Probe $PATH — the registered `command` is PATH-resolved, so a binary
    // installed anywhere on PATH counts even outside the conventional dirs.
    if let Some(path_os) = env.var_os("PATH") {
        for dir in std::env::split_paths(&path_os) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            if is_executable_file(fs, &dir.join(name))
                || is_executable_file(fs, &dir.join(format!("{name}.exe")))
            {
                return true;
            }
        }
    }
    false
}

/// Both required engine binaries present: the `memory` CLI plus `launcher` —
/// the EXACT command that will be registered (`memory-mcp`, or
/// `memory-mcp-mirror` when an org/remote mirror is configured). Gating on
/// `memory-mcp` alone would happily register a mirror launcher that isn't
/// installed.
fn binaries_present(fs: &dyn FileSystemPort, env: &dyn EnvPort, launcher: &str) -> bool {
    find_binary(fs, env, "memory") && find_binary(fs, env, launcher)
}

// ---------------------------------------------------------------------------
// ~/.claude.json registration — PURE merge + thin IO wrapper
// ---------------------------------------------------------------------------

/// Result of the pure [`merge_memory_server`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The `memory` server was inserted (the document changed).
    Merged,
    /// A `memory` server already existed (or the document had an unexpected
    /// `mcpServers` shape) — the document is left untouched.
    AlreadyPresent,
}

/// PURE, IO-free merge of the `memory` MCP server into a parsed `~/.claude.json`
/// document `root`.
///
/// * Preserves every existing top-level key and every existing MCP server.
/// * Idempotent: if a `memory` server already exists, returns
///   [`Outcome::AlreadyPresent`] and does NOT mutate.
/// * When `remote_url` is `Some`, the entry's launcher is the mirror command
///   and its `env` carries `MEMORY_REMOTE_URL` + `MEMORY_MIRROR_ORG=1`; when
///   `None`, the entry is Qdrant-only (no `env`).
#[must_use]
pub fn merge_memory_server(root: &mut serde_json::Value, remote_url: Option<&str>) -> Outcome {
    use serde_json::{json, Map, Value};

    if !root.is_object() {
        // Defensive — the IO wrapper guarantees an object; if a caller hands us
        // something else, replace with an empty object so the merge is total.
        *root = Value::Object(Map::new());
    }
    let obj = root.as_object_mut().expect("root is object");

    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else {
        // `mcpServers` exists but isn't an object — unexpected shape; don't
        // clobber the operator's file.
        return Outcome::AlreadyPresent;
    };

    if servers.contains_key(MEMORY_SERVER_KEY) {
        return Outcome::AlreadyPresent;
    }

    let mut entry = Map::new();
    entry.insert("type".into(), json!("stdio"));
    let command = if remote_url.is_some() {
        MEMORY_MIRROR_COMMAND
    } else {
        MEMORY_COMMAND
    };
    entry.insert("command".into(), json!(command));
    entry.insert("args".into(), json!([]));
    if let Some(url) = remote_url {
        let mut env = Map::new();
        env.insert("MEMORY_REMOTE_URL".into(), json!(url));
        env.insert("MEMORY_MIRROR_ORG".into(), json!("1"));
        entry.insert("env".into(), Value::Object(env));
    }
    servers.insert(MEMORY_SERVER_KEY.into(), Value::Object(entry));
    Outcome::Merged
}

/// Atomically write `bytes` to `path` (temp sibling + rename).
///
/// * The temp name is unique per process (`<path>.tmp.<pid>`) so two
///   concurrent sentinel processes (e.g. two sessions starting at once)
///   cannot interleave partial writes through the same temp file. There is
///   deliberately NO cross-process lock (the workspace has no shared flock
///   utility): concurrent writers are last-writer-wins on the final rename,
///   which is acceptable here because every writer renames in a complete,
///   valid document — the file can go stale by one merge, never torn.
/// * `private` routes the temp write through [`FileSystemPort::write_private`]
///   (0600 on unix); `rename` preserves the temp file's mode, so the target
///   ends up owner-only with no world-readable window.
/// * A failed rename is propagated (after best-effort temp cleanup) rather
///   than degraded to a non-atomic direct write — callers must fail loudly,
///   not risk a torn `~/.claude.json` / credential file.
fn atomic_write(
    fs: &dyn FileSystemPort,
    path: &Path,
    bytes: &[u8],
    private: bool,
) -> anyhow::Result<()> {
    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(format!(".tmp.{}", std::process::id()));
    let tmp = PathBuf::from(tmp_os);
    if private {
        fs.write_private(&tmp, bytes)?;
    } else {
        fs.write(&tmp, bytes)?;
        // Preserve the destination's existing mode across the rename: the
        // temp is created with default umask perms (0644), and renaming it
        // over e.g. a user-tightened 0600 ~/.claude.json (which can carry
        // other MCP servers' env secrets) would silently loosen it.
        // Private writes skip this — 0600 must not be widened back.
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Don't leave the temp file behind, and don't silently degrade to a
        // non-atomic write — surface the failure to the caller.
        let _ = fs.remove_file(&tmp);
        anyhow::bail!(
            "atomic rename {} -> {} failed: {e}",
            tmp.display(),
            path.display()
        );
    }
    Ok(())
}

/// Thin IO wrapper around [`merge_memory_server`]. Reads `~/.claude.json`,
/// refuses to clobber a missing/invalid file, backs the original up to
/// `~/.claude.json.bak`, and writes the merged document atomically.
fn register_memory_mcp(
    fs: &dyn FileSystemPort,
    remote_url: Option<&str>,
) -> anyhow::Result<Outcome> {
    let home = fs
        .home_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let path = home.join(".claude.json");

    let content = fs.read_to_string(&path).map_err(|e| {
        anyhow::anyhow!("~/.claude.json unreadable ({e}); refusing to register (no clobber)")
    })?;
    let mut root: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
        anyhow::anyhow!("~/.claude.json is invalid JSON ({e}); refusing to register (no clobber)")
    })?;
    if !root.is_object() {
        anyhow::bail!("~/.claude.json is not a JSON object; refusing to register (no clobber)");
    }

    match merge_memory_server(&mut root, remote_url) {
        Outcome::AlreadyPresent => Ok(Outcome::AlreadyPresent),
        Outcome::Merged => {
            // Back up the prior file contents before any write to the live
            // file. Written 0600 — ~/.claude.json can carry MCP `env` secrets.
            let bak = home.join(".claude.json.bak");
            if let Err(e) = fs.write_private(&bak, content.as_bytes()) {
                warn!(error = %e, "failed to write ~/.claude.json.bak; aborting registration");
                anyhow::bail!("backup write failed: {e}");
            }
            let pretty = serde_json::to_string_pretty(&root)?; // 2-space indent
            atomic_write(fs, &path, pretty.as_bytes(), false)?;
            Ok(Outcome::Merged)
        }
    }
}

/// Result of the pure [`remove_memory_server`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// A sentinel-managed `memory` entry was removed (the document changed).
    Removed,
    /// No `memory` entry exists (or the document has an unexpected shape) —
    /// nothing to do.
    NotPresent,
    /// A `memory` entry exists but is NOT recognizably one this hook wrote
    /// (custom command/args/env) — left untouched, user-owned.
    ForeignEntry,
}

/// True when an `mcpServers.memory` entry is recognizably one THIS hook wrote
/// — i.e. exactly the shape [`merge_memory_server`] produces: `stdio` type,
/// one of the two known launcher commands, empty/absent `args`, `env` absent
/// (Qdrant-only) or exactly the mirror pair, and no extra keys. PURE.
///
/// Anything else is treated as user-owned: the registration path refuses to
/// clobber an existing entry, so the removal path must be equally
/// conservative — a key-only match would let the withhold branch delete an
/// operator's custom memory server.
#[must_use]
pub fn is_sentinel_managed_entry(entry: &serde_json::Value) -> bool {
    let Some(obj) = entry.as_object() else {
        return false;
    };
    if obj.get("type").and_then(|v| v.as_str()) != Some("stdio") {
        return false;
    }
    let command = obj.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if command != MEMORY_COMMAND && command != MEMORY_MIRROR_COMMAND {
        return false;
    }
    // args: absent or exactly [].
    if let Some(args) = obj.get("args") {
        if args.as_array().is_none_or(|a| !a.is_empty()) {
            return false;
        }
    }
    // env: absent (Qdrant-only) or exactly the mirror pair this hook writes.
    if let Some(env) = obj.get("env") {
        let Some(env) = env.as_object() else {
            return false;
        };
        let mut keys: Vec<&str> = env.keys().map(String::as_str).collect();
        keys.sort_unstable();
        if keys != ["MEMORY_MIRROR_ORG", "MEMORY_REMOTE_URL"] {
            return false;
        }
    }
    // Any key beyond what merge_memory_server writes → user-modified.
    obj.keys()
        .all(|k| matches!(k.as_str(), "type" | "command" | "args" | "env"))
}

/// PURE, IO-free removal of the `memory` MCP server from a parsed
/// `~/.claude.json` document. Only removes an entry that passes
/// [`is_sentinel_managed_entry`]; a user-owned/custom entry is left untouched
/// ([`RemoveOutcome::ForeignEntry`]). Unexpected shapes (non-object root /
/// `mcpServers`) are left untouched — same no-clobber posture as
/// [`merge_memory_server`].
#[must_use]
pub fn remove_memory_server(root: &mut serde_json::Value) -> RemoveOutcome {
    let Some(obj) = root.as_object_mut() else {
        return RemoveOutcome::NotPresent;
    };
    let Some(servers) = obj.get_mut("mcpServers").and_then(|v| v.as_object_mut()) else {
        return RemoveOutcome::NotPresent;
    };
    let Some(entry) = servers.get(MEMORY_SERVER_KEY) else {
        return RemoveOutcome::NotPresent;
    };
    if !is_sentinel_managed_entry(entry) {
        return RemoveOutcome::ForeignEntry;
    }
    servers.remove(MEMORY_SERVER_KEY);
    RemoveOutcome::Removed
}

/// Thin IO wrapper around [`remove_memory_server`] — the withhold-path
/// counterpart of [`register_memory_mcp`], so `require_remote` holds on
/// re-runs too (a previously registered entry is de-registered when the
/// credential stops working). Same safety: refuses to clobber a malformed
/// file, backs up to `~/.claude.json.bak` (0600), writes atomically, and
/// never touches an entry it didn't write.
fn deregister_memory_mcp(fs: &dyn FileSystemPort) -> anyhow::Result<RemoveOutcome> {
    let home = fs
        .home_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let path = home.join(".claude.json");

    // Missing/unreadable file → nothing registered, nothing to remove.
    let Ok(content) = fs.read_to_string(&path) else {
        return Ok(RemoveOutcome::NotPresent);
    };
    let mut root: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
        anyhow::anyhow!("~/.claude.json is invalid JSON ({e}); refusing to deregister (no clobber)")
    })?;
    if !root.is_object() {
        anyhow::bail!("~/.claude.json is not a JSON object; refusing to deregister (no clobber)");
    }
    match remove_memory_server(&mut root) {
        RemoveOutcome::NotPresent => Ok(RemoveOutcome::NotPresent),
        RemoveOutcome::ForeignEntry => Ok(RemoveOutcome::ForeignEntry),
        RemoveOutcome::Removed => {
            let bak = home.join(".claude.json.bak");
            if let Err(e) = fs.write_private(&bak, content.as_bytes()) {
                warn!(error = %e, "failed to write ~/.claude.json.bak; aborting deregistration");
                anyhow::bail!("backup write failed: {e}");
            }
            let pretty = serde_json::to_string_pretty(&root)?; // 2-space indent
            atomic_write(fs, &path, pretty.as_bytes(), false)?;
            Ok(RemoveOutcome::Removed)
        }
    }
}

// ---------------------------------------------------------------------------
// Readiness marker — IO (shared with memory_discipline)
// ---------------------------------------------------------------------------

/// Provisioning readiness marker, written each `SessionStart` and read by
/// `memory_discipline` to gate its `Enforce` tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // flat serde marker: 4 independent flags
pub struct ProvisionMarker {
    pub binaries_present: bool,
    pub mcp_registered: bool,
    pub remote_configured: bool,
    /// A valid, non-expired Qdrant credential is present on disk.
    #[serde(default)]
    pub credential_ok: bool,
    pub ts: String,
}

fn state_dir(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let dir = fs
        .home_dir()?
        .join(".claude")
        .join("sentinel")
        .join("state");
    let _ = fs.create_dir_all(&dir);
    Some(dir)
}

fn marker_path(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    Some(state_dir(fs)?.join("memory-provision.json"))
}

/// Record the provisioning result. Best-effort; never errors a hook.
pub fn record_provisioned(
    fs: &dyn FileSystemPort,
    binaries_present: bool,
    mcp_registered: bool,
    remote_configured: bool,
    credential_ok: bool,
) {
    let Some(path) = marker_path(fs) else {
        return;
    };
    let marker = ProvisionMarker {
        binaries_present,
        mcp_registered,
        remote_configured,
        credential_ok,
        ts: Utc::now().to_rfc3339(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&marker) {
        let _ = fs.write(&path, json.as_bytes());
    }
}

/// Read the readiness marker. Provisioning counts as "ready" iff BOTH the
/// binaries are present AND the MCP server is registered. Absent / unparseable
/// → `false`.
#[must_use]
pub fn is_provisioned(fs: &dyn FileSystemPort) -> bool {
    let Some(path) = marker_path(fs) else {
        return false;
    };
    let Ok(content) = fs.read_to_string(&path) else {
        return false;
    };
    match serde_json::from_str::<ProvisionMarker>(&content) {
        Ok(m) => m.binaries_present && m.mcp_registered && m.credential_ok,
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Hook entry point
// ---------------------------------------------------------------------------

/// `SessionStart` entry point. Verifies binaries, (idempotently) registers the
/// MCP server, writes the readiness marker, and surfaces warn-level guidance.
/// NEVER denies; NEVER installs.
#[must_use]
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let _ = input;
    let cfg = load_config(ctx);
    if !cfg.enabled {
        debug!("memory_provision: disabled by config — no-op");
        return HookOutput::allow();
    }

    let remote_url = resolve_remote_url(ctx, &cfg);
    // Gate on the exact launcher that would be registered — the mirror
    // launcher when a remote mirror is configured, `memory-mcp` otherwise.
    let launcher = if remote_url.is_some() {
        MEMORY_MIRROR_COMMAND
    } else {
        MEMORY_COMMAND
    };
    let binaries = binaries_present(ctx.fs, ctx.env, launcher);

    let mut warnings: Vec<String> = Vec::new();
    if !binaries {
        warnings.push(INSTALL_HINT.to_string());
    }

    // Remote (Qdrant) credential: verify ~/.qdrant/config.json and, when it is
    // missing / invalid / expired, fetch a fresh one from Doppler. Memory is
    // Qdrant-backed — without a working credential the engine cannot run.
    let cred = ensure_credential(ctx, &cfg, Utc::now().timestamp());
    match (cred.ok, &cred.message) {
        (true, Some(m)) => info!("memory_provision: {m}"),
        (false, Some(m)) => warnings.push(m.clone()),
        _ => {}
    }
    // `require_remote` (default true): do not activate memory without a working
    // remote credential — withhold MCP registration and surface the error.
    let withhold_for_remote = cfg.require_remote && !cred.ok;

    // MCP registration (idempotent, safe). Failure → warn, never fatal.
    let mut mcp_registered = false;
    if cfg.register_mcp && withhold_for_remote {
        warn!("memory_provision: MCP registration withheld — Qdrant remote credential unavailable (require_remote=true)");
        // The require_remote guarantee must hold on re-runs too: a `memory`
        // entry registered by an earlier (credentialed) session is REMOVED,
        // not just left in place — otherwise the withhold only ever bites on
        // first registration.
        let removed_note = match deregister_memory_mcp(ctx.fs) {
            Ok(RemoveOutcome::Removed) => {
                warn!("memory_provision: removed the existing memory MCP registration from ~/.claude.json (require_remote withhold)");
                " The existing memory MCP registration in ~/.claude.json was REMOVED (backup at ~/.claude.json.bak) until the credential is restored."
            }
            Ok(RemoveOutcome::NotPresent) => "",
            Ok(RemoveOutcome::ForeignEntry) => {
                warn!("memory_provision: existing memory MCP entry is not sentinel-managed — left untouched");
                " An existing `memory` MCP entry in ~/.claude.json was NOT removed: it does not match \
                 the shape this hook writes (custom command/args/env — user-owned). Remove it manually \
                 if memory must stay inactive."
            }
            Err(e) => {
                warn!(error = %e, "memory_provision: could not remove existing memory MCP registration");
                warnings.push(format!(
                    "an existing memory MCP registration could not be removed — {e}"
                ));
                ""
            }
        };
        warnings.push(format!(
            "Memory MCP was NOT registered: its remote store (Qdrant) has no working credential and \
             require_remote=true, so memory will not activate until the credential is provisioned.{removed_note}"
        ));
    } else if cfg.register_mcp && !binaries {
        // Registering a PATH-resolved command that doesn't exist would give
        // Claude Code a permanently failing MCP server — skip until installed.
        warn!(
            launcher,
            "memory_provision: MCP registration skipped — memory engine binaries not found"
        );
        warnings.push(format!(
            "memory MCP registration skipped: the engine binaries are not installed \
             (`memory` + `{launcher}` required), so the registered command would fail to launch. \
             See the install guidance above."
        ));
    } else if cfg.register_mcp {
        match register_memory_mcp(ctx.fs, remote_url.as_deref()) {
            Ok(Outcome::Merged) => {
                mcp_registered = true;
                info!(
                    remote = remote_url.is_some(),
                    "memory_provision: registered memory MCP server in ~/.claude.json"
                );
            }
            Ok(Outcome::AlreadyPresent) => {
                mcp_registered = true;
                debug!("memory_provision: memory MCP server already registered");
            }
            Err(e) => {
                warn!(error = %e, "memory_provision: MCP registration skipped");
                warnings.push(format!("memory MCP server not registered — {e}"));
            }
        }
    } else {
        debug!("memory_provision: register_mcp disabled — skipping ~/.claude.json");
    }

    // Readiness marker drives memory_discipline's warn-first enforcement gating.
    record_provisioned(
        ctx.fs,
        binaries,
        mcp_registered,
        remote_url.is_some(),
        cred.ok,
    );

    if warnings.is_empty() {
        return HookOutput::allow();
    }
    let body = warnings.join(" ");
    HookOutput::inject_context(
        HookEvent::SessionStart,
        HookEnvelope::warn(HOOK_LABEL, body).render(),
    )
}

// ---------------------------------------------------------------------------
// Qdrant credential provisioning — fetch from Doppler when missing/expired
// ---------------------------------------------------------------------------

/// Refetch this many seconds *before* a JWT `exp` rather than let a session
/// start on a token about to die.
const JWT_EXP_SKEW_SECS: i64 = 300;

/// Path to the engine's Qdrant credential file (`~/.qdrant/config.json`).
fn qdrant_config_path(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    Some(fs.home_dir()?.join(".qdrant").join("config.json"))
}

/// State of the on-disk Qdrant credential. PURE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStatus {
    /// Present, well-formed, and (if a JWT) not near expiry.
    Valid,
    /// File missing/malformed, or missing/empty `cluster_url` / `api_key`.
    MissingOrInvalid,
    /// A JWT `api_key` whose `exp` is at/near/past `now`.
    Expired,
}

/// Assess an already-parsed `~/.qdrant/config.json` value. PURE + testable.
#[must_use]
pub fn assess_credential(existing: Option<&serde_json::Value>, now_unix: i64) -> CredentialStatus {
    let Some(obj) = existing.and_then(|v| v.as_object()) else {
        return CredentialStatus::MissingOrInvalid;
    };
    let url = obj
        .get("cluster_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let key = obj
        .get("api_key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if url.is_empty() || key.is_empty() {
        return CredentialStatus::MissingOrInvalid;
    }
    // A JWT key carries its own expiry; a non-JWT key can't be expiry-checked.
    if let Some(exp) = jwt_exp(key) {
        if exp <= now_unix + JWT_EXP_SKEW_SECS {
            return CredentialStatus::Expired;
        }
    }
    CredentialStatus::Valid
}

/// Extract the `exp` (unix seconds) claim from a 3-part JWT. PURE.
#[must_use]
pub fn jwt_exp(token: &str) -> Option<i64> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _sig = parts.next()?;
    if parts.next().is_some() {
        return None; // not a 3-part JWT
    }
    let bytes = b64url_decode(payload)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp").and_then(serde_json::Value::as_i64)
}

/// Minimal base64url decoder for the JWT payload segment. PURE.
/// Trailing `=` padding is accepted and stripped (some issuers pad the
/// payload segment; rejecting it made padded tokens un-assessable — and
/// therefore treated as never-expiring). `=` anywhere else still fails.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    for &c in s.as_bytes() {
        let v = val(c)?;
        acc = (acc << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Some(out)
}

/// Pull a secret's value out of `doppler secrets get --json` output. PURE.
/// Shape: `{ "NAME": { "computed": "...", "raw": "..." } }`; falls back to a
/// bare string value if the shape ever differs.
fn doppler_value(root: &serde_json::Value, name: &str) -> Option<String> {
    let entry = root.get(name)?;
    if let Some(s) = entry.as_str() {
        return Some(s.to_string());
    }
    entry
        .get("computed")
        .or_else(|| entry.get("raw"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Fetch `(cluster_url, api_key)` from Doppler. IO (runs the `doppler` CLI).
/// `Err(reason)` carries a console-friendly explanation (used when the operator
/// has no Doppler access).
fn fetch_qdrant_from_doppler(
    process: &dyn ProcessPort,
    cfg: &MemoryProvisionConfig,
) -> Result<(String, String), String> {
    let args = [
        "secrets",
        "get",
        cfg.qdrant_url_secret.as_str(),
        cfg.qdrant_key_secret.as_str(),
        "-p",
        cfg.doppler_project.as_str(),
        "-c",
        cfg.doppler_config.as_str(),
        "--json",
    ];
    // Bounded network call: doppler talks to the Doppler API, and an
    // unreachable API must not hang the synchronous SessionStart hook.
    let timeout = std::time::Duration::from_secs(cfg.doppler_timeout_secs.max(1));
    let out = process
        .run_with_timeout("doppler", &args, None, timeout)
        .map_err(|e| match e {
            sentinel_domain::port_errors::ProcessError::Timeout(_) => format!(
                "`doppler secrets get` timed out after {}s (Doppler API unreachable or hanging); \
                 the process was killed",
                timeout.as_secs()
            ),
            e => format!(
                "the `doppler` CLI could not be run ({e}); install it and ensure it is on PATH"
            ),
        })?;
    if !out.success {
        let err = out.stderr.trim().lines().next().unwrap_or("").trim();
        return Err(format!(
            "`doppler secrets get` failed for {}/{} (not authenticated, or no access){}",
            cfg.doppler_project,
            cfg.doppler_config,
            if err.is_empty() {
                String::new()
            } else {
                format!(": {err}")
            }
        ));
    }
    let v: serde_json::Value = serde_json::from_str(&out.stdout)
        .map_err(|e| format!("could not parse `doppler --json` output ({e})"))?;
    let url = doppler_value(&v, &cfg.qdrant_url_secret).ok_or_else(|| {
        format!(
            "{} not found in {}/{}",
            cfg.qdrant_url_secret, cfg.doppler_project, cfg.doppler_config
        )
    })?;
    let key = doppler_value(&v, &cfg.qdrant_key_secret).ok_or_else(|| {
        format!(
            "{} not found in {}/{}",
            cfg.qdrant_key_secret, cfg.doppler_project, cfg.doppler_config
        )
    })?;
    if url.trim().is_empty() || key.trim().is_empty() {
        return Err(format!(
            "Doppler returned an empty {} / {}",
            cfg.qdrant_url_secret, cfg.qdrant_key_secret
        ));
    }
    Ok((url, key))
}

/// Write `~/.qdrant/config.json` with the fetched url + key, preserving any
/// existing extra fields (`collection_prefix`, ...) and backing up the prior
/// file to `config.json.bak`. All writes (live file via its temp sibling,
/// and the `.bak`) are owner-only (0600 on unix) — this file holds the
/// Qdrant API key. IO.
fn write_qdrant_config(
    fs: &dyn FileSystemPort,
    path: &Path,
    existing: Option<&serde_json::Value>,
    url: &str,
    key: &str,
) -> anyhow::Result<()> {
    let mut obj = existing
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    obj.insert("cluster_url".to_string(), serde_json::json!(url));
    obj.insert("api_key".to_string(), serde_json::json!(key));
    obj.entry("collection_prefix")
        .or_insert_with(|| serde_json::json!("memory"));
    let bytes = serde_json::to_vec_pretty(&serde_json::Value::Object(obj))?;

    if let Ok(orig) = fs.read_to_string(path) {
        let mut bak = path.as_os_str().to_os_string();
        bak.push(".bak");
        let _ = fs.write_private(&PathBuf::from(bak), orig.as_bytes());
    }
    if let Some(parent) = path.parent() {
        let _ = fs.create_dir_all(parent);
    }
    atomic_write(fs, path, &bytes, true)
}

/// Outcome of credential provisioning, surfaced to [`process`].
pub struct CredentialOutcome {
    /// A valid Qdrant credential is now present on disk.
    pub ok: bool,
    /// A refetch from Doppler was performed this run.
    pub fetched: bool,
    /// Human-readable console message (present on fetch or failure).
    pub message: Option<String>,
}

/// Ensure `~/.qdrant/config.json` holds a valid, non-expired credential:
/// verify -> (on missing/invalid/expired) fetch from Doppler -> write. IO.
fn ensure_credential(
    ctx: &HookContext<'_>,
    cfg: &MemoryProvisionConfig,
    now_unix: i64,
) -> CredentialOutcome {
    let Some(path) = qdrant_config_path(ctx.fs) else {
        return CredentialOutcome {
            ok: false,
            fetched: false,
            message: Some("no home directory - cannot locate ~/.qdrant/config.json".to_string()),
        };
    };
    let existing: Option<serde_json::Value> = ctx
        .fs
        .read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    match assess_credential(existing.as_ref(), now_unix) {
        CredentialStatus::Valid => CredentialOutcome {
            ok: true,
            fetched: false,
            message: None,
        },
        status => {
            let reason = if status == CredentialStatus::Expired {
                "expired"
            } else {
                "missing or invalid"
            };
            match fetch_qdrant_from_doppler(ctx.process, cfg) {
                Ok((url, key)) => {
                    // Re-validate the FETCHED pair before declaring success —
                    // Doppler can hand back a stale/expired JWT (e.g. the
                    // secret was never rotated), and writing it would mint a
                    // credential_ok marker for a credential that is already
                    // dead. Assessed exactly like the on-disk one.
                    let fetched_cred =
                        serde_json::json!({ "cluster_url": url.as_str(), "api_key": key.as_str() });
                    if assess_credential(Some(&fetched_cred), now_unix) != CredentialStatus::Valid {
                        return CredentialOutcome {
                            ok: false,
                            fetched: true,
                            message: Some(format!(
                                "Qdrant credential was {reason} and Doppler ({}/{}) returned an \
                                 expired/invalid credential — refusing to write it. Rotate the \
                                 {} / {} secrets in Doppler.",
                                cfg.doppler_project,
                                cfg.doppler_config,
                                cfg.qdrant_url_secret,
                                cfg.qdrant_key_secret
                            )),
                        };
                    }
                    match write_qdrant_config(ctx.fs, &path, existing.as_ref(), &url, &key) {
                        Ok(()) => CredentialOutcome {
                            ok: true,
                            fetched: true,
                            message: Some(format!(
                                "Qdrant credential was {reason}; fetched a fresh one from Doppler ({}/{}) and wrote ~/.qdrant/config.json.",
                                cfg.doppler_project, cfg.doppler_config
                            )),
                        },
                        Err(e) => CredentialOutcome {
                            ok: false,
                            fetched: false,
                            message: Some(format!(
                                "Qdrant credential was {reason} and a fresh one could not be written: {e}"
                            )),
                        },
                    }
                }
                Err(reason_err) => CredentialOutcome {
                    ok: false,
                    fetched: false,
                    message: Some(format!(
                        "Qdrant credential is {reason} and could not be provisioned from Doppler: {reason_err}. \
                         Memory's remote store is UNAVAILABLE. Fix: authenticate Doppler (`doppler login`, or set \
                         DOPPLER_TOKEN) with read access to {}/{}, or place a valid ~/.qdrant/config.json.",
                        cfg.doppler_project, cfg.doppler_config
                    )),
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, StubEnv};
    use sentinel_domain::port_errors::{FileSystemError, ProcessError};
    use sentinel_domain::ports::{FileSystemPort, ProcessOutput};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    // -- Credential assessment / JWT / doppler parsing (PURE) ----------------

    /// Test-only base64url (no-pad) encoder to build JWT fixtures.
    fn b64url_encode(bytes: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = *chunk.get(1).unwrap_or(&0);
            let b2 = *chunk.get(2).unwrap_or(&0);
            let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
            out.push(A[(n >> 18 & 63) as usize] as char);
            out.push(A[(n >> 12 & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(A[(n >> 6 & 63) as usize] as char);
            }
            if chunk.len() > 2 {
                out.push(A[(n & 63) as usize] as char);
            }
        }
        out
    }
    fn make_jwt(exp: i64) -> String {
        let payload = format!("{{\"exp\":{exp}}}");
        format!(
            "eyJhbGciOiJIUzI1NiJ9.{}.sig",
            b64url_encode(payload.as_bytes())
        )
    }

    #[test]
    fn jwt_exp_roundtrips_and_rejects_non_jwt() {
        assert_eq!(jwt_exp(&make_jwt(1_900_000_000)), Some(1_900_000_000));
        assert_eq!(jwt_exp("not-a-jwt"), None);
        assert_eq!(jwt_exp("a.b"), None);
        assert_eq!(jwt_exp("a.b.c.d"), None);
    }

    #[test]
    fn jwt_exp_accepts_padded_payload() {
        // Some issuers pad the payload segment. Rejecting padding made padded
        // tokens un-assessable, i.e. treated as Valid forever.
        let payload = b64url_encode(b"{\"exp\":42}");
        let pad = "=".repeat((4 - payload.len() % 4) % 4);
        assert!(!pad.is_empty(), "fixture must actually exercise padding");
        assert_eq!(jwt_exp(&format!("h.{payload}{pad}.s")), Some(42));
        // '=' anywhere but the end still fails the decode.
        assert_eq!(jwt_exp("h.ab=cd.s"), None);
    }

    #[test]
    fn assess_padded_expired_jwt_is_expired() {
        // Payload length chosen so base64 genuinely needs padding.
        let payload = b64url_encode(b"{\"exp\":100000000}");
        let pad = "=".repeat((4 - payload.len() % 4) % 4);
        assert!(!pad.is_empty(), "fixture must actually exercise padding");
        let token = format!("h.{payload}{pad}.s");
        let v = serde_json::json!({"cluster_url":"https://x:6333","api_key": token});
        assert_eq!(
            assess_credential(Some(&v), 2_000_000_000),
            CredentialStatus::Expired
        );
    }

    #[test]
    fn assess_valid_future_jwt_credential() {
        let v =
            serde_json::json!({"cluster_url":"https://x:6333","api_key": make_jwt(9_000_000_000)});
        assert_eq!(
            assess_credential(Some(&v), 1_000_000_000),
            CredentialStatus::Valid
        );
    }

    #[test]
    fn assess_expired_jwt_credential() {
        let v =
            serde_json::json!({"cluster_url":"https://x:6333","api_key": make_jwt(1_000_000_000)});
        assert_eq!(
            assess_credential(Some(&v), 2_000_000_000),
            CredentialStatus::Expired
        );
    }

    #[test]
    fn assess_missing_or_empty_fields() {
        assert_eq!(
            assess_credential(None, 0),
            CredentialStatus::MissingOrInvalid
        );
        assert_eq!(
            assess_credential(Some(&serde_json::json!({"cluster_url":""})), 0),
            CredentialStatus::MissingOrInvalid
        );
        assert_eq!(
            assess_credential(
                Some(&serde_json::json!({"cluster_url":"u","api_key":""})),
                0
            ),
            CredentialStatus::MissingOrInvalid
        );
    }

    #[test]
    fn assess_non_jwt_key_is_valid_when_present() {
        let v = serde_json::json!({"cluster_url":"https://x","api_key":"plain-opaque-key"});
        assert_eq!(
            assess_credential(Some(&v), 9_999_999_999),
            CredentialStatus::Valid
        );
    }

    #[test]
    fn doppler_value_reads_computed_raw_and_bare_string() {
        let j = serde_json::json!({
            "QDRANT_URL": {"computed":"https://c","raw":"https://r"},
            "K": {"raw":"onlyraw"},
            "S": "bare"
        });
        assert_eq!(
            doppler_value(&j, "QDRANT_URL").as_deref(),
            Some("https://c")
        );
        assert_eq!(doppler_value(&j, "K").as_deref(), Some("onlyraw"));
        assert_eq!(doppler_value(&j, "S").as_deref(), Some("bare"));
        assert_eq!(doppler_value(&j, "MISSING"), None);
    }

    // ── Pure merge ───────────────────────────────────────────────────────────

    #[test]
    fn merge_inserts_when_absent_and_preserves_other_servers() {
        let mut root = serde_json::json!({
            "numStartups": 7,
            "mcpServers": {
                "brave-search": { "type": "stdio", "command": "brave" }
            }
        });
        assert_eq!(merge_memory_server(&mut root, None), Outcome::Merged);
        let servers = &root["mcpServers"];
        // Existing server + key preserved.
        assert_eq!(root["numStartups"], serde_json::json!(7));
        assert_eq!(
            servers["brave-search"]["command"],
            serde_json::json!("brave")
        );
        // New memory server, Qdrant-only (no env).
        assert_eq!(servers["memory"]["type"], serde_json::json!("stdio"));
        assert_eq!(
            servers["memory"]["command"],
            serde_json::json!(MEMORY_COMMAND)
        );
        assert!(
            servers["memory"].get("env").is_none(),
            "qdrant-only ⇒ no env"
        );
    }

    #[test]
    fn merge_creates_mcpservers_when_missing() {
        let mut root = serde_json::json!({ "theme": "dark" });
        assert_eq!(merge_memory_server(&mut root, None), Outcome::Merged);
        assert_eq!(root["theme"], serde_json::json!("dark"));
        assert_eq!(
            root["mcpServers"]["memory"]["command"],
            serde_json::json!(MEMORY_COMMAND)
        );
    }

    #[test]
    fn merge_is_idempotent_when_already_present() {
        let mut root = serde_json::json!({
            "mcpServers": { "memory": { "type": "stdio", "command": "/abs/path/memory-mcp-mirror" } }
        });
        let before = root.clone();
        assert_eq!(
            merge_memory_server(&mut root, Some("https://mem.example/org")),
            Outcome::AlreadyPresent
        );
        assert_eq!(root, before, "existing memory entry must not be clobbered");
    }

    #[test]
    fn merge_remote_some_adds_mirror_env_and_command() {
        let mut root = serde_json::json!({});
        assert_eq!(
            merge_memory_server(&mut root, Some("https://mem.example/org")),
            Outcome::Merged
        );
        let mem = &root["mcpServers"]["memory"];
        assert_eq!(mem["command"], serde_json::json!(MEMORY_MIRROR_COMMAND));
        assert_eq!(
            mem["env"]["MEMORY_REMOTE_URL"],
            serde_json::json!("https://mem.example/org")
        );
        assert_eq!(mem["env"]["MEMORY_MIRROR_ORG"], serde_json::json!("1"));
    }

    #[test]
    fn merge_remote_none_has_no_mirror_env() {
        let mut root = serde_json::json!({});
        let _ = merge_memory_server(&mut root, None);
        assert!(root["mcpServers"]["memory"].get("env").is_none());
    }

    // ── In-memory + disk-backed FS for IO-path tests ─────────────────────────

    /// Disk-backed FS scoped to a temp home so `atomic_write`'s real
    /// `std::fs::rename` exercises the true atomic path.
    struct DiskFs {
        home: PathBuf,
    }
    impl FileSystemPort for DiskFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(&self, p: &Path) -> Result<String, FileSystemError> {
            std::fs::read_to_string(p).map_err(FileSystemError::backend)
        }
        fn write(&self, p: &Path, c: &[u8]) -> Result<(), FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par).map_err(FileSystemError::backend)?;
            }
            std::fs::write(p, c).map_err(FileSystemError::backend)
        }
        fn write_private(&self, p: &Path, c: &[u8]) -> Result<(), FileSystemError> {
            self.write(p, c)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))
                    .map_err(FileSystemError::backend)?;
            }
            Ok(())
        }
        fn remove_file(&self, p: &Path) -> Result<(), FileSystemError> {
            match std::fs::remove_file(p) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(FileSystemError::backend(e)),
            }
        }
        fn create_dir_all(&self, p: &Path) -> Result<(), FileSystemError> {
            std::fs::create_dir_all(p).map_err(FileSystemError::backend)
        }
        fn read_dir(&self, _: &Path) -> Result<Vec<PathBuf>, FileSystemError> {
            Ok(vec![])
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(&self, p: &Path) -> Result<std::fs::Metadata, FileSystemError> {
            std::fs::metadata(p).map_err(FileSystemError::backend)
        }
        fn append(&self, _: &Path, _: &[u8]) -> Result<(), FileSystemError> {
            Ok(())
        }
    }

    /// Pure in-memory FS for marker round-trips.
    #[derive(Clone)]
    struct MemFs {
        home: PathBuf,
        files: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
    }
    impl MemFs {
        fn new() -> Self {
            Self {
                home: PathBuf::from("/mem/home"),
                files: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }
    impl FileSystemPort for MemFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(&self, p: &Path) -> Result<String, FileSystemError> {
            self.files
                .lock()
                .unwrap()
                .get(p)
                .map(|b| String::from_utf8_lossy(b).to_string())
                .ok_or_else(|| FileSystemError::NotFound("not found".into()))
        }
        fn write(&self, p: &Path, c: &[u8]) -> Result<(), FileSystemError> {
            self.files
                .lock()
                .unwrap()
                .insert(p.to_path_buf(), c.to_vec());
            Ok(())
        }
        fn remove_file(&self, p: &Path) -> Result<(), FileSystemError> {
            self.files.lock().unwrap().remove(p);
            Ok(())
        }
        fn create_dir_all(&self, _: &Path) -> Result<(), FileSystemError> {
            Ok(())
        }
        fn read_dir(&self, _: &Path) -> Result<Vec<PathBuf>, FileSystemError> {
            Ok(vec![])
        }
        fn exists(&self, p: &Path) -> bool {
            self.files.lock().unwrap().contains_key(p)
        }
        fn is_dir(&self, _: &Path) -> bool {
            false
        }
        fn metadata(&self, _: &Path) -> Result<std::fs::Metadata, FileSystemError> {
            Err(FileSystemError::backend("no in-memory metadata"))
        }
        fn append(&self, _: &Path, _: &[u8]) -> Result<(), FileSystemError> {
            Ok(())
        }
    }

    // ── IO wrapper: register + backup + atomic + idempotent ──────────────────

    #[test]
    fn register_merges_backs_up_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        let claude = tmp.path().join(".claude.json");
        std::fs::write(
            &claude,
            r#"{"numStartups":3,"mcpServers":{"brave":{"type":"stdio","command":"b"}}}"#,
        )
        .unwrap();

        // First call merges.
        let out = register_memory_mcp(&fs, None).unwrap();
        assert_eq!(out, Outcome::Merged);
        // .bak holds the ORIGINAL bytes.
        let bak = std::fs::read_to_string(tmp.path().join(".claude.json.bak")).unwrap();
        assert!(bak.contains("\"numStartups\":3") && !bak.contains("memory"));
        // Live file now has memory + still has brave + numStartups, pretty-printed.
        let live: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(live["numStartups"], serde_json::json!(3));
        assert_eq!(
            live["mcpServers"]["brave"]["command"],
            serde_json::json!("b")
        );
        assert_eq!(
            live["mcpServers"]["memory"]["command"],
            serde_json::json!(MEMORY_COMMAND)
        );

        // Second call is a no-op.
        let out2 = register_memory_mcp(&fs, None).unwrap();
        assert_eq!(out2, Outcome::AlreadyPresent);
    }

    #[test]
    fn register_refuses_to_clobber_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        let claude = tmp.path().join(".claude.json");
        std::fs::write(&claude, "{ this is not json").unwrap();

        let err = register_memory_mcp(&fs, None).unwrap_err();
        assert!(err.to_string().contains("invalid JSON"), "got: {err}");
        // File untouched, no backup written.
        assert_eq!(
            std::fs::read_to_string(&claude).unwrap(),
            "{ this is not json"
        );
        assert!(!tmp.path().join(".claude.json.bak").exists());
    }

    #[test]
    fn register_refuses_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        let err = register_memory_mcp(&fs, None).unwrap_err();
        assert!(err.to_string().contains("unreadable"), "got: {err}");
        assert!(
            !tmp.path().join(".claude.json").exists(),
            "must not create the file"
        );
    }

    #[test]
    fn register_rename_failure_propagates_and_cleans_tmp() {
        // MemFs is not disk-backed, so `std::fs::rename` of the temp sibling
        // fails — the old code silently degraded to a non-atomic write; the
        // hardened path must propagate the error and remove the temp file.
        let fs = MemFs::new();
        let live = PathBuf::from("/mem/home/.claude.json");
        let original = r#"{"mcpServers":{}}"#;
        fs.write(&live, original.as_bytes()).unwrap();

        let err = register_memory_mcp(&fs, None).unwrap_err();
        assert!(err.to_string().contains("atomic rename"), "got: {err}");

        let files = fs.files.lock().unwrap();
        // Live file untouched — no silent non-atomic fallback.
        assert_eq!(
            files
                .get(&live)
                .map(|b| String::from_utf8_lossy(b).to_string()),
            Some(original.to_string()),
            "live file must not be rewritten when the rename fails"
        );
        // No leftover temp file.
        assert!(
            !files
                .keys()
                .any(|p| p.to_string_lossy().contains(".claude.json.tmp.")),
            "temp file must be cleaned up on rename failure; files: {:?}",
            files.keys().collect::<Vec<_>>()
        );
    }

    // ── Pure removal + deregistration ────────────────────────────────────────

    #[test]
    fn remove_memory_server_removes_only_managed_memory() {
        let mut root = serde_json::json!({
            "mcpServers": {
                "memory": { "type": "stdio", "command": "memory-mcp", "args": [] },
                "brave": { "type": "stdio", "command": "b" }
            }
        });
        assert_eq!(remove_memory_server(&mut root), RemoveOutcome::Removed);
        assert!(root["mcpServers"].get("memory").is_none());
        assert_eq!(
            root["mcpServers"]["brave"]["command"],
            serde_json::json!("b")
        );
        // Idempotent: nothing left to remove.
        assert_eq!(remove_memory_server(&mut root), RemoveOutcome::NotPresent);
        // Unexpected shapes are left untouched.
        let mut weird = serde_json::json!({ "mcpServers": "nope" });
        assert_eq!(remove_memory_server(&mut weird), RemoveOutcome::NotPresent);
        assert_eq!(weird["mcpServers"], serde_json::json!("nope"));
    }

    #[test]
    fn remove_memory_server_leaves_foreign_entry_untouched() {
        // A user-owned custom memory server must NEVER be deleted by the
        // withhold path — the registration path refuses to clobber it, so
        // removal must be equally conservative.
        let mut root = serde_json::json!({
            "mcpServers": {
                "memory": { "type": "stdio", "command": "/opt/custom/my-memory", "args": ["--db", "x"] }
            }
        });
        let before = root.clone();
        assert_eq!(remove_memory_server(&mut root), RemoveOutcome::ForeignEntry);
        assert_eq!(root, before, "foreign entry must not be touched");
    }

    #[test]
    fn is_sentinel_managed_entry_matches_exact_written_shapes() {
        // Exactly what merge_memory_server writes: Qdrant-only…
        let mut root = serde_json::json!({});
        let _ = merge_memory_server(&mut root, None);
        assert!(is_sentinel_managed_entry(&root["mcpServers"]["memory"]));
        // …and mirror-configured.
        let mut root = serde_json::json!({});
        let _ = merge_memory_server(&mut root, Some("https://mem.example/org"));
        assert!(is_sentinel_managed_entry(&root["mcpServers"]["memory"]));

        // Anything a user could plausibly have customized is foreign:
        for foreign in [
            // custom launcher path
            serde_json::json!({"type":"stdio","command":"/abs/path/memory-mcp"}),
            // extra args
            serde_json::json!({"type":"stdio","command":"memory-mcp","args":["--flag"]}),
            // custom env
            serde_json::json!({"type":"stdio","command":"memory-mcp","env":{"MY_KEY":"v"}}),
            // extra top-level key
            serde_json::json!({"type":"stdio","command":"memory-mcp","cwd":"/x"}),
            // different transport
            serde_json::json!({"type":"http","command":"memory-mcp"}),
            // non-object
            serde_json::json!("memory-mcp"),
        ] {
            assert!(
                !is_sentinel_managed_entry(&foreign),
                "must be foreign: {foreign}"
            );
        }
    }

    // ── Scripted process stub (Doppler CLI scenarios) ────────────────────────

    /// `ProcessPort` stub returning a canned result for every `run` /
    /// `run_with_timeout` call — models the `doppler` CLI succeeding,
    /// failing, or timing out. Extends the `test_support::StubProcess`
    /// pattern (hooks/mod.rs) with a configurable outcome.
    struct ScriptedProcess {
        result: Result<ProcessOutput, ProcessError>,
    }
    impl ScriptedProcess {
        fn ok(stdout: &str) -> Self {
            Self {
                result: Ok(ProcessOutput {
                    success: true,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                }),
            }
        }
        fn fails(stderr: &str) -> Self {
            Self {
                result: Ok(ProcessOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                }),
            }
        }
        fn times_out() -> Self {
            Self {
                result: Err(ProcessError::Timeout(
                    "doppler exceeded 10s and was killed".into(),
                )),
            }
        }
    }
    impl ProcessPort for ScriptedProcess {
        fn run(&self, _: &str, _: &[&str], _: Option<&str>) -> Result<ProcessOutput, ProcessError> {
            self.result.clone()
        }
        fn spawn_detached(&self, _: &str, _: &[&str]) -> Result<(), ProcessError> {
            Ok(())
        }
    }

    fn doppler_json(url: &str, key: &str) -> String {
        serde_json::json!({
            "QDRANT_URL": { "computed": url },
            "QDRANT_API_KEY": { "computed": key },
        })
        .to_string()
    }

    /// Context wiring a disk-backed home + a scripted `doppler`.
    fn ctx_with<'a>(fs: &'a dyn FileSystemPort, process: &'a dyn ProcessPort) -> HookContext<'a> {
        HookContext {
            process,
            ..stub_ctx_with_fs(fs)
        }
    }

    // ── ensure_credential orchestration ──────────────────────────────────────

    #[test]
    fn ensure_credential_fetch_success_writes_config() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        let proc = ScriptedProcess::ok(&doppler_json("https://q.example:6333", "opaque-key"));
        let ctx = ctx_with(&fs, &proc);
        let cfg = MemoryProvisionConfig::default();

        let out = ensure_credential(&ctx, &cfg, 1_000_000_000);
        assert!(out.ok, "got message: {:?}", out.message);
        assert!(out.fetched);
        assert!(out.message.unwrap().contains("fetched a fresh one"));

        let cred_path = tmp.path().join(".qdrant").join("config.json");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cred_path).unwrap()).unwrap();
        assert_eq!(
            v["cluster_url"],
            serde_json::json!("https://q.example:6333")
        );
        assert_eq!(v["api_key"], serde_json::json!("opaque-key"));
        assert_eq!(v["collection_prefix"], serde_json::json!("memory"));

        // Credential material lands owner-only on disk.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&cred_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "~/.qdrant/config.json must be 0600");
        }
    }

    #[test]
    fn ensure_credential_fetch_failure_reports_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        let proc = ScriptedProcess::fails("Doppler Error: not authenticated");
        let ctx = ctx_with(&fs, &proc);
        let cfg = MemoryProvisionConfig::default();

        let out = ensure_credential(&ctx, &cfg, 1_000_000_000);
        assert!(!out.ok);
        assert!(!out.fetched);
        let msg = out.message.unwrap();
        assert!(
            msg.contains("could not be provisioned from Doppler"),
            "got: {msg}"
        );
        assert!(msg.contains("not authenticated"), "got: {msg}");
        assert!(
            !tmp.path().join(".qdrant").join("config.json").exists(),
            "must not write a credential file on fetch failure"
        );
    }

    #[test]
    fn ensure_credential_rejects_expired_token_from_doppler() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        // Doppler "succeeds" but hands back a JWT that is already expired.
        let proc = ScriptedProcess::ok(&doppler_json(
            "https://q.example:6333",
            &make_jwt(1_000_000_000),
        ));
        let ctx = ctx_with(&fs, &proc);
        let cfg = MemoryProvisionConfig::default();

        let out = ensure_credential(&ctx, &cfg, 2_000_000_000);
        assert!(
            !out.ok,
            "an expired fetched credential must not count as ok"
        );
        assert!(out.fetched, "the fetch itself did happen");
        let msg = out.message.unwrap();
        assert!(
            msg.contains("expired/invalid credential"),
            "failure reason must be distinct: {msg}"
        );
        assert!(
            !tmp.path().join(".qdrant").join("config.json").exists(),
            "an expired fetched credential must NOT be written to disk"
        );
    }

    #[test]
    fn ensure_credential_doppler_timeout_is_fetch_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        let proc = ScriptedProcess::times_out();
        let ctx = ctx_with(&fs, &proc);
        let cfg = MemoryProvisionConfig::default();

        let out = ensure_credential(&ctx, &cfg, 1_000_000_000);
        assert!(!out.ok);
        let msg = out.message.unwrap();
        assert!(msg.contains("timed out after 10s"), "got: {msg}");
        assert!(!tmp.path().join(".qdrant").join("config.json").exists());
    }

    // ── process() end-to-end ─────────────────────────────────────────────────

    fn read_marker(fs: &dyn FileSystemPort) -> ProvisionMarker {
        let path = marker_path(fs).unwrap();
        serde_json::from_str(&fs.read_to_string(&path).unwrap()).unwrap()
    }

    /// Create an executable fixture file (exec bit set on unix — a plain
    /// data file must NOT count as an installed binary).
    fn write_executable(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn install_fake_binaries(home: &Path) {
        for name in ["memory", "memory-mcp"] {
            write_executable(&home.join(".cargo").join("bin").join(name));
        }
    }

    fn write_valid_credential(home: &Path) {
        let p = home.join(".qdrant").join("config.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            r#"{"cluster_url":"https://q:6333","api_key":"opaque-key"}"#,
        )
        .unwrap();
    }

    #[test]
    fn process_withhold_removes_preexisting_memory_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        install_fake_binaries(tmp.path());
        // A previous (credentialed) session registered the memory MCP…
        let claude = tmp.path().join(".claude.json");
        std::fs::write(
            &claude,
            r#"{"mcpServers":{"memory":{"type":"stdio","command":"memory-mcp"},"brave":{"type":"stdio","command":"b"}}}"#,
        )
        .unwrap();
        // …but now there is no credential and Doppler fails.
        let proc = ScriptedProcess::fails("not authenticated");
        let ctx = ctx_with(&fs, &proc);

        let out = process(&HookInput::default(), &ctx);
        let rendered = serde_json::to_string(&out).unwrap();
        assert!(rendered.contains("NOT registered"), "got: {rendered}");
        assert!(
            rendered.contains("REMOVED"),
            "warn message must say the stale entry was removed: {rendered}"
        );

        // The stale entry is gone; other servers survive.
        let live: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
        assert!(
            live["mcpServers"].get("memory").is_none(),
            "require_remote must hold on re-runs: {live}"
        );
        assert_eq!(
            live["mcpServers"]["brave"]["command"],
            serde_json::json!("b")
        );
        // Backup of the pre-removal document exists and still has the entry.
        let bak = std::fs::read_to_string(tmp.path().join(".claude.json.bak")).unwrap();
        assert!(bak.contains("memory"));

        let marker = read_marker(&fs);
        assert!(!marker.mcp_registered);
        assert!(!marker.credential_ok);
        assert!(!is_provisioned(&fs));
    }

    #[test]
    fn process_withhold_without_existing_entry_does_not_touch_file() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        let claude = tmp.path().join(".claude.json");
        std::fs::write(&claude, r#"{"mcpServers":{}}"#).unwrap();
        let proc = ScriptedProcess::fails("not authenticated");
        let ctx = ctx_with(&fs, &proc);

        let out = process(&HookInput::default(), &ctx);
        let rendered = serde_json::to_string(&out).unwrap();
        assert!(rendered.contains("NOT registered"), "got: {rendered}");
        assert!(!rendered.contains("REMOVED"), "got: {rendered}");
        // No rewrite, no backup — nothing was removed.
        assert_eq!(
            std::fs::read_to_string(&claude).unwrap(),
            r#"{"mcpServers":{}}"#
        );
        assert!(!tmp.path().join(".claude.json.bak").exists());
    }

    #[test]
    fn process_skips_registration_when_binaries_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        // Credential is fine — only the engine binaries are missing.
        write_valid_credential(tmp.path());
        let claude = tmp.path().join(".claude.json");
        std::fs::write(&claude, r#"{"mcpServers":{}}"#).unwrap();
        let proc = ScriptedProcess::fails("doppler must not be called");
        let ctx = ctx_with(&fs, &proc);

        let out = process(&HookInput::default(), &ctx);
        let rendered = serde_json::to_string(&out).unwrap();
        assert!(rendered.contains("registration skipped"), "got: {rendered}");
        assert!(
            rendered.contains("cargo install"),
            "install hint must be surfaced: {rendered}"
        );

        let live: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
        assert!(
            live["mcpServers"].get("memory").is_none(),
            "must not register a PATH-resolved command that does not exist"
        );
        let marker = read_marker(&fs);
        assert!(!marker.binaries_present);
        assert!(!marker.mcp_registered);
        assert!(marker.credential_ok);
    }

    #[test]
    fn process_registers_when_binaries_and_credential_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        install_fake_binaries(tmp.path());
        write_valid_credential(tmp.path());
        let claude = tmp.path().join(".claude.json");
        std::fs::write(&claude, r#"{"mcpServers":{}}"#).unwrap();
        let proc = ScriptedProcess::fails("doppler must not be called");
        let ctx = ctx_with(&fs, &proc);

        let _ = process(&HookInput::default(), &ctx);

        let live: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(
            live["mcpServers"]["memory"]["command"],
            serde_json::json!(MEMORY_COMMAND)
        );
        assert!(is_provisioned(&fs));
    }

    #[test]
    fn process_withhold_leaves_foreign_memory_entry_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        install_fake_binaries(tmp.path());
        // The user runs their OWN memory server under the `memory` key.
        let original = r#"{"mcpServers":{"memory":{"type":"stdio","command":"/opt/custom/my-memory","args":["--db","x"]}}}"#;
        let claude = tmp.path().join(".claude.json");
        std::fs::write(&claude, original).unwrap();
        let proc = ScriptedProcess::fails("not authenticated");
        let ctx = ctx_with(&fs, &proc);

        let out = process(&HookInput::default(), &ctx);
        let rendered = serde_json::to_string(&out).unwrap();
        assert!(
            rendered.contains("NOT removed"),
            "must warn the foreign entry was left alone: {rendered}"
        );
        // File byte-for-byte untouched, no backup.
        assert_eq!(std::fs::read_to_string(&claude).unwrap(), original);
        assert!(!tmp.path().join(".claude.json.bak").exists());
    }

    #[test]
    fn process_skips_registration_when_mirror_binary_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        // memory + memory-mcp installed, but NOT memory-mcp-mirror…
        install_fake_binaries(tmp.path());
        write_valid_credential(tmp.path());
        let claude = tmp.path().join(".claude.json");
        std::fs::write(&claude, r#"{"mcpServers":{}}"#).unwrap();
        // …while a mirror is configured, so `memory-mcp-mirror` is the
        // command that WOULD be registered.
        let proc = ScriptedProcess::fails("doppler must not be called");
        let env = StubEnv::with(&[("MEMORY_REMOTE_URL", "https://mem.example/org")]);
        let ctx = HookContext {
            process: &proc,
            env: &env,
            ..stub_ctx_with_fs(&fs)
        };

        let out = process(&HookInput::default(), &ctx);
        let rendered = serde_json::to_string(&out).unwrap();
        assert!(
            rendered.contains(MEMORY_MIRROR_COMMAND),
            "skip warning must name the missing mirror launcher: {rendered}"
        );
        let live: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
        assert!(
            live["mcpServers"].get("memory").is_none(),
            "must not register a mirror launcher that is not installed"
        );

        // Installing the mirror binary flips the gate.
        write_executable(
            &tmp.path()
                .join(".cargo")
                .join("bin")
                .join(MEMORY_MIRROR_COMMAND),
        );
        let _ = process(&HookInput::default(), &ctx);
        let live: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(
            live["mcpServers"]["memory"]["command"],
            serde_json::json!(MEMORY_MIRROR_COMMAND)
        );
    }

    #[cfg(unix)]
    #[test]
    fn register_preserves_tightened_claude_json_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: tmp.path().to_path_buf(),
        };
        let claude = tmp.path().join(".claude.json");
        std::fs::write(&claude, r#"{"mcpServers":{}}"#).unwrap();
        // The user tightened their ~/.claude.json (it can carry other MCP
        // servers' env secrets) — the atomic rewrite must not loosen it.
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(register_memory_mcp(&fs, None).unwrap(), Outcome::Merged);
        let mode = std::fs::metadata(&claude).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "registration rewrite must preserve the destination's mode"
        );
    }

    // ── PATH probing ─────────────────────────────────────────────────────────

    #[test]
    fn find_binary_probes_path_dirs() {
        let home = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: home.path().to_path_buf(),
        };
        let bindir = tempfile::tempdir().unwrap();
        write_executable(&bindir.path().join("memory"));
        write_executable(&bindir.path().join("memory-mcp"));

        let env_with = StubEnv::with(&[("PATH", bindir.path().to_str().unwrap())]);
        assert!(
            binaries_present(&fs, &env_with, MEMORY_COMMAND),
            "binaries on PATH must count — the registered command is PATH-resolved"
        );
        let env_empty = StubEnv::new();
        assert!(!binaries_present(&fs, &env_empty, MEMORY_COMMAND));
        // The mirror launcher is a distinct binary: memory + memory-mcp on
        // PATH is NOT enough when the mirror command would be registered.
        assert!(
            !binaries_present(&fs, &env_with, MEMORY_MIRROR_COMMAND),
            "mirror launcher must be gated on its own binary"
        );
        write_executable(&bindir.path().join(MEMORY_MIRROR_COMMAND));
        assert!(binaries_present(&fs, &env_with, MEMORY_MIRROR_COMMAND));
    }

    #[cfg(unix)]
    #[test]
    fn find_binary_rejects_non_executable_file() {
        let home = tempfile::tempdir().unwrap();
        let fs = DiskFs {
            home: home.path().to_path_buf(),
        };
        // A plain (non-executable) data file at the conventional install
        // path must not count as an installed binary.
        let p = home.path().join(".cargo").join("bin").join("memory");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"not a binary").unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let env = StubEnv::new();
        assert!(!find_binary(&fs, &env, "memory"));
        // Same file with an exec bit counts.
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(find_binary(&fs, &env, "memory"));
    }

    // ── Readiness marker ─────────────────────────────────────────────────────

    #[test]
    fn marker_round_trip_fields_and_is_provisioned() {
        let fs = MemFs::new();
        assert!(!is_provisioned(&fs), "no marker ⇒ not provisioned");

        // binaries present but not registered ⇒ NOT ready.
        record_provisioned(&fs, true, false, false, false);
        assert!(!is_provisioned(&fs));

        // registered but no binaries ⇒ NOT ready.
        record_provisioned(&fs, false, true, false, false);
        assert!(!is_provisioned(&fs));

        // both ⇒ ready; fields persist.
        record_provisioned(&fs, true, true, true, true);
        assert!(is_provisioned(&fs));
        let path = marker_path(&fs).unwrap();
        let m: ProvisionMarker = serde_json::from_str(&fs.read_to_string(&path).unwrap()).unwrap();
        assert!(m.binaries_present && m.mcp_registered && m.remote_configured);
        assert!(!m.ts.is_empty());
    }

    // ── Config + remote resolution ───────────────────────────────────────────

    #[test]
    fn shipped_defaults_parse() {
        let cfg = MemoryProvisionConfig::from_toml_or_default(SHIPPED_DEFAULTS);
        assert!(cfg.enabled);
        assert!(cfg.register_mcp);
        assert!(cfg.org_mirror_url.is_empty());
        assert_eq!(cfg.doppler_timeout_secs, 10);
    }
}
