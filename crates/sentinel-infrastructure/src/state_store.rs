//! State Store
//!
//! Persists session state to disk. Uses a single JSON file per session
//! instead of the 13+ temp files the Node.js hooks use.
//!
//! Uses file locking to prevent race conditions from concurrent writes.
//!
//! **Integrity protection (Attack #42)**: Each state file is paired with an
//! HMAC-SHA256 signature file (`.json.sig`). The HMAC key is derived from the
//! machine hostname + username + a sentinel-specific salt. This prevents:
//!   - Bash `echo '{}' > state.json` — tampered JSON won't have a valid sig
//!   - Manual editing of state files to forge workflow progress
//!   - Cross-session state injection (different session_id = different file)
//!
//! The HMAC is NOT cryptographically unbreakable (the key is on the same
//! machine), but it raises the bar from "trivial Bash one-liner" to "must
//! reverse-engineer the sentinel binary to extract the key derivation".

use std::path::PathBuf;

use anyhow::{Context, Result};
use fs2::FileExt;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use sentinel_domain::state::SessionState;

type HmacSha256 = Hmac<Sha256>;

/// Validate that a session ID is safe for use in file paths.
/// Rejects any session_id containing path traversal characters (`/`, `\`, `..`)
/// or non-ASCII characters. Only allows: alphanumeric, hyphen, underscore, dot.
/// Maximum length: 128 characters.
///
/// **Attack #43**: Without this, `session_id = "../../.bashrc"` writes outside
/// the state directory via path traversal.
/// Validate session ID for safe filesystem use.
/// **Attack #121 fix**: Without this, `session_id = "../../etc/passwd"` writes state
/// files outside the intended directory via path traversal.
/// **Attack #146 note**: Made public so the daemon API can reuse it for request validation.
pub fn sanitize_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty() {
        anyhow::bail!("Session ID is empty");
    }
    if session_id.len() > 128 {
        anyhow::bail!("Session ID too long (max 128 chars): {}", session_id.len());
    }
    if session_id.contains("..") {
        anyhow::bail!("Session ID contains path traversal: '{}'", session_id);
    }
    if !session_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!(
            "Session ID contains unsafe characters (only alphanumeric, hyphen, underscore allowed): '{}'",
            session_id
        );
    }
    Ok(())
}

/// State storage directory
///
/// **Attack #85 fix**: Panic instead of falling back to `"."` when HOME is unset.
/// The `"."` fallback writes state files to CWD (attacker-controlled project dir),
/// letting an attacker plant forged state files that sentinel will load as real session state.
pub fn state_dir() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory. HOME/USERPROFILE must be set.")
        .join(".claude")
        .join("sentinel")
        .join("state")
}

/// Encode bytes as hexadecimal string (avoids `hex` crate dependency)
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode hexadecimal string to bytes (avoids `hex` crate dependency)
fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Base directory for HMAC secret files.
fn hmac_secret_dir() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory. HOME/USERPROFILE must be set.")
        .join(".claude")
        .join("sentinel")
}

/// Path to the random secret file used for HMAC key derivation.
///
/// **Key rotation support**: Secrets are now versioned as `.hmac-secret-v{N}`.
/// The legacy unversioned `.hmac-secret` is treated as v1 for backward compat.
///
/// **Attack #86 fix**: Panic instead of falling back to `"."` when HOME is unset.
fn hmac_secret_path() -> PathBuf {
    hmac_secret_dir().join(".hmac-secret")
}

/// Path to a versioned secret file.
fn hmac_secret_path_versioned(version: u32) -> PathBuf {
    hmac_secret_dir().join(format!(".hmac-secret-v{version}"))
}

/// Discover all key versions available on disk.
/// Returns sorted list of (version, path) pairs, highest version last.
fn discover_key_versions() -> Vec<(u32, PathBuf)> {
    let dir = hmac_secret_dir();
    let mut versions = Vec::new();

    // Check legacy unversioned secret (treated as v1)
    let legacy = dir.join(".hmac-secret");
    if legacy.exists() {
        // Only count as v1 if no explicit v1 exists
        let v1_path = dir.join(".hmac-secret-v1");
        if !v1_path.exists() {
            versions.push((1, legacy));
        }
    }

    // Scan for versioned secrets
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(v_str) = name.strip_prefix(".hmac-secret-v") {
                if let Ok(v) = v_str.parse::<u32>() {
                    versions.push((v, entry.path()));
                }
            }
        }
    }

    versions.sort_by_key(|(v, _)| *v);
    versions
}

/// Get the current (latest) key version number.
fn current_key_version() -> u32 {
    discover_key_versions().last().map(|(v, _)| *v).unwrap_or(1)
}

