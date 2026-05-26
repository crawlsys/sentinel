//! State Store
//!
//! Persists session state to disk. Uses a single JSON file per session
//! instead of the 13+ temp files the Node.js hooks use.
//!
//! Uses file locking to prevent race conditions from concurrent writes.
//!
//! **Encryption at rest**: State files are encrypted with ChaCha20-Poly1305
//! authenticated encryption. The AEAD tag provides both confidentiality AND
//! integrity, replacing the separate HMAC `.sig` files used in earlier versions.
//! An attacker with file read access cannot observe workflow progress, session
//! IDs, or timing data without the encryption key.
//!
//! **Backward compatibility**: On load, the format is auto-detected by checking
//! the first byte: `0x01` = encrypted (v1), `{` = legacy plaintext JSON with
//! HMAC `.sig` file. Legacy files are still verified via HMAC but new saves
//! always use encrypted format.
//!
//! **Key derivation**: The encryption key is derived from the same HMAC key
//! material (machine hostname + username + random secret file) via a second
//! SHA-256 pass with domain separation (`sentinel-encryption-v1`). This ensures
//! the encryption key is independent of the HMAC signing key.
//!
//! **Legacy integrity protection (Attack #42)**: Retained for backward compat.
//! Each legacy state file is paired with an HMAC-SHA256 signature file
//! (`.json.sig`). The HMAC key is derived from the machine hostname + username
//! + a sentinel-specific salt.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{AeadCore, ChaCha20Poly1305, KeyInit};
use fs2::FileExt;
use hmac::{Hmac, Mac};
use sha2::{Digest as _, Sha256};

use sentinel_domain::state::SessionState;

type HmacSha256 = Hmac<Sha256>;

/// Validate that a session ID is safe for use in file paths.
///
/// Rejects any `session_id` containing path traversal characters (`/`, `\`, `..`)
/// or non-ASCII characters. Only allows: alphanumeric, hyphen, underscore, dot.
/// Maximum length: 128 characters.
///
/// **Attack #43**: Without this, `session_id = "../../.bashrc"` writes outside
/// the state directory via path traversal.
/// Validate session ID for safe filesystem use.
///
/// Delegates to `sentinel_domain::SessionId::validate` — the validation rules
/// live in the domain layer (`crates/sentinel-domain/src/session.rs`); this
/// wrapper preserves the legacy `anyhow::Result<()>` return type so existing
/// call sites don't need to change.
///
/// **Attack #121 fix**: Without this, `session_id = "../../etc/passwd"` writes
/// state files outside the intended directory via path traversal.
/// **Attack #146 note**: Public so the daemon API can reuse it for request
/// validation.
pub fn sanitize_session_id(session_id: &str) -> Result<()> {
    sentinel_domain::SessionId::validate(session_id).map_err(|e| anyhow::anyhow!("{e}"))
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
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
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
    discover_key_versions().last().map_or(1, |(v, _)| *v)
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
/// On Windows, uses `icacls` to remove inherited permissions and grant only the current user,
/// with error checking and post-verification of the resulting ACL.
/// On Unix, uses chmod 600.
fn restrict_secret_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }

    #[cfg(windows)]
    {
        let path_str = path.to_string_lossy().to_string();
        let username = std::env::var("USERNAME").unwrap_or_default();
        if username.is_empty() {
            eprintln!(
                "[sentinel] WARNING: USERNAME not set — cannot restrict file permissions on {}",
                path.display()
            );
            return;
        }

        if let Err(e) = apply_windows_owner_only_acl(&path_str, &username) {
            eprintln!(
                "[sentinel] WARNING: Failed to restrict file permissions on {}: {}",
                path.display(),
                e
            );
        }

        // Verify — read ACL and check only our user appears
        verify_acl_owner_only(path, &username, true);
    }
}

