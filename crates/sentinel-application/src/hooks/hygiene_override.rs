//! Hygiene Override Hook
//!
//! Runs on `UserPromptSubmit`. Checks user prompt for override patterns
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
//!
//! **Signature scheme (v2)**:
//!   HMAC-SHA256 keyed on `b"sentinel-override-sig-v2"`.  Message is
//!   `"{type}:{session_id}:{timestamp}"`.  Verification uses
//!   `hmac::Mac::verify_slice` which is constant-time, eliminating the
//!   timing-oracle present in the previous string-equality check.
//!   Tokens produced by the old SHA-256(salt||msg) scheme will fail
//!   verification (fail-closed); since TTL is ≤ 3600 s any in-flight
//!   tokens expire naturally within an hour.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sentinel_domain::events::{HookInput, HookOutput};
use sha2::{Digest, Sha256};

use super::{FileSystemPort, HookContext};

/// HMAC key for override token signing (v2).
///
/// Keying material is embedded in the binary.  The key is intentionally
/// short and static — the security goal is *tamper-detection* (an attacker
/// who can write arbitrary files cannot forge a valid token without knowing
/// the key), not cryptographic secrecy of the session ID.
const HMAC_KEY: &[u8] = b"sentinel-override-sig-v2";

/// Override files directory — under sentinel's protected path
fn override_dir(fs: &dyn FileSystemPort) -> PathBuf {
    fs.claude_dir().join("sentinel").join("overrides")
}

/// Override file paths — session-scoped with SHA-256 hash of session ID.
///
/// **Attack #46**: Uses SHA-256 (128-bit truncation) instead of `DefaultHasher`
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

/// Phase-gate override path. Suppresses the `phase_gate.rs` block on
/// `~/.claude/skills/**/SKILL.md` and `~/.claude/skills/**/phases/**`
/// edits. Lets the user perform marketplace-wide skill refactors. Does
/// NOT suspend protection on `~/.claude/sentinel/`, settings.json, or
/// hooks.toml — those remain blocked always.
pub fn phase_gate_override_path(fs: &dyn FileSystemPort, session_id: &str) -> PathBuf {
    let hash = sha256_hash(session_id);
    override_dir(fs).join(format!("phase-gate-{hash}"))
}