/// Create a new key version. Returns the new version number.
/// The old key versions are preserved for verification of existing signatures.
pub fn rotate_hmac_key() -> Result<u32> {
    let new_version = current_key_version() + 1;
    let path = hmac_secret_path_versioned(new_version);

    let mut secret = vec![0u8; 32];
    getrandom::getrandom(&mut secret)
        .map_err(|e| anyhow::anyhow!("CSPRNG failed during key rotation: {e}"))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &secret)?;
    restrict_secret_permissions(&path);

    eprintln!(
        "[sentinel] HMAC key rotated to version {new_version}. \
         Old versions preserved for signature verification."
    );

    Ok(new_version)
}

/// Get or create the random secret for HMAC key derivation.
///
/// **Attack #44**: The original key was derived entirely from observable
/// system properties (`COMPUTERNAME:USERNAME:salt:exe_path`). An attacker
/// could reproduce it via Bash and forge valid HMACs. Adding a random
/// secret file makes the key non-derivable from system observations.
/// The secret is readable only by the owning user (mode 0600 on Unix).
fn get_or_create_secret() -> Vec<u8> {
    // Use the latest versioned key if available, else fall back to legacy
    let versions = discover_key_versions();
    if let Some((_, ref path)) = versions.last() {
        if let Ok(bytes) = std::fs::read(path) {
            if bytes.len() >= 32 {
                validate_secret_permissions(path);
                return bytes;
            }
        }
    }

    // Fall back to legacy path for backward compat
    let path = hmac_secret_path();
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() >= 32 {
            validate_secret_permissions(&path);
            return bytes;
        }
    }

    // **Attack #63 fix**: Use getrandom for proper CSPRNG entropy.
    // The previous approach used observable system properties (time, PID, stack addr)
    // which could be predicted by an attacker on the same machine.
    let mut secret = vec![0u8; 32];
    if getrandom::getrandom(&mut secret).is_err() {
        // Last resort fallback if CSPRNG fails (should never happen on supported platforms)
        use sha2::Digest;
        let mut hasher = Sha256::new();
        hasher.update(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_le_bytes(),
        );
        hasher.update(std::process::id().to_le_bytes());
        hasher.update(format!("{:?}", std::thread::current().id()).as_bytes());
        secret = hasher.finalize().to_vec();
    }

    // Write atomically — create parent dirs if needed
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &secret);

    // Restrict permissions to owner-only
    restrict_secret_permissions(&path);

    secret
}