/// Verify that a file's ACL contains only the expected owner.
/// Logs a warning if unexpected ACEs (other users/groups) are found.
#[cfg(windows)]
fn verify_acl_owner_only(path: &std::path::Path, username: &str, log_warning: bool) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let path_str = path.to_string_lossy().to_string();

    let verify = std::process::Command::new("icacls")
        .args([&path_str])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    match verify {
        Ok(output) if output.status.success() => {
            let acl_output = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<&str> = acl_output
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.contains("Successfully processed"))
                .collect();
            // First line is the file path; remaining lines are ACE entries.
            // Each ACE line should reference only the current user.
            let unexpected_aces: Vec<&&str> = lines
                .iter()
                .skip(1) // skip the path line
                .filter(|l| !l.to_lowercase().contains(&username.to_lowercase()))
                .collect();
            if !unexpected_aces.is_empty() {
                if log_warning {
                    eprintln!(
                        "[sentinel] WARNING: Unexpected ACEs on {}: {:?}. File may be readable by other users.",
                        path.display(),
                        unexpected_aces
                    );
                }
                return false;
            }
            true
        }
        _ => {
            if log_warning {
                eprintln!(
                    "[sentinel] WARNING: Could not verify ACL on {}",
                    path.display()
                );
            }
            false
        }
    }
}

#[cfg(windows)]
fn apply_windows_owner_only_acl(path: &str, username: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let principal = std::env::var("USERDOMAIN").map_or_else(
        |_| username.to_string(),
        |domain| {
            if domain.is_empty() {
                username.to_string()
            } else {
                format!(r"{domain}\{username}")
            }
        },
    );

    let escaped_path = path.replace('\'', "''");
    let escaped_principal = principal.replace('\'', "''");
    let script = format!(
        "$ErrorActionPreference='Stop'; \
         $path='{escaped_path}'; \
         $principal='{escaped_principal}'; \
         $acl=Get-Acl -LiteralPath $path; \
         $acl.SetAccessRuleProtection($true, $false); \
         foreach ($rule in @($acl.Access)) {{ [void]$acl.RemoveAccessRuleAll($rule) }}; \
         $newRule=New-Object System.Security.AccessControl.FileSystemAccessRule($principal, 'FullControl', 'Allow'); \
         $acl.AddAccessRule($newRule); \
         Set-Acl -LiteralPath $path -AclObject $acl"
    );

    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .with_context(|| format!("Failed to run PowerShell ACL repair on {path}"))?;

    if output.status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "{}{}",
            String::from_utf8_lossy(&output.stderr).trim(),
            if output.stdout.is_empty() {
                String::new()
            } else {
                format!(" {}", String::from_utf8_lossy(&output.stdout).trim())
            }
        );
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

    // On Windows, verify ACL by running icacls in read-only mode.
    // Checks that only the current user has access to the secret file.
    #[cfg(windows)]
    {
        let username = std::env::var("USERNAME").unwrap_or_default();
        if username.is_empty() {
            eprintln!(
                "[sentinel] WARNING: USERNAME not set — cannot validate permissions on {}",
                path.display()
            );
            return;
        }
        if !verify_acl_owner_only(path, &username, false) {
            restrict_secret_permissions(path);
        }
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
    let mut hasher = Sha256::new();
    hasher.update(key_material.as_bytes());
    hasher.finalize().to_vec()
}

/// Expose HMAC key derivation for `proof_store` to reuse the same signing key.
/// **Attack #123**: Proof files need the same integrity protection as state files.
pub fn derive_hmac_key_for_proofs() -> Vec<u8> {
    derive_hmac_key()
}

/// Compute versioned HMAC for `proof_store` (delegates to internal `compute_hmac`).
pub fn compute_hmac_for_proofs(data: &[u8]) -> String {
    compute_hmac(data)
}

/// Verify versioned HMAC for `proof_store` (delegates to internal `verify_hmac`).
pub fn verify_hmac_for_proofs(data: &[u8], sig_str: &str) -> bool {
    verify_hmac(data, sig_str)
}

