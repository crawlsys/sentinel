//! Hygiene Override Hook
//!
//! Runs on UserPromptSubmit. Checks user prompt for override patterns
//! (e.g., "override hygiene", "skip tests"). If matched, writes temporary
//! override files with 60-second expiry so that git-hygiene-gate and
//! verification-gate will allow tool calls through.
//!
//! **Security hardening (Attacks #46, #47, #56)**:
//!   - Override files stored under `~/.claude/sentinel/overrides/` (protected
//!     by the Bash redirect guard and Write/Edit protection)
//!   - Session ID hashed with SHA-256 (128-bit truncation, not 48-bit)
//!   - File content is a signed token: `{timestamp}:{hmac}` where HMAC is
//!     computed over `{type}:{session_id}:{timestamp}` with a secret salt.
//!     A simple `touch` produces an empty/invalid file that fails verification.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use sha2::{Digest, Sha256};

use super::{FileSystemPort, HookContext};

/// Override files directory — under sentinel's protected path
fn override_dir(fs: &dyn FileSystemPort) -> PathBuf {
    fs.home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("sentinel")
        .join("overrides")
}

/// Override file paths — session-scoped with SHA-256 hash of session ID.
///
/// **Attack #46**: Uses SHA-256 (128-bit truncation) instead of DefaultHasher
/// (48-bit truncation). Stored under ~/.claude/sentinel/ instead of temp dir.
/// **Attack #56**: No longer in world-readable /tmp.
pub fn hygiene_override_path(fs: &dyn FileSystemPort, session_id: &str) -> PathBuf {
    let hash = sha256_hash(session_id);
    override_dir(fs).join(format!("hygiene-{hash}"))
}

pub fn verification_override_path(fs: &dyn FileSystemPort, session_id: &str) -> PathBuf {
    let hash = sha256_hash(session_id);
    override_dir(fs).join(format!("verification-{hash}"))
}

pub fn doppler_override_path(fs: &dyn FileSystemPort, session_id: &str) -> PathBuf {
    let hash = sha256_hash(session_id);
    override_dir(fs).join(format!("doppler-{hash}"))
}

