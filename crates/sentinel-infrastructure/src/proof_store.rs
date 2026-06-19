//! Proof Store
//!
//! Persists proof chains to JSONL files for audit trails.
//! Each session gets its own proof file.
//!
//! **Attack #123 fix**: Proof chain files are now HMAC-signed (`.sig` companion
//! files), mirroring the `state_store` integrity pattern. Without this, an attacker
//! could inject forged proofs into JSONL files to fake phase completion evidence.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};

use sentinel_domain::proof::{PhaseProof, ProofChain};

/// Reuse the same versioned HMAC signing as `state_store`.
/// Returns `v{N}:{hex}` format for consistency with state file signatures.
fn compute_hmac(data: &[u8]) -> String {
    // Delegate to state_store's versioned compute_hmac
    crate::state_store::compute_hmac_for_proofs(data)
}

/// Verify HMAC of data against expected signature string.
/// Supports versioned signatures (`v{N}:{hex}`).
fn verify_hmac(data: &[u8], sig_str: &str) -> bool {
    crate::state_store::verify_hmac_for_proofs(data, sig_str)
}

/// Public accessor for proof directory path (used by `resign_cmd`).
pub fn proof_dir_public() -> PathBuf {
    proof_dir()
}

/// Proof storage directory
///
/// **Attack #122 fix**: Panic instead of writing to `"."` when HOME is unset.
/// Writing proof files to CWD would put them in an attacker-controlled project
/// directory, letting an attacker plant forged proof files that sentinel will
/// load as real.
fn proof_dir() -> PathBuf {
    crate::paths::home_root_or_fatal()
        .join(".claude")
        .join("sentinel")
        .join("proofs")
}

/// Validate session ID for safe filesystem use.
///
/// Delegates to `sentinel_domain::SessionId::validate` — the validation rules
/// live in the domain layer; this wrapper maps the domain validation error into
/// the infrastructure layer's `anyhow::Result<()>`.
///
/// **Attack #121 fix**: Without this, `session_id = "../../etc/passwd"` writes
/// proof files outside the intended directory via path traversal.
fn sanitize_session_id(session_id: &str) -> Result<()> {
    sentinel_domain::SessionId::validate(session_id).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Append a proof to the session's proof file (JSONL format)
pub fn append_proof(session_id: &str, proof: &PhaseProof) -> Result<()> {
    sanitize_session_id(session_id)?;
    let dir = proof_dir();
    std::fs::create_dir_all(&dir)?;

    let path = dir.join(format!("{session_id}.jsonl"));
    let line = serde_json::to_string(proof)? + "\n";

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())?;

    Ok(())
}

/// Save a complete proof chain with HMAC signature
pub fn save_chain(chain: &ProofChain) -> Result<()> {
    sanitize_session_id(&chain.session_id)?;
    let dir = proof_dir();
    std::fs::create_dir_all(&dir)?;

    let path = dir.join(format!("{}-chain.json", chain.session_id));
    let sig_path = dir.join(format!("{}-chain.json.sig", chain.session_id));
    let json = serde_json::to_string_pretty(chain)?;

    // Write sig first (same pattern as state_store — sig before data)
    let sig = compute_hmac(json.as_bytes());
    std::fs::write(&sig_path, &sig)?;
    std::fs::write(&path, json)?;

    Ok(())
}

/// Load a proof chain with HMAC verification
pub fn load_chain(session_id: &str) -> Result<Option<ProofChain>> {
    sanitize_session_id(session_id)?;
    let path = proof_dir().join(format!("{session_id}-chain.json"));
    if !path.exists() {
        return Ok(None);
    }

    let json = std::fs::read_to_string(&path)?;

    // Verify HMAC signature — reject unsigned or tampered chains.
    // **Attack #147 fix**: Unsigned chains are rejected.
    // An attacker could write a forged chain JSON without a .sig file and it would
    // be silently accepted. Now all chains MUST have valid HMAC signatures.
    let sig_path = proof_dir().join(format!("{session_id}-chain.json.sig"));
    if !sig_path.exists() {
        eprintln!(
            "[sentinel] SECURITY: Proof chain for session '{session_id}' has no signature file. \
             Rejecting unsigned chain (may be forged)."
        );
        return Ok(None);
    }
    let sig = std::fs::read_to_string(&sig_path).context("Failed to read proof chain signature")?;
    if !verify_hmac(json.as_bytes(), sig.trim()) {
        eprintln!(
            "[sentinel] SECURITY: Proof chain integrity check FAILED for session '{session_id}'. \
             The proof chain may have been tampered with. Discarding."
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&sig_path);
        return Ok(None);
    }

    let chain: ProofChain = serde_json::from_str(&json)?;
    Ok(Some(chain))
}

/// Load individual proofs from JSONL
pub fn load_proofs(session_id: &str) -> Result<Vec<PhaseProof>> {
    sanitize_session_id(session_id)?;
    let path = proof_dir().join(format!("{session_id}.jsonl"));
    if !path.exists() {
        return Ok(vec![]);
    }

    let content = std::fs::read_to_string(&path)?;
    let mut proofs = Vec::new();
    for line in content.lines() {
        if !line.is_empty() {
            let proof: PhaseProof = serde_json::from_str(line).context(format!(
                "Failed to parse proof line: {}",
                &line[..line.len().min(80)]
            ))?;
            proofs.push(proof);
        }
    }
    Ok(proofs)
}

/// List all sessions with proof chains
pub fn list_sessions() -> Result<Vec<String>> {
    let dir = proof_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(id) = name.strip_suffix("-chain.json") {
            sessions.push(id.to_string());
        }
    }
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_lock() -> MutexGuard<'static, ()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    struct EnvGuard {
        original: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_sentinel_home(value: impl AsRef<std::ffi::OsStr>) -> Self {
            let original = std::env::var_os("SENTINEL_HOME");
            std::env::set_var("SENTINEL_HOME", value);
            Self { original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => std::env::set_var("SENTINEL_HOME", value),
                None => std::env::remove_var("SENTINEL_HOME"),
            }
        }
    }

    #[test]
    fn proof_dir_uses_sentinel_home_root() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = EnvGuard::set_sentinel_home(tmp.path());

        assert_eq!(
            proof_dir(),
            tmp.path().join(".claude").join("sentinel").join("proofs")
        );
    }
}