/// Derive a 256-bit encryption key from the HMAC key material.
/// Uses a second SHA-256 pass with a domain-separation salt so the
/// encryption key is independent of the HMAC signing key, even though
/// both derive from the same root secret.
fn derive_encryption_key() -> [u8; 32] {
    use sha2::Digest;
    let hmac_key = derive_hmac_key();
    let mut hasher = Sha256::new();
    hasher.update(&hmac_key);
    hasher.update(b"sentinel-encryption-v1"); // domain separation
    let result = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&result);
    key
}

/// Encrypt plaintext using ChaCha20-Poly1305 authenticated encryption.
///
/// Output format: `version_byte (0x01) || nonce (12 bytes) || ciphertext+tag`.
/// The AEAD tag provides both integrity AND confidentiality, replacing the
/// separate HMAC `.sig` file for encrypted state files.
fn encrypt_state(plaintext: &[u8]) -> Result<Vec<u8>> {
    let key_bytes = derive_encryption_key();
    let key = chacha20poly1305::Key::from_slice(&key_bytes);
    let cipher = ChaCha20Poly1305::new(key);
    let nonce = ChaCha20Poly1305::generate_nonce(&mut chacha20poly1305::aead::OsRng);

    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("Encryption failed: {e}"))?;

    // Format: version_byte || nonce (12 bytes) || ciphertext
    let mut output = Vec::with_capacity(1 + 12 + ciphertext.len());
    output.push(0x01); // version 1
    output.extend_from_slice(&nonce);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

/// Decrypt state data encrypted by `encrypt_state`.
///
/// Validates the version byte, extracts the nonce, and uses ChaCha20-Poly1305
/// to decrypt + authenticate. Any tampering (bit flip, truncation, nonce reuse
/// with different ciphertext) causes the AEAD tag check to fail.
fn decrypt_state(data: &[u8]) -> Result<Vec<u8>> {
    // Minimum size: 1 (version) + 12 (nonce) + 16 (AEAD tag, no plaintext)
    if data.len() < 1 + 12 + 16 {
        anyhow::bail!("Encrypted state too short");
    }

    let version = data[0];
    if version != 0x01 {
        anyhow::bail!("Unknown encryption version: {version}");
    }

    let nonce = chacha20poly1305::Nonce::from_slice(&data[1..13]);
    let ciphertext = &data[13..];

    let key_bytes = derive_encryption_key();
    let key = chacha20poly1305::Key::from_slice(&key_bytes);
    let cipher = ChaCha20Poly1305::new(key);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("Decryption failed (possible tampering): {e}"))
}

/// Compute HMAC-SHA256 of the given data using the current (latest) key.
/// Returns a versioned signature string: `v{N}:{hex}`.
fn compute_hmac(data: &[u8]) -> String {
    let version = current_key_version();
    let key = derive_hmac_key();
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&key).expect("HMAC accepts any key size");
    Mac::update(&mut mac, data);
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
    let (version, hex) = sig_str.strip_prefix('v').map_or(
        (None, sig_str), // No 'v' prefix, legacy format
        |rest| {
            rest.split_once(':').map_or(
                (None, sig_str), // No colon, treat as legacy
                |(v_str, hex_part)| {
                    v_str.parse::<u32>().map_or(
                        (None, sig_str), // Malformed version, treat as legacy
                        |v| (Some(v), hex_part),
                    )
                },
            )
        },
    );

    let Some(expected) = from_hex(hex) else { return false };

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

    let mut mac: HmacSha256 = match <HmacSha256 as Mac>::new_from_slice(&key) {
        Ok(m) => m,
        Err(_) => return false,
    };
    Mac::update(&mut mac, data);
    mac.verify_slice(&expected).is_ok()
}

/// Path to the generation tracking file for a session.
/// Stores the highest known generation number as a plain u64 string.
/// This file is NEVER overwritten with a lower value — it only ratchets up.
fn generation_file_path(session_id: &str) -> PathBuf {
    state_dir().join(format!("{session_id}.gen"))
}

