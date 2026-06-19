//! `sentinel manifest` — write/verify signed step-config manifests
//! (M2.13, sentinel #26)
//!
//! Produces or verifies a `manifest.toml` for every step-config TOML
//! under `<config_dir>/steps/`. The on-disk format is whatever
//! `sentinel_domain::step_manifest::StepConfigManifest` serializes to;
//! the CLI here is the only writing surface — domain types stay pure.
//!
//! Three modes:
//!
//! - `sentinel manifest write` — scan steps/, recompute hashes, write
//!   `manifest.toml`. Reads a 32-byte hex Ed25519 seed from `--key-env`
//!   (default: `SENTINEL_SIGNING_KEY`) and signs every entry. The public
//!   key (hex) is recorded in the manifest header so downstream "verify"
//!   steps can self-check.
//!
//! - `sentinel manifest verify` — re-read steps/ + `manifest.toml`,
//!   recompute hashes, require a public key via `--pubkey <hex>` or the
//!   manifest's recorded `public_key` header, and verify every signature.
//!
//! - `sentinel manifest show` — pretty-print the manifest summary
//!   (count of entries, signed-vs-unsigned, public key fingerprint).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use sentinel_domain::step_manifest::{ManifestCheck, ManifestError, StepConfigManifest};

/// Default file name for the manifest written into a steps/ directory.
pub const MANIFEST_FILENAME: &str = "manifest.toml";
const DEFAULT_SIGNING_KEY_ENV: &str = "SENTINEL_SIGNING_KEY";

/// What `sentinel manifest write` should do.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Path to `<config_dir>` (the parent of `steps/`).
    pub config_dir: PathBuf,
    /// Sign each entry with the Ed25519 key whose 32-byte hex-encoded seed
    /// lives in this env var. Defaults to `SENTINEL_SIGNING_KEY`.
    pub key_env: Option<String>,
    /// If true, print what would be written without touching the file.
    pub dry_run: bool,
}

/// What `sentinel manifest verify` should do.
#[derive(Debug, Clone)]
pub struct VerifyOptions {
    pub config_dir: PathBuf,
    /// Optional hex-encoded 32-byte public key to verify signatures
    /// against. If omitted, uses the manifest's `public_key`
    /// header (still trusted only as far as the binary that read the
    /// manifest trusts that header — the verifying key is a *hint*
    /// unless pinned out-of-band).
    pub pubkey_hex: Option<String>,
}

/// One entry's verification result. Distinct from `ManifestCheck` so we
/// can carry the entry name for reporting.
#[derive(Debug, Clone)]
pub struct EntryReport {
    pub name: String,
    pub result: Result<ManifestCheck, ManifestError>,
}

/// Aggregate verify result.
#[derive(Debug, Default)]
pub struct VerifyReport {
    pub entries: Vec<EntryReport>,
    /// All entries that didn't return Ok.
    pub failures: Vec<String>,
    /// Signed-and-verified successes.
    pub signed_ok: usize,
}

impl VerifyReport {
    #[must_use]
    pub const fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Build a manifest from the step-config TOMLs in `<config_dir>/steps/`.
/// Doesn't touch disk; caller decides whether to write it.
fn build_manifest(config_dir: &Path, signer: &SigningKey) -> Result<StepConfigManifest> {
    let steps_dir = config_dir.join("steps");
    if !steps_dir.is_dir() {
        bail!("steps directory not found: {}", steps_dir.display());
    }

    let mut manifest = StepConfigManifest::new();
    manifest.public_key = Some(hex::encode(signer.verifying_key().to_bytes()));

    // Stable iteration order so manifest diffs are reviewable.
    let mut sources: Vec<(String, String)> = Vec::new();
    let entries = std::fs::read_dir(&steps_dir)
        .with_context(|| format!("read_dir {}", steps_dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        // Skip the manifest file itself — it's not a step config.
        if path.file_name().and_then(|n| n.to_str()) == Some(MANIFEST_FILENAME) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let body =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        sources.push((stem.to_string(), body));
    }
    sources.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, body) in sources {
        manifest.upsert_signed(name, &body, signer);
    }

    Ok(manifest)
}

/// Load `manifest.toml` from `<config_dir>/steps/` if present.
fn load_manifest(config_dir: &Path) -> Result<StepConfigManifest> {
    let path = config_dir.join("steps").join(MANIFEST_FILENAME);
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&body).with_context(|| format!("parse {}", path.display()))
}