/// Restrict file permissions to owner-only on all platforms.
/// On Windows, uses `icacls` to remove inherited permissions and grant only the current user.
/// On Unix, uses chmod 600.
fn restrict_secret_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }

    #[cfg(windows)]
    {
        // Use icacls to restrict to current user only:
        // 1. Disable inheritance, removing inherited ACEs
        // 2. Grant current user full control
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let path_str = path.to_string_lossy();
        let username = std::env::var("USERNAME").unwrap_or_default();
        if !username.is_empty() {
            // Disable inheritance and remove inherited ACEs
            let _ = std::process::Command::new("icacls")
                .args([path_str.as_ref(), "/inheritance:r"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            // Grant only current user full control
            let _ = std::process::Command::new("icacls")
                .args([path_str.as_ref(), "/grant:r", &format!("{username}:F")])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
    }
}

/// Validate that the HMAC secret file has restricted permissions.
/// Warns if the file is readable by other users (potential key compromise).
fn validate_secret_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mode = metadata.permissions().mode();
            if mode & 0o077 != 0 {
                eprintln!(
                    "[sentinel] WARNING: HMAC secret file has overly permissive mode {:o}. \
                     Expected 600 (owner-only). Run: chmod 600 {}",
                    mode & 0o777,
                    path.display()
                );
                // Auto-fix
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }

    // On Windows, we trust the icacls setup from creation. Checking ACLs
    // programmatically requires the windows-sys crate which isn't worth
    // adding for this single check.
    #[cfg(windows)]
    {
        let _ = path;
    }
}

/// Derive the HMAC signing key from machine-local entropy PLUS a random secret.
///
/// The random secret file (`~/.claude/sentinel/.hmac-secret`) is generated once
/// and makes the key non-reproducible from system environment variables alone.
/// The machine properties are still mixed in as defense-in-depth (prevents
/// copying the secret file to a different machine and forging state there).
fn derive_hmac_key() -> Vec<u8> {
    let hostname = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    let username = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    let salt = "sentinel-state-integrity-v2";
    let secret = get_or_create_secret();
    let mut key_material = format!("{hostname}:{username}:{salt}:");
    // Add the random secret
    key_material.push_str(&to_hex(&secret));
    // SHA-256 the key material to get a fixed-size key
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(key_material.as_bytes());
    hasher.finalize().to_vec()
}

/// Expose HMAC key derivation for proof_store to reuse the same signing key.
/// **Attack #123**: Proof files need the same integrity protection as state files.
pub fn derive_hmac_key_for_proofs() -> Vec<u8> {
    derive_hmac_key()
}

/// Compute versioned HMAC for proof_store (delegates to internal compute_hmac).
pub fn compute_hmac_for_proofs(data: &[u8]) -> String {
    compute_hmac(data)
}

/// Verify versioned HMAC for proof_store (delegates to internal verify_hmac).
pub fn verify_hmac_for_proofs(data: &[u8], sig_str: &str) -> bool {
    verify_hmac(data, sig_str)
}

/// Compute HMAC-SHA256 of the given data using the current (latest) key.
/// Returns a versioned signature string: `v{N}:{hex}`.
fn compute_hmac(data: &[u8]) -> String {
    let version = current_key_version();
    let key = derive_hmac_key();
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts any key size");
    mac.update(data);
    let result = mac.finalize();
    let hex = to_hex(&result.into_bytes());
    format!("v{version}:{hex}")
}

/// Derive an HMAC key for a specific key version (used during verification).
fn derive_hmac_key_for_version(version: u32) -> Option<Vec<u8>> {
    let path = hmac_secret_path_versioned(version);
    let secret = if path.exists() {
        std::fs::read(&path).ok()?
    } else if version == 1 {
        // Fall back to legacy unversioned path for v1
        std::fs::read(hmac_secret_path()).ok()?
    } else {
        return None;
    };

    if secret.len() < 32 {
        return None;
    }

    let hostname = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    let username = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    let salt = "sentinel-state-integrity-v2";
    let key_material = format!("{hostname}:{username}:{salt}:{}", to_hex(&secret));

    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(key_material.as_bytes());
    Some(hasher.finalize().to_vec())
}

/// Verify HMAC-SHA256 of the given data against expected signature.
/// Supports versioned signatures (`v{N}:{hex}`) and legacy unversioned (`{hex}`).
/// For versioned sigs, only tries the specified key version.
/// For legacy sigs, tries the current key (backward compat).
fn verify_hmac(data: &[u8], sig_str: &str) -> bool {
    // Parse versioned signature: "v{N}:{hex}"
    let (version, hex) = if let Some(rest) = sig_str.strip_prefix('v') {
        if let Some((v_str, hex_part)) = rest.split_once(':') {
            if let Ok(v) = v_str.parse::<u32>() {
                (Some(v), hex_part)
            } else {
                (None, sig_str) // Malformed version, treat as legacy
            }
        } else {
            (None, sig_str) // No colon, treat as legacy
        }
    } else {
        (None, sig_str) // No 'v' prefix, legacy format
    };

    let expected = match from_hex(hex) {
        Some(v) => v,
        None => return false,
    };

    // Derive the correct key based on version
    let key = if let Some(v) = version {
        // Versioned signature: use the specific key version
        match derive_hmac_key_for_version(v) {
            Some(k) => k,
            None => return false, // Unknown version, reject
        }
    } else {
        // Legacy unversioned signature: try current key
        derive_hmac_key()
    };

    let mut mac = match HmacSha256::new_from_slice(&key) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(data);
    mac.verify_slice(&expected).is_ok()
}

/// Save session state to disk (atomic write with file lock + HMAC)
///
/// Uses an exclusive lock file to serialize concurrent writes,
/// preventing corruption from parallel hook invocations.
/// Writes a companion `.json.sig` file with HMAC-SHA256 signature.
pub fn save(state: &mut SessionState) -> Result<()> {
    sanitize_session_id(&state.session_id)?;

    // **Attack #81 fix**: Increment monotonic generation counter before saving.
    // This makes every save strictly newer than the previous one. If state is
    // loaded and has a lower generation than expected, the file was deleted
    // and recreated (state regression attack).
    state.state_generation += 1;

    let dir = state_dir();
    std::fs::create_dir_all(&dir).context("Failed to create state directory")?;

    let path = dir.join(format!("{}.json", state.session_id));
    let sig_path = dir.join(format!("{}.json.sig", state.session_id));
    let tmp_path = dir.join(format!("{}.json.tmp", state.session_id));

    // NOTE: No lock acquired here — the caller (hook_cmd) already holds the
    // session lock via acquire_session_lock(). Taking a second exclusive lock
    // on the same file deadlocks on Windows (per-handle, not re-entrant).

    let json = serde_json::to_string_pretty(state)?;

    // Compute HMAC before writing
    let sig = compute_hmac(json.as_bytes());

    // **Attack #53 fix**: Write sig BEFORE the atomic rename of JSON.
    // If we crash between writing JSON and sig, the next load() sees JSON
    // without a sig → treats it as unsigned → rejects. By writing sig first,
    // a stale sig for non-existent JSON is harmless, and once JSON lands
    // the sig is guaranteed present.
    let tmp_sig_path = dir.join(format!("{}.json.sig.tmp", state.session_id));
    std::fs::write(&tmp_sig_path, &sig).context("Failed to write temp state signature")?;
    std::fs::rename(&tmp_sig_path, &sig_path).context("Failed to rename temp sig file")?;

    std::fs::write(&tmp_path, &json).context("Failed to write temp state file")?;
    std::fs::rename(&tmp_path, &path).context("Failed to rename temp state file")?;

    Ok(())
}

/// Acquire an exclusive session lock for the entire load-process-save cycle.
/// **Attack #67 fix**: Without this, concurrent hook invocations race between
/// load and save, causing lost updates (state regression). Hold this lock
/// from before load() through save() to serialize access.
///
/// Returns the lock file handle — dropping it releases the lock.
pub fn acquire_session_lock(session_id: &str) -> Result<std::fs::File> {
    sanitize_session_id(session_id)?;
    let dir = state_dir();
    std::fs::create_dir_all(&dir).context("Failed to create state directory")?;
    let lock_path = dir.join(format!("{session_id}.json.lock"));
    let lock_file = std::fs::File::create(&lock_path).context("Failed to create lock file")?;

    // **Attack #186 fix**: Restrict lock file permissions on Unix.
    // Default umask may leave lock files world-writable, allowing another
    // user to hold the lock and block or hijack session state updates.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600));
    }

    // Try non-blocking lock with retry + timeout.
    // If a previous sentinel process hung (e.g., on stdin), its lock may still
    // be held. Blocking indefinitely here would cascade the hang to all
    // subsequent hook invocations for this session.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match lock_file.try_lock_exclusive() {
            Ok(()) => return Ok(lock_file),
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => {
                tracing::warn!(
                    session_id,
                    "Session lock contention — proceeding without lock after 2s timeout"
                );
                // Proceed without lock rather than blocking forever.
                // Slight risk of lost state updates, but vastly better than hanging.
                return Ok(lock_file);
            }
        }
    }
}