/// Read the highest known generation from the `.gen` file.
/// Returns 0 if the file doesn't exist or is unreadable.
fn read_generation_floor(session_id: &str) -> u64 {
    let path = generation_file_path(session_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Write the generation floor to the `.gen` file (atomic via tmp+rename).
fn write_generation_floor(session_id: &str, generation: u64) -> Result<()> {
    let dir = state_dir();
    let path = generation_file_path(session_id);
    let tmp_path = dir.join(format!("{session_id}.gen.tmp"));
    std::fs::write(&tmp_path, generation.to_string())
        .context("Failed to write temp generation file")?;
    std::fs::rename(&tmp_path, &path).context("Failed to rename temp generation file")?;
    Ok(())
}

/// Save session state to disk (atomic write with file lock + encryption)
///
/// Uses an exclusive lock file to serialize concurrent writes,
/// preventing corruption from parallel hook invocations.
/// Encrypts state with ChaCha20-Poly1305 AEAD (authenticated encryption).
///
/// **Anti-replay (Attack #81 hardening)**: Also writes the generation to a
/// separate `.gen` file that acts as a monotonic floor. On load, the state's
/// generation must be >= this floor, preventing file deletion/replacement attacks.
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

    // Encrypt the JSON — ChaCha20-Poly1305 AEAD provides both confidentiality
    // and integrity, replacing the separate HMAC `.sig` file.
    let encrypted = encrypt_state(json.as_bytes())?;

    // Write encrypted data (binary, not text) via atomic tmp+rename.
    // No `.sig` file needed — the AEAD tag embedded in the ciphertext
    // provides authenticated integrity.
    std::fs::write(&tmp_path, &encrypted).context("Failed to write temp state file")?;
    std::fs::rename(&tmp_path, &path).context("Failed to rename temp state file")?;

    // Clean up legacy `.sig` file if it exists (from pre-encryption saves).
    let _ = std::fs::remove_file(&sig_path);

    // **Anti-replay**: Write the generation floor AFTER the state file lands.
    // The .gen file only ratchets up — it records the highest generation ever saved.
    write_generation_floor(&state.session_id, state.state_generation)?;

    Ok(())
}

/// Acquire an exclusive session lock for the entire load-process-save cycle.
///
/// **Attack #67 fix**: Without this, concurrent hook invocations race between
/// load and save, causing lost updates (state regression). Hold this lock
/// from before `load()` through `save()` to serialize access.
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
    //
    // **Note on lock enforcement**: fs2's `try_lock_exclusive()` uses:
    //   - Windows: `LockFileEx` with `LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY`
    //     This IS a mandatory lock enforced by the OS kernel. Other processes cannot
    //     read/write the locked region without also calling LockFileEx and waiting.
    //   - Unix: `flock(LOCK_EX | LOCK_NB)` — advisory only. A malicious process
    //     can bypass it by not calling flock. Acceptable for our threat model since
    //     Unix users can chmod the state directory to prevent other-user access.
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

    let raw = std::fs::read(&path).context("Failed to read state file")?;

    let json = if raw.first() == Some(&0x01) {
        // ── Encrypted format (v1) ──────────────────────────────────────────
        // The AEAD tag provides integrity — no separate .sig file needed.
        let decrypted = decrypt_state(&raw).map_err(|e| {
            eprintln!(
                "[sentinel] SECURITY: State decryption failed for session '{session_id}': {e}. \
                 Possible tampering. Discarding."
            );
            let _ = crate::security_log::log_security_event(
                "decrypt_failure",
                session_id,
                &format!("State decryption failed: {e}"),
            );
            // Delete corrupted state
            let _ = std::fs::remove_file(&path);
            e
        })?;
        String::from_utf8(decrypted).context("Decrypted state is not valid UTF-8")?
    } else {
        // ── Legacy plaintext format — verify HMAC ──────────────────────────
        let json = String::from_utf8(raw).context("State file is not valid UTF-8")?;
        let sig_path = state_dir().join(format!("{session_id}.json.sig"));
        if sig_path.exists() {
            let sig =
                std::fs::read_to_string(&sig_path).context("Failed to read state signature")?;
            if !verify_hmac(json.as_bytes(), sig.trim()) {
                eprintln!(
                    "[sentinel] SECURITY: State file integrity check FAILED for session '{session_id}'. \
                     The state file may have been tampered with. Discarding corrupted state."
                );
                let _ = crate::security_log::log_security_event(
                    "hmac_failure",
                    session_id,
                    "State file HMAC verification failed — possible tampering",
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
                "[sentinel] SECURITY: No signature file for session '{session_id}'. \
                 Unsigned state files are not trusted. Starting fresh."
            );
            let _ = crate::security_log::log_security_event(
                "tamper_detected",
                session_id,
                "No signature file for state — unsigned state rejected",
            );
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }
        json
    };

    let state: SessionState = serde_json::from_str(&json).context("Failed to parse state")?;

    // **Anti-replay (Attack #81 hardening)**: Check that the loaded state's
    // generation is >= the highest generation we've ever saved for this session.
    // If it's lower, someone deleted the state file and let sentinel recreate
    // it at a lower generation (state regression / replay attack).
    let gen_floor = read_generation_floor(session_id);
    if state.state_generation < gen_floor {
        eprintln!(
            "[sentinel] SECURITY: State regression detected for session '{}'. \
             Expected generation >= {}, got {}. Possible replay attack.",
            session_id, gen_floor, state.state_generation,
        );
        let _ = crate::security_log::log_security_event(
            "state_regression",
            session_id,
            &format!(
                "Expected generation >= {}, got {}. Possible replay attack.",
                gen_floor, state.state_generation,
            ),
        );
        // Reject the downgraded state — delete the corrupted/replayed file
        let _ = std::fs::remove_file(&path);
        // Also clean up legacy .sig file if present
        let sig_path = state_dir().join(format!("{session_id}.json.sig"));
        let _ = std::fs::remove_file(&sig_path);
        return Ok(None);
    }

    Ok(Some(state))
}

/// Delete session state (and its signature + generation files)
pub fn delete(session_id: &str) -> Result<()> {
    sanitize_session_id(session_id)?;

    let dir = state_dir();
    let path = dir.join(format!("{session_id}.json"));
    let sig_path = dir.join(format!("{session_id}.json.sig"));
    let gen_path = generation_file_path(session_id);
    if path.exists() {
        std::fs::remove_file(&path).context("Failed to delete state file")?;
    }
    let _ = std::fs::remove_file(&sig_path);
    let _ = std::fs::remove_file(&gen_path);
    Ok(())
}

/// Expose `compute_hmac` for tests that need to forge valid-HMAC state files (legacy format).
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn compute_hmac_for_tests(data: &[u8]) -> String {
    compute_hmac(data)
}

/// Expose `encrypt_state` for tests that need to forge encrypted state files.
#[cfg(test)]
pub(crate) fn encrypt_state_for_tests(plaintext: &[u8]) -> Result<Vec<u8>> {
    encrypt_state(plaintext)
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

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::state::SessionState;

    /// Generate a unique test session ID to avoid collisions between parallel tests.
    fn test_session_id(suffix: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("test-antireplay-{n}-{suffix}")
    }

    /// Cleanup helper — removes all files for a session.
    fn cleanup_session(session_id: &str) {
        let _ = delete(session_id);
    }

    #[test]
    fn test_state_generation_prevents_replay() {
        let sid = test_session_id("replay");

        // Ensure clean slate
        cleanup_session(&sid);

        // Step 1: Save state a few times to advance generation to N
        let mut state = SessionState::new(sid.clone());
        save(&mut state).expect("save 1");
        assert_eq!(state.state_generation, 1);
        save(&mut state).expect("save 2");
        assert_eq!(state.state_generation, 2);
        save(&mut state).expect("save 3");
        assert_eq!(state.state_generation, 3);

        // Step 2: Verify normal load succeeds (generation 3 >= floor 3)
        let loaded = load(&sid).expect("load should succeed");
        assert!(loaded.is_some(), "normal load should return Some");
        assert_eq!(loaded.unwrap().state_generation, 3);

        // Step 3: Forge an encrypted state file with generation 1 (regression)
        let mut forged_state = SessionState::new(sid.clone());
        forged_state.state_generation = 1; // lower than floor of 3

        let forged_json = serde_json::to_string_pretty(&forged_state).unwrap();
        let forged_encrypted = encrypt_state_for_tests(forged_json.as_bytes()).unwrap();

        let dir = state_dir();
        let state_path = dir.join(format!("{sid}.json"));
        std::fs::write(&state_path, &forged_encrypted).unwrap();

        // Step 4: Load should reject — generation 1 < floor 3
        let loaded = load(&sid).expect("load should not error");
        assert!(
            loaded.is_none(),
            "load should return None for replayed state with lower generation"
        );

        // Step 5: The corrupted state file should have been deleted
        assert!(
            !state_path.exists(),
            "replayed state file should be deleted"
        );

        // Cleanup
        cleanup_session(&sid);
    }

    #[test]
    fn test_generation_floor_ratchets_up() {
        let sid = test_session_id("ratchet");
        cleanup_session(&sid);

        let mut state = SessionState::new(sid.clone());
        save(&mut state).expect("save 1");
        assert_eq!(read_generation_floor(&sid), 1);

        save(&mut state).expect("save 2");
        assert_eq!(read_generation_floor(&sid), 2);

        save(&mut state).expect("save 3");
        assert_eq!(read_generation_floor(&sid), 3);

        cleanup_session(&sid);
    }

    #[test]
    fn test_delete_removes_gen_file() {
        let sid = test_session_id("delgen");
        cleanup_session(&sid);

        let mut state = SessionState::new(sid.clone());
        save(&mut state).expect("save");

        let gen_path = generation_file_path(&sid);
        assert!(gen_path.exists(), ".gen file should exist after save");

        delete(&sid).expect("delete");
        assert!(
            !gen_path.exists(),
            ".gen file should be removed after delete"
        );
    }

    #[test]
    fn test_normal_save_load_cycle_with_generation() {
        let sid = test_session_id("normal");
        cleanup_session(&sid);

        let mut state = SessionState::new(sid.clone());
        state.set_active_skill("linear");
        save(&mut state).expect("save");

        let loaded = load(&sid).expect("load").expect("should be Some");
        assert_eq!(loaded.state_generation, 1);
        assert_eq!(loaded.active_skill.as_deref(), Some("linear"));

        cleanup_session(&sid);
    }

    /// Regression: `save()` must not deadlock when session lock is already held.
    /// The bug: `save()` acquired its own exclusive lock on the same file that
    /// `acquire_session_lock()` already held. On Windows (per-handle, non-reentrant
    /// locks), this deadlocked every `UserPromptSubmit` invocation.
    #[test]
    fn test_save_does_not_deadlock_under_session_lock() {
        let sid = test_session_id("nodeadlock");
        cleanup_session(&sid);

        let _lock = acquire_session_lock(&sid).expect("should acquire lock");

        let mut state = SessionState::new(sid.clone());
        let start = std::time::Instant::now();
        save(&mut state).expect("save should succeed while session lock is held");
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < 1000,
            "save() took {}ms — possible deadlock",
            elapsed.as_millis()
        );

        let loaded = load(&sid).expect("load").expect("should be Some");
        assert_eq!(loaded.state_generation, 1);

        cleanup_session(&sid);
    }
}
