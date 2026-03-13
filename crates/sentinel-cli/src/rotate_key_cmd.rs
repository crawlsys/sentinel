//! `sentinel rotate-key` — Rotate the HMAC signing key
//!
//! Creates a new versioned secret file (`.hmac-secret-v{N+1}`).
//! Old key versions are preserved so existing signatures can still be verified.
//! After rotation, run `sentinel resign` to re-sign all files with the new key.

use anyhow::Result;

pub async fn run() -> Result<()> {
    let new_version = sentinel_infrastructure::state_store::rotate_hmac_key()?;

    eprintln!("[sentinel] Key rotation complete. New version: v{new_version}");
    eprintln!(
        "[sentinel] Run `sentinel resign` to re-sign all state and proof files \
         with the new key."
    );

    Ok(())
}