/// SHA-256 hash of input, truncated to 32 hex chars (128 bits)
fn sha256_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// Compute HMAC-like signature for override content.
/// Uses SHA-256(salt + type + session_id + timestamp) as a simple MAC.
/// Not a true HMAC (no hmac crate in this crate) but sufficient since
/// the salt is embedded in the binary and not observable.
fn compute_override_sig(override_type: &str, session_id: &str, timestamp: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"sentinel-override-sig-v1:");
    hasher.update(override_type.as_bytes());
    hasher.update(b":");
    hasher.update(session_id.as_bytes());
    hasher.update(b":");
    hasher.update(timestamp.to_string().as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

/// Verify an override file's content is a valid signed token.
/// Format: `{timestamp}:{signature}`
fn verify_override_content(content: &str, override_type: &str, session_id: &str) -> Option<u64> {
    let parts: Vec<&str> = content.trim().splitn(2, ':').collect();
    if parts.len() != 2 {
        return None;
    }
    let timestamp: u64 = parts[0].parse().ok()?;
    let expected_sig = compute_override_sig(override_type, session_id, timestamp);
    if parts[1] == expected_sig {
        Some(timestamp)
    } else {
        None
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Override expiry reduced from 5 minutes to 60 seconds (Attack #31).
/// Shorter window limits exposure from accidental or social-engineered overrides.
const OVERRIDE_TTL_SECS: u64 = sentinel_domain::constants::OVERRIDE_TTL_SECS;

/// Check if prompt matches hygiene override patterns
fn is_hygiene_override(prompt: &str) -> bool {
    let patterns = [
        r"override\s+(hygiene|git|commit)",
        r"hygiene\s+override",
        r"force\s+continue",
        r"skip\s+hygiene",
    ];
    patterns
        .iter()
        .any(|p| Regex::new(p).map(|re| re.is_match(prompt)).unwrap_or(false))
}

/// Check if prompt matches verification override patterns
fn is_verification_override(prompt: &str) -> bool {
    let patterns = [
        r"override\s+verification",
        r"verification\s+override",
        r"skip\s+verification",
        r"skip\s+tests?",
        r"override\s+test",
    ];
    patterns
        .iter()
        .any(|p| Regex::new(p).map(|re| re.is_match(prompt)).unwrap_or(false))
}

/// Check if prompt matches Doppler override patterns.
/// Requires explicit high-friction language because Doppler writes touch secrets.
fn is_doppler_override(prompt: &str) -> bool {
    let patterns = [
        r"override\s+doppler",
        r"doppler\s+override",
        r"allow\s+doppler\s+(write|writes|mutation|mutations)",
        r"authorize\s+doppler\s+(write|writes|mutation|mutations)",
    ];
    patterns
        .iter()
        .any(|p| Regex::new(p).map(|re| re.is_match(prompt)).unwrap_or(false))
}

/// Write a signed override file.
/// Content format: `{timestamp}:{signature}` — a simple `touch` won't produce valid content.
///
/// **Attack #47**: Override files now contain signed tokens. `touch /path` or
/// `echo "" > /path` creates invalid content that fails `verify_override_content()`.
fn write_signed_override(
    fs: &dyn FileSystemPort,
    path: &PathBuf,
    override_type: &str,
    session_id: &str,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs.create_dir_all(parent)?;
    }
    let ts = now_secs();
    let sig = compute_override_sig(override_type, session_id, ts);
    fs.write(path, format!("{ts}:{sig}").as_bytes())
}

/// Check if a signed override file is active (exists, valid signature, not expired).
///
/// **Attack #47**: Replaces the old `is_override_active_at()` which only checked
/// file mtime. Now verifies the content signature, preventing `touch`-based bypass.
pub fn is_signed_override_active(
    fs: &dyn FileSystemPort,
    path: &std::path::Path,
    override_type: &str,
    session_id: &str,
) -> bool {
    let content = match fs.read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    match verify_override_content(&content, override_type, session_id) {
        Some(timestamp) => {
            let now = now_secs();
            if now.saturating_sub(timestamp) < OVERRIDE_TTL_SECS {
                true
            } else {
                // Expired — clean up (write empty)
                let _ = fs.write(path, b"");
                false
            }
        }
        None => {
            // Invalid content (unsigned/tampered) — clean up
            eprintln!(
                "[sentinel] SECURITY: Override file at '{}' has invalid signature. Removing.",
                path.display()
            );
            let _ = fs.write(path, b"");
            false
        }
    }
}

/// Process the hygiene-override hook event.
/// Accepts session_id for session-scoped override files.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let prompt = match &input.prompt {
        Some(p) => p.to_lowercase(),
        None => return HookOutput::allow(),
    };

    let session_id = input.session_id.as_deref().unwrap_or("unknown");

    let hygiene = is_hygiene_override(&prompt);
    let verification = is_verification_override(&prompt);
    let doppler = is_doppler_override(&prompt);

    if hygiene {
        if let Err(e) = write_signed_override(
            ctx.fs,
            &hygiene_override_path(ctx.fs, session_id),
            "hygiene",
            session_id,
        ) {
            eprintln!("Failed to set hygiene override: {e}");
            return HookOutput::allow();
        }
        eprintln!(
            "\
+-------------------------------------------------------------+\n\
|  GIT HYGIENE OVERRIDE ACTIVATED                             |\n\
+-------------------------------------------------------------+\n\
|  Edit/Write tools unblocked for {OVERRIDE_TTL_SECS} seconds.                   |\n\
|                                                             |\n\
|  Remember to commit your changes!                           |\n\
|  The gate will re-engage after timeout or next commit.      |\n\
+-------------------------------------------------------------+"
        );
    }

    if verification {
        if let Err(e) = write_signed_override(
            ctx.fs,
            &verification_override_path(ctx.fs, session_id),
            "verification",
            session_id,
        ) {
            eprintln!("Failed to set verification override: {e}");
            return HookOutput::allow();
        }
        eprintln!(
            "\
+-------------------------------------------------------------+\n\
|  VERIFICATION OVERRIDE ACTIVATED                            |\n\
+-------------------------------------------------------------+\n\
|  git commit/push unblocked for {OVERRIDE_TTL_SECS} seconds.                    |\n\
|                                                             |\n\
|  Run tests before your next commit!                         |\n\
|  The gate will re-engage after timeout.                     |\n\
+-------------------------------------------------------------+"
        );
    }

    if doppler {
        if let Err(e) = write_signed_override(
            ctx.fs,
            &doppler_override_path(ctx.fs, session_id),
            "doppler",
            session_id,
        ) {
            eprintln!("Failed to set doppler override: {e}");
            return HookOutput::allow();
        }
        eprintln!(
            "\
+-------------------------------------------------------------+\n\
|  DOPPLER OVERRIDE ACTIVATED                                 |\n\
+-------------------------------------------------------------+\n\
|  Doppler mutation tools unblocked for {OVERRIDE_TTL_SECS} seconds.              |\n\
|                                                             |\n\
|  Secrets ops are high-risk — verify target config!          |\n\
|  The gate will re-engage after timeout.                     |\n\
+-------------------------------------------------------------+"
        );
    }

    HookOutput::allow()
}

/// Test helper: write a signed override file at the given path.
/// Only available for tests in sibling modules.
#[doc(hidden)]
pub fn write_signed_override_for_test(
    fs: &dyn FileSystemPort,
    path: &std::path::Path,
    override_type: &str,
    session_id: &str,
) {
    if let Some(parent) = path.parent() {
        let _ = fs.create_dir_all(parent);
    }
    let ts = now_secs();
    let sig = compute_override_sig(override_type, session_id, ts);
    let _ = fs.write(path, format!("{ts}:{sig}").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hygiene_override_patterns() {
        assert!(is_hygiene_override("override hygiene"));
        assert!(is_hygiene_override("override git"));
        assert!(is_hygiene_override("override commit"));
        assert!(is_hygiene_override("hygiene override"));
        assert!(is_hygiene_override("force continue"));
        assert!(is_hygiene_override("skip hygiene"));
    }

    #[test]
    fn test_hygiene_override_no_match() {
        assert!(!is_hygiene_override("hello world"));
        assert!(!is_hygiene_override("commit my changes"));
        assert!(!is_hygiene_override("what is hygiene"));
    }

    #[test]
    fn test_verification_override_patterns() {
        assert!(is_verification_override("override verification"));
        assert!(is_verification_override("verification override"));
        assert!(is_verification_override("skip verification"));
        assert!(is_verification_override("skip tests"));
        assert!(is_verification_override("skip test"));
        assert!(is_verification_override("override test"));
    }

    #[test]
    fn test_verification_override_no_match() {
        assert!(!is_verification_override("run the tests"));
        assert!(!is_verification_override("test everything"));
        assert!(!is_verification_override("verify my work"));
    }

    #[test]
    fn test_process_no_prompt() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_process_normal_prompt() {
        let input = HookInput {
            prompt: Some("just a normal message".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_process_hygiene_override() {
        let session = "test-sess-hygiene";
        let input = HookInput {
            prompt: Some("override hygiene".to_string()),
            session_id: Some(session.to_string()),
            ..Default::default()
        };
        // StubFs.write is a no-op, so this just verifies no panic
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_process_verification_override() {
        let session = "test-sess-verification";
        let input = HookInput {
            prompt: Some("skip tests".to_string()),
            session_id: Some(session.to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_case_insensitive() {
        let session = "test-sess-case";
        let input = HookInput {
            prompt: Some("Override Hygiene".to_string()),
            session_id: Some(session.to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