/// Resolve an Ed25519 `SigningKey` from a 32-byte hex seed in an env var.
fn signing_key_from_env(env_var: &str) -> Result<SigningKey> {
    let raw = std::env::var(env_var)
        .with_context(|| format!("env var {env_var} not set — needed for signing"))?;
    let bytes =
        hex::decode(raw.trim()).with_context(|| format!("env var {env_var} is not valid hex"))?;
    if bytes.len() != 32 {
        bail!(
            "env var {env_var} must be 32-byte (64 hex char) Ed25519 seed; got {} bytes",
            bytes.len()
        );
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes);
    Ok(SigningKey::from_bytes(&seed))
}

fn verifying_key_from_hex(hex_str: &str) -> Result<VerifyingKey> {
    let bytes = hex::decode(hex_str.trim()).context("public key is not valid hex")?;
    if bytes.len() != 32 {
        bail!(
            "public key must be 32-byte (64 hex char) Ed25519 verifying key; got {} bytes",
            bytes.len()
        );
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    VerifyingKey::from_bytes(&arr).map_err(|e| anyhow!("invalid Ed25519 public key: {e}"))
}

/// `sentinel manifest write` — write `<config_dir>/steps/manifest.toml`.
///
/// # Errors
/// - signing env var missing/invalid
/// - steps/ unreadable
/// - manifest serialization fails (shouldn't happen with valid data)
/// - disk write fails
pub fn run_write(opts: WriteOptions) -> Result<()> {
    let key_env = opts.key_env.as_deref().unwrap_or(DEFAULT_SIGNING_KEY_ENV);
    let signer = signing_key_from_env(key_env)?;

    let manifest = build_manifest(&opts.config_dir, &signer)?;
    let serialized = toml::to_string_pretty(&manifest).context("serialize manifest to TOML")?;

    let out_path = opts.config_dir.join("steps").join(MANIFEST_FILENAME);
    if opts.dry_run {
        println!(
            "# would write {} ({} entries):\n{serialized}",
            out_path.display(),
            manifest.entries.len()
        );
        return Ok(());
    }
    std::fs::write(&out_path, &serialized)
        .with_context(|| format!("write {}", out_path.display()))?;
    println!(
        "wrote {} ({} entries, {})",
        out_path.display(),
        manifest.entries.len(),
        "signed"
    );
    Ok(())
}

/// `sentinel manifest verify` — check `<config_dir>/steps/manifest.toml`.
///
/// # Errors
/// - manifest file missing or unparseable
/// - steps/ unreadable
/// - public key resolution fails
pub fn run_verify(opts: VerifyOptions) -> Result<VerifyReport> {
    let manifest = load_manifest(&opts.config_dir)?;

    // Resolve the verifying key: explicit --pubkey wins, else manifest header.
    // Enterprise verification is signature-only: without a public key, this
    // command fails instead of downgrading to hash-only acceptance.
    let verifying = match opts.pubkey_hex.as_deref() {
        Some(hex_str) => Some(verifying_key_from_hex(hex_str)?),
        None => match manifest.public_key.as_deref() {
            Some(hex_str) => Some(verifying_key_from_hex(hex_str)?),
            None => None,
        },
    };
    let verifying = verifying.ok_or_else(|| {
        anyhow!(
            "manifest verification requires an Ed25519 public key via --pubkey or manifest public_key"
        )
    })?;

    let mut report = VerifyReport::default();
    let steps_dir = opts.config_dir.join("steps");

    for entry in &manifest.entries {
        let path = steps_dir.join(format!("{}.toml", entry.name));
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                report.entries.push(EntryReport {
                    name: entry.name.clone(),
                    result: Err(ManifestError::MissingEntry),
                });
                report.failures.push(format!("{}: {e}", entry.name));
                continue;
            }
        };

        let result = manifest.verify_entry(&entry.name, &source, Some(&verifying));

        match &result {
            Ok(ManifestCheck::SignedOk) => report.signed_ok += 1,
            Ok(ManifestCheck::HashOnlyOk) => report.failures.push(format!(
                "{}: hash-only verification is not authoritative",
                entry.name
            )),
            Err(e) => report.failures.push(format!("{}: {e}", entry.name)),
        }
        report.entries.push(EntryReport {
            name: entry.name.clone(),
            result,
        });
    }
    Ok(report)
}

