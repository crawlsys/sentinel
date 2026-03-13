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
fn sanitize_session_id(session_id: &str) -> Result<()> {
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
fn state_dir() -> PathBuf {
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

/// Path to the random secret file used for HMAC key derivation.
/// Generated once on first use, never regenerated.
///
/// **Attack #86 fix**: Panic instead of falling back to `"."` when HOME is unset.
/// The `"."` fallback reads/writes the HMAC secret from CWD — an attacker plants
/// a known secret file in the project dir, sentinel uses it, and all state file
/// HMACs become forgeable with a known key.
fn hmac_secret_path() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory. HOME/USERPROFILE must be set.")
        .join(".claude")
        .join("sentinel")
        .join(".hmac-secret")
}

/// Get or create the random secret for HMAC key derivation.
///
/// **Attack #44**: The original key was derived entirely from observable
/// system properties (`COMPUTERNAME:USERNAME:salt:exe_path`). An attacker
/// could reproduce it via Bash and forge valid HMACs. Adding a random
/// secret file makes the key non-derivable from system observations.
/// The secret is readable only by the owning user (mode 0600 on Unix).
fn get_or_create_secret() -> Vec<u8> {
    let path = hmac_secret_path();

    // Try to read existing secret
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() >= 32 {
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

    // On Unix, restrict permissions to owner-only
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    secret
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

/// Compute HMAC-SHA256 of the given data
fn compute_hmac(data: &[u8]) -> String {
    let key = derive_hmac_key();
    let mut mac = HmacSha256::new_from_slice(&key)
        .expect("HMAC accepts any key size");
    mac.update(data);
    let result = mac.finalize();
    to_hex(&result.into_bytes())
}

/// Verify HMAC-SHA256 of the given data against expected signature
fn verify_hmac(data: &[u8], expected_hex: &str) -> bool {
    let key = derive_hmac_key();
    let mut mac = HmacSha256::new_from_slice(&key)
        .expect("HMAC accepts any key size");
    mac.update(data);
    let expected = match from_hex(expected_hex) {
        Some(v) => v,
        None => return false,
    };
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
    let lock_path = dir.join(format!("{}.json.lock", state.session_id));
    let tmp_path = dir.join(format!("{}.json.tmp", state.session_id));

    // Acquire exclusive lock
    let lock_file =
        std::fs::File::create(&lock_path).context("Failed to create lock file")?;
    lock_file
        .lock_exclusive()
        .context("Failed to acquire state lock")?;

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

    // Lock released on drop
    drop(lock_file);
    let _ = std::fs::remove_file(&lock_path);

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
    lock_file.lock_exclusive().context("Failed to acquire session lock")?;
    Ok(lock_file)
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
        let sig = std::fs::read_to_string(&sig_path)
            .context("Failed to read state signature")?;
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