/// Load session state from disk with HMAC integrity verification.
/// Caller should hold the session lock (from `acquire_session_lock`).
pub fn load(session_id: &str) -> Result<Option<SessionState>> {
    sanitize_session_id(session_id)?;

    let path = state_dir().join(format!("{session_id}.json"));
    if !path.exists() {
        return Ok(None);
    }

    let json = std::fs::read_to_string(&path).context("Failed to read state file")?;

    // Verify HMAC integrity
    let sig_path = state_dir().join(format!("{session_id}.json.sig"));
    if sig_path.exists() {
        let sig = std::fs::read_to_string(&sig_path).context("Failed to read state signature")?;
        if !verify_hmac(json.as_bytes(), sig.trim()) {
            eprintln!(
                "[sentinel] SECURITY: State file integrity check FAILED for session '{}'. \
                 The state file may have been tampered with. Discarding corrupted state.",
                session_id
            );
            // Delete corrupted state + sig
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&sig_path);
            return Ok(None);
        }
    } else {
        // No signature file — state is unsigned.
        // **Attack #45**: Accepting unsigned files as a "migration path" creates
        // a permanent bypass: delete the .sig and inject forged JSON.
        // Now we reject unsigned files — they're treated as corrupted.
        eprintln!(
            "[sentinel] SECURITY: No signature file for session '{}'. \
             Unsigned state files are not trusted. Starting fresh.",
            session_id
        );
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }

    let state: SessionState = serde_json::from_str(&json).context("Failed to parse state")?;
    Ok(Some(state))
}

/// Delete session state (and its signature file)
pub fn delete(session_id: &str) -> Result<()> {
    sanitize_session_id(session_id)?;

    let dir = state_dir();
    let path = dir.join(format!("{session_id}.json"));
    let sig_path = dir.join(format!("{session_id}.json.sig"));
    if path.exists() {
        std::fs::remove_file(&path).context("Failed to delete state file")?;
    }
    let _ = std::fs::remove_file(&sig_path);
    Ok(())
}

/// List all session IDs with saved state
pub fn list_sessions() -> Result<Vec<String>> {
    let dir = state_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(id) = name.strip_suffix(".json") {
            sessions.push(id.to_string());
        }
    }
    Ok(sessions)
}