/// SHA-256 hash of input, truncated to 32 hex chars (128 bits)
fn sha256_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.iter().take(16).fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Compute an HMAC-SHA256 signature for an override token.
///
/// The HMAC is keyed on [`HMAC_KEY`].  The message is the concatenation
/// `"{override_type}:{session_id}:{timestamp}"`.  The result is encoded as
/// 64 lowercase hex characters (full 256-bit MAC — no truncation).
///
/// On-disk token format is unchanged: `{timestamp}:{sig}`.
fn compute_override_sig(override_type: &str, session_id: &str, timestamp: u64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(HMAC_KEY).expect("HMAC accepts any key length");
    mac.update(override_type.as_bytes());
    mac.update(b":");
    mac.update(session_id.as_bytes());
    mac.update(b":");
    mac.update(timestamp.to_string().as_bytes());
    let result = mac.finalize().into_bytes();
    result.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Verify an override file's content is a valid signed token.
///
/// Format: `{timestamp}:{signature}` where `{signature}` is 64 hex chars.
///
/// Verification re-computes the HMAC and compares in **constant time** via
/// [`hmac::Mac::verify_slice`], which prevents timing-oracle attacks that
/// would have been possible with the previous `parts[1] == expected_sig`
/// string equality.
fn verify_override_content(content: &str, override_type: &str, session_id: &str) -> Option<u64> {
    let parts: Vec<&str> = content.trim().splitn(2, ':').collect();
    if parts.len() != 2 {
        return None;
    }
    let timestamp: u64 = parts[0].parse().ok()?;
    // Decode the on-disk hex signature into raw bytes for constant-time comparison.
    let sig_bytes = hex::decode(parts[1]).ok()?;
    // Re-derive the expected MAC and compare in constant time.
    let mut mac = Hmac::<Sha256>::new_from_slice(HMAC_KEY).expect("HMAC accepts any key length");
    mac.update(override_type.as_bytes());
    mac.update(b":");
    mac.update(session_id.as_bytes());
    mac.update(b":");
    mac.update(timestamp.to_string().as_bytes());
    // `verify_slice` returns Err on mismatch; the comparison is constant-time.
    mac.verify_slice(&sig_bytes).ok()?;
    Some(timestamp)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Override expiry reduced from 5 minutes to 60 seconds (Attack #31).
/// Shorter window limits exposure from accidental or social-engineered overrides.
const OVERRIDE_TTL_SECS: u64 = sentinel_domain::constants::OVERRIDE_TTL_SECS;

/// Phase-gate override TTL — 1 hour (3600s).
///
/// Phase-gate overrides are explicitly invoked for marketplace-wide skill
/// refactors which involve many sequential file edits AND spawned subagents
/// that don't naturally share the parent's `session_id`. The original 600s
/// ceiling was too tight: a single skill standardization sweep (Sprint 2–7
/// of the 75-skill marketplace audit) runs 30–60 min continuous. Multiple
/// re-triggers per sweep created a poor operator experience and lost work
/// when subagents hit expired tokens mid-write.
///
/// Phase-gate overrides are strictly scoped (only SKILL.md + phase files;
/// sentinel config/state/settings remain protected), so the longer window
/// is a smaller blast radius than the hygiene override and the trade-off
/// favors completion of legitimate authoring work.
pub const PHASE_GATE_OVERRIDE_TTL_SECS: u64 = 3600;

// Override-phrase predicates have moved to `sentinel_domain::override_phrase`
// where the patterns are reviewable + tested in isolation. The hook is still
// responsible for what to *do* once a phrase matches (write a signed token,
// reset cooldown).
use sentinel_domain::override_phrase::{
    is_doppler_override, is_hygiene_override, is_phase_gate_override, is_verification_override,
};

fn concrete_session_id(input: &HookInput) -> Option<&str> {
    input
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty() && *session_id != "unknown")
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
    fs.write(path, format!("{ts}:{sig}").as_bytes())?;
    Ok(())
}

/// Check if a signed override file is active (exists, valid signature, not expired).
/// Uses the default `OVERRIDE_TTL_SECS` (60s).
///
/// **Attack #47**: Replaces the old `is_override_active_at()` which only checked
/// file mtime. Now verifies the content signature, preventing `touch`-based bypass.
pub fn is_signed_override_active(
    fs: &dyn FileSystemPort,
    path: &std::path::Path,
    override_type: &str,
    session_id: &str,
) -> bool {
    is_signed_override_active_with_ttl(fs, path, override_type, session_id, OVERRIDE_TTL_SECS)
}

/// Like `is_signed_override_active` but with a caller-specified TTL.
/// Used by `phase_gate` (which needs 600s instead of 60s) — see the
/// `PHASE_GATE_OVERRIDE_TTL_SECS` rationale.
pub fn is_signed_override_active_with_ttl(
    fs: &dyn FileSystemPort,
    path: &std::path::Path,
    override_type: &str,
    session_id: &str,
    ttl_secs: u64,
) -> bool {
    let content = match fs.read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(timestamp) = verify_override_content(&content, override_type, session_id) {
        let now = now_secs();
        if now.saturating_sub(timestamp) < ttl_secs {
            true
        } else {
            // Expired — clean up (write empty)
            let _ = fs.write(path, b"");
            false
        }
    } else {
        // Invalid content (unsigned/tampered) — clean up
        eprintln!(
            "[sentinel] SECURITY: Override file at '{}' has invalid signature. Removing.",
            path.display()
        );
        let _ = fs.write(path, b"");
        false
    }
}

/// Process the hygiene-override hook event.
/// Accepts `session_id` for session-scoped override files.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let prompt = match &input.prompt {
        Some(p) => p.to_lowercase(),
        None => return HookOutput::allow(),
    };

    let hygiene = is_hygiene_override(&prompt);
    let verification = is_verification_override(&prompt);
    let doppler = is_doppler_override(&prompt);
    let phase_gate = is_phase_gate_override(&prompt);

    let override_requested = hygiene || verification || doppler || phase_gate;
    let Some(session_id) = concrete_session_id(input) else {
        if override_requested {
            eprintln!(
                "[sentinel] override request ignored: concrete session_id is required for signed override authority"
            );
        }
        return HookOutput::allow();
    };

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
        let path = doppler_override_path(ctx.fs, session_id);
        eprintln!(
            "[sentinel][doppler_override_write] session={}, path={}",
            session_id,
            path.display()
        );
        if let Err(e) = write_signed_override(ctx.fs, &path, "doppler", session_id) {
            eprintln!("Failed to set doppler override: {e}");
            return HookOutput::allow();
        }
        eprintln!("[sentinel][doppler_override_write] wrote successfully");
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

    if phase_gate {
        let path = phase_gate_override_path(ctx.fs, session_id);
        if let Err(e) = write_signed_override(ctx.fs, &path, "phase-gate", session_id) {
            eprintln!("Failed to set phase-gate override: {e}");
            return HookOutput::allow();
        }
        eprintln!(
            "\
+-------------------------------------------------------------+\n\
|  PHASE GATE OVERRIDE ACTIVATED                              |\n\
+-------------------------------------------------------------+\n\
|  Skill/phase file edits unblocked for {OVERRIDE_TTL_SECS} seconds.              |\n\
|                                                             |\n\
|  Refactor-scope only: SKILL.md + phases/*.md.               |\n\
|  Sentinel config/state + settings.json remain protected.    |\n\
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
    use std::path::{Path, PathBuf};

    struct ScopedHomeFs {
        home: PathBuf,
    }

    impl FileSystemPort for ScopedHomeFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }

        fn read_to_string(
            &self,
            p: &Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_to_string(p)?)
        }

        fn write(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Ok(std::fs::write(p, c)?)
        }

        fn create_dir_all(
            &self,
            p: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::create_dir_all(p)?)
        }

        fn read_dir(
            &self,
            p: &Path,
        ) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_dir(p)?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect())
        }

        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }

        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }

        fn metadata(
            &self,
            p: &Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::metadata(p)?)
        }

        fn append(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?;
            file.write_all(c)?;
            Ok(())
        }
    }

    fn scoped_ctx(fs: &'static dyn FileSystemPort) -> HookContext<'static> {
        let base = crate::hooks::test_support::stub_ctx();
        HookContext { fs, ..base }
    }

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
    fn test_process_override_requires_concrete_session_id() {
        let tmpdir = tempfile::tempdir().unwrap();
        let fs: &'static ScopedHomeFs = Box::leak(Box::new(ScopedHomeFs {
            home: tmpdir.path().to_path_buf(),
        }));
        let ctx = scoped_ctx(fs);
        let unknown_path = verification_override_path(fs, "unknown");

        let input = HookInput {
            prompt: Some("skip tests".to_string()),
            session_id: None,
            ..Default::default()
        };
        let output = process(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(
            !unknown_path.exists(),
            "override prompt without session id must not write unknown override"
        );
    }

    #[test]
    fn test_process_override_rejects_unknown_session_id() {
        let tmpdir = tempfile::tempdir().unwrap();
        let fs: &'static ScopedHomeFs = Box::leak(Box::new(ScopedHomeFs {
            home: tmpdir.path().to_path_buf(),
        }));
        let ctx = scoped_ctx(fs);
        let unknown_path = hygiene_override_path(fs, "unknown");

        let input = HookInput {
            prompt: Some("override hygiene".to_string()),
            session_id: Some(" unknown ".to_string()),
            ..Default::default()
        };
        let output = process(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(
            !unknown_path.exists(),
            "override prompt with synthetic session id must not write unknown override"
        );
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

    /// Round-trip: a freshly computed token verifies successfully.
    #[test]
    fn test_valid_sig_verifies() {
        let session = "test-valid-sig";
        let ts = now_secs();
        let sig = compute_override_sig("hygiene", session, ts);
        let token = format!("{ts}:{sig}");
        assert!(
            verify_override_content(&token, "hygiene", session).is_some(),
            "valid token must verify"
        );
    }

    /// A tampered signature (one hex digit flipped) must be rejected.
    #[test]
    fn test_tampered_sig_fails() {
        let session = "test-tampered-sig";
        let ts = now_secs();
        let sig = compute_override_sig("hygiene", session, ts);
        // Flip the last hex digit to produce an invalid MAC.
        let mut bad_sig = sig.clone();
        let last = bad_sig.pop().unwrap_or('0');
        let flipped = if last == '0' { '1' } else { '0' };
        bad_sig.push(flipped);
        let token = format!("{ts}:{bad_sig}");
        assert!(
            verify_override_content(&token, "hygiene", session).is_none(),
            "tampered signature must be rejected"
        );
    }

    /// A token for one override type must not verify as a different type.
    #[test]
    fn test_wrong_type_fails() {
        let session = "test-wrong-type";
        let ts = now_secs();
        let sig = compute_override_sig("hygiene", session, ts);
        let token = format!("{ts}:{sig}");
        assert!(
            verify_override_content(&token, "verification", session).is_none(),
            "token for 'hygiene' must not verify as 'verification'"
        );
    }

    /// An empty or truncated file must be rejected.
    #[test]
    fn test_empty_content_fails() {
        assert!(verify_override_content("", "hygiene", "any-session").is_none());
        assert!(verify_override_content("12345", "hygiene", "any-session").is_none());
    }
}