/// `sentinel manifest show` — print a one-line summary plus per-entry status.
///
/// # Errors
/// - manifest missing or unparseable
pub fn run_show(config_dir: &Path) -> Result<()> {
    let manifest = load_manifest(config_dir)?;
    let signed = manifest
        .entries
        .iter()
        .filter(|e| e.signature.is_some())
        .count();
    let unsigned = manifest.entries.len() - signed;
    let key_summary = manifest.public_key.as_deref().map_or_else(
        || "public_key = (none)".to_string(),
        |k| format!("public_key = {}…{}", &k[..8], &k[k.len() - 8..]),
    );
    println!(
        "manifest version {} | {} entries ({signed} signed, {unsigned} unsigned) | {key_summary}",
        manifest.version,
        manifest.entries.len()
    );
    for e in &manifest.entries {
        let marker = if e.signature.is_some() { "S" } else { "h" };
        println!("  [{marker}] {} {}", e.name, &e.hash[..16]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_config_dir_with_steps() -> TempDir {
        let dir = TempDir::new().unwrap();
        let steps = dir.path().join("steps");
        fs::create_dir_all(&steps).unwrap();
        fs::write(steps.join("linear.toml"), "name = \"linear\"\n").unwrap();
        fs::write(steps.join("github.toml"), "name = \"github\"\n").unwrap();
        dir
    }

    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn build_manifest_signs_one_entry_per_step_config() {
        let dir = make_config_dir_with_steps();
        let key = test_key();
        let manifest = build_manifest(dir.path(), &key).unwrap();
        assert_eq!(manifest.entries.len(), 2);
        let names: Vec<&str> = manifest.entries.iter().map(|e| e.name.as_str()).collect();
        // Sorted iteration order.
        assert_eq!(names, vec!["github", "linear"]);
        assert!(manifest.entries.iter().all(|e| e.signature.is_some()));
        assert_eq!(
            manifest.public_key.as_deref(),
            Some(hex::encode(key.verifying_key().to_bytes()).as_str())
        );
    }

    #[test]
    fn build_manifest_skips_existing_manifest_toml() {
        let dir = make_config_dir_with_steps();
        // Pre-existing manifest.toml in steps/ — must not be hashed as a step.
        fs::write(
            dir.path().join("steps").join(MANIFEST_FILENAME),
            "# placeholder\n",
        )
        .unwrap();
        let key = test_key();
        let manifest = build_manifest(dir.path(), &key).unwrap();
        let names: Vec<&str> = manifest.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["github", "linear"]);
        assert!(
            !names.contains(&"manifest"),
            "manifest.toml itself must never become an entry"
        );
    }

    #[test]
    fn build_manifest_signed_records_public_key_in_header() {
        let dir = make_config_dir_with_steps();
        let key = test_key();
        let manifest = build_manifest(dir.path(), &key).unwrap();
        assert!(manifest.entries.iter().all(|e| e.signature.is_some()));
        assert_eq!(
            manifest.public_key.as_deref(),
            Some(hex::encode(key.verifying_key().to_bytes()).as_str())
        );
    }

    #[test]
    fn run_write_missing_key_fails_closed() {
        let dir = make_config_dir_with_steps();
        std::env::remove_var("TEST_MISSING_MANIFEST_SIGN_KEY");
        let err = run_write(WriteOptions {
            config_dir: dir.path().to_path_buf(),
            key_env: Some("TEST_MISSING_MANIFEST_SIGN_KEY".to_string()),
            dry_run: false,
        })
        .expect_err("manifest write without signing key must fail closed");
        assert!(
            err.to_string().contains("TEST_MISSING_MANIFEST_SIGN_KEY"),
            "error must name missing signing env var: {err:#}"
        );
    }

    #[test]
    fn run_verify_detects_drift_after_edit() {
        let dir = make_config_dir_with_steps();
        let seed_hex = "0808080808080808080808080808080808080808080808080808080808080808";
        std::env::set_var("TEST_SIGN_KEY_DRIFT", seed_hex);
        run_write(WriteOptions {
            config_dir: dir.path().to_path_buf(),
            key_env: Some("TEST_SIGN_KEY_DRIFT".to_string()),
            dry_run: false,
        })
        .unwrap();

        // Edit one step config AFTER manifest was written.
        fs::write(
            dir.path().join("steps").join("linear.toml"),
            "name = \"linear\"\nversion = 2\n",
        )
        .unwrap();

        let report = run_verify(VerifyOptions {
            config_dir: dir.path().to_path_buf(),
            pubkey_hex: None,
        })
        .unwrap();
        std::env::remove_var("TEST_SIGN_KEY_DRIFT");
        assert!(!report.ok());
        assert_eq!(report.failures.len(), 1);
        assert!(
            report.failures[0].starts_with("linear:"),
            "failure must name the drifted entry: {:?}",
            report.failures
        );
    }

    #[test]
    fn signing_key_from_env_rejects_wrong_length() {
        std::env::set_var("TEST_KEY_WRONG_LEN", "deadbeef");
        let err = signing_key_from_env("TEST_KEY_WRONG_LEN").expect_err("short hex must error");
        let msg = err.to_string();
        assert!(
            msg.contains("32-byte"),
            "error must mention required size: {msg}"
        );
        std::env::remove_var("TEST_KEY_WRONG_LEN");
    }

    #[test]
    fn run_write_with_signing_then_verify_strict_round_trip() {
        let dir = make_config_dir_with_steps();

        // 32 bytes (64 hex chars) deterministic seed.
        let seed_hex = "0707070707070707070707070707070707070707070707070707070707070707";
        std::env::set_var("TEST_SIGN_KEY", seed_hex);

        run_write(WriteOptions {
            config_dir: dir.path().to_path_buf(),
            key_env: Some("TEST_SIGN_KEY".to_string()),
            dry_run: false,
        })
        .unwrap();

        let report = run_verify(VerifyOptions {
            config_dir: dir.path().to_path_buf(),
            pubkey_hex: None, // use manifest's public_key header
        })
        .unwrap();
        assert!(
            report.ok(),
            "signed manifest must verify: {:?}",
            report.failures
        );
        assert_eq!(report.signed_ok, 2);

        std::env::remove_var("TEST_SIGN_KEY");
    }

    #[test]
    fn run_verify_rejects_unsigned_manifest_entry() {
        let dir = make_config_dir_with_steps();
        let steps_dir = dir.path().join("steps");
        let mut manifest = StepConfigManifest::new();
        manifest.public_key = Some(hex::encode(test_key().verifying_key().to_bytes()));
        manifest.upsert_hash_only("linear", "name = \"linear\"\n");
        let body = toml::to_string_pretty(&manifest).unwrap();
        fs::write(steps_dir.join(MANIFEST_FILENAME), body).unwrap();

        let report = run_verify(VerifyOptions {
            config_dir: dir.path().to_path_buf(),
            pubkey_hex: None,
        })
        .unwrap();
        assert!(!report.ok(), "unsigned manifest entry must fail verify");
        assert_eq!(report.failures.len(), 1);
        assert!(
            report.failures[0].contains("unsigned"),
            "failure must name unsigned entry: {:?}",
            report.failures
        );
    }
}
