//! `sentinel resign` — Re-sign all state and proof chain files with the current key
//!
//! After an HMAC key rotation, existing `.sig` files contain signatures from
//! the old key version. While `verify_hmac()` supports multi-version verification,
//! re-signing consolidates everything to the latest key version.
//!
//! This reads each file, verifies the old signature (must be valid), then
//! writes a new signature with the current key.

use anyhow::{Context, Result};

pub fn run() -> Result<()> {
    let mut state_count = 0u32;
    let mut proof_count = 0u32;
    let mut errors = 0u32;

    // Re-sign state files
    let state_dir = sentinel_infrastructure::state_store::state_dir();
    if state_dir.exists() {
        for entry in std::fs::read_dir(&state_dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            // Process .json state files (skip .sig, .lock files)
            if name.ends_with(".json") && !name.ends_with(".json.sig") {
                let sig_path = path.with_extension("json.sig");
                match resign_file(&path, &sig_path) {
                    Ok(()) => state_count += 1,
                    Err(e) => {
                        eprintln!(
                            "[sentinel] WARNING: Failed to re-sign {}: {e}",
                            path.display()
                        );
                        errors += 1;
                    }
                }
            }
        }
    }

    // Re-sign proof chain files
    let proof_dir = sentinel_infrastructure::proof_store::proof_dir_public();
    if proof_dir.exists() {
        for entry in std::fs::read_dir(&proof_dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if name.ends_with("-chain.json") {
                let sig_path = proof_dir.join(format!(
                    "{}.sig",
                    path.file_name().unwrap().to_string_lossy()
                ));
                match resign_file(&path, &sig_path) {
                    Ok(()) => proof_count += 1,
                    Err(e) => {
                        eprintln!(
                            "[sentinel] WARNING: Failed to re-sign {}: {e}",
                            path.display()
                        );
                        errors += 1;
                    }
                }
            }
        }
    }

    eprintln!(
        "[sentinel] Re-signed {state_count} state files, {proof_count} proof chains. \
         {errors} errors."
    );

    if errors > 0 {
        anyhow::bail!(
            "{errors} files could not be re-signed. They may have been tampered with \
             or signed with a key version that no longer exists on disk."
        );
    }

    Ok(())
}

/// Read a data file, verify its existing signature, then write a new signature
/// using the current key version.
fn resign_file(data_path: &std::path::Path, sig_path: &std::path::Path) -> Result<()> {
    let data = std::fs::read(data_path)
        .with_context(|| format!("Failed to read {}", data_path.display()))?;

    // Verify old signature if it exists (reject tampered files)
    if sig_path.exists() {
        let old_sig = std::fs::read_to_string(sig_path)
            .with_context(|| format!("Failed to read {}", sig_path.display()))?;
        if !sentinel_infrastructure::state_store::verify_hmac_for_proofs(&data, old_sig.trim()) {
            anyhow::bail!("Existing signature verification failed — file may be tampered");
        }
    }

    // Compute new signature with current key version
    let new_sig = sentinel_infrastructure::state_store::compute_hmac_for_proofs(&data);
    std::fs::write(sig_path, &new_sig)
        .with_context(|| format!("Failed to write {}", sig_path.display()))?;

    Ok(())
}
