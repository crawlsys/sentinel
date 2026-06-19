//! Signed step-config manifests (M2.13, sentinel #26 — Microsoft AGT pattern)
//!
//! Each entry in a step-config directory (`~/.claude/sentinel/config/steps/*.toml`)
//! can be paired with a manifest entry that records its canonical SHA-256
//! hash plus an optional Ed25519 signature over that hash. The manifest is
//! a per-directory file (typically `manifest.toml`) holding one entry per
//! step config TOML. Producers sign at publish time; consumers verify at
//! load time before trusting any step definition.
//!
//! ## Why a manifest, not signatures-in-config
//!
//! 1. Step configs are human-edited TOML. Adding signature fields directly
//!    inline would make every edit invalidate the file's signature — no
//!    workflow that humans actually maintain.
//!
//! 2. A separate manifest lets the producer canonicalize the source before
//!    hashing (strip trailing whitespace, normalize line endings) without
//!    touching the source file's bytes.
//!
//! 3. The manifest itself can be small enough to fit in code review with
//!    the source — reviewers can see "this hash matches this content"
//!    without diffing megabytes of TOML.
//!
//! ## Trust model
//!
//! - **Signed verification**: producer signs the hash with an Ed25519 key.
//!   Consumer verifies against a configured public key. Catches any
//!   tampering of either the source or the manifest, assuming the
//!   public key is delivered out-of-band (in a binary, in a TUF root,
//!   or pinned in code).
//!
//! - **Hash-only inspection**: non-authoritative helper for tests and local
//!   drift checks. Strict verification rejects unsigned entries.
//!
//! ## Canonical hash
//!
//! To make the hash deterministic across editor save settings, we:
//!
//! 1. Strip a leading UTF-8 BOM if present.
//! 2. Normalize line endings: CRLF → LF.
//! 3. Strip trailing whitespace on each line.
//! 4. Strip trailing blank lines.
//! 5. Ensure exactly one trailing newline.
//! 6. SHA-256 the result.
//!
//! This matches what `rustfmt` and most pre-commit hooks already do.
//! If a producer's source has Windows line endings and a Unix consumer
//! checks it out with `core.autocrlf=input`, both still compute the
//! same hash. The canonicalization is deliberately limited to whitespace
//! — TOML key reordering or comment changes WILL invalidate the hash,
//! since they're meaningful edits.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::step_proof::SignatureError;

/// One entry in a step-config manifest: name + canonical hash + optional
/// Ed25519 signature over the hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Skill name or filename stem the entry covers (e.g. `"linear"` for
    /// `linear.toml`). Producers MUST keep this stable across edits —
    /// renaming a step config requires re-signing.
    pub name: String,

    /// Lowercase hex SHA-256 of the canonicalized source bytes.
    /// See module docs for canonicalization rules.
    pub hash: String,

    /// Optional Ed25519 signature over the bytes of `hash` (NOT over the
    /// canonical source — keeps signatures and hashes verifiable
    /// independently). Hex-encoded 64-byte signature when present,
    /// `None` only for non-authoritative hash inspection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// A signed manifest for a step-config directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StepConfigManifest {
    /// Manifest schema version. Bump on breaking change to the canonical
    /// hash algorithm or the manifest wire format. Consumers refuse
    /// manifests with versions they don't recognize.
    #[serde(default = "default_manifest_version")]
    pub version: u32,

    /// Optional hex-encoded Ed25519 public key the entries are signed
    /// with. Strict verification requires either this key or an out-of-band
    /// key supplied by the caller.
    /// Including the key in the manifest is a convenience — the
    /// *trust* in the key still has to come from an out-of-band channel
    /// (binary, TUF root, pinned constant). Don't be fooled into
    /// thinking the manifest authenticates itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,

    /// Per-step-config entries.
    pub entries: Vec<ManifestEntry>,
}

const MANIFEST_VERSION: u32 = 1;

const fn default_manifest_version() -> u32 {
    MANIFEST_VERSION
}

impl StepConfigManifest {
    /// Empty manifest with the current schema version.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            version: MANIFEST_VERSION,
            public_key: None,
            entries: Vec::new(),
        }
    }

    /// Canonicalize source bytes per the rules in this module's docs.
    /// Returns the canonical bytes — caller can hash them, sign over the
    /// hash, or write them to disk for reproducibility checks.
    #[must_use]
    pub fn canonicalize(source: &str) -> String {
        // 1. Strip BOM.
        let stripped = source.strip_prefix('\u{FEFF}').unwrap_or(source);

        // 2-4. Per-line: trim trailing whitespace.
        let normalized: Vec<&str> = stripped.lines().map(str::trim_end).collect();

        // Strip trailing blank lines.
        let mut end = normalized.len();
        while end > 0 && normalized[end - 1].is_empty() {
            end -= 1;
        }

        let mut out = normalized[..end].join("\n");
        // 5. Exactly one trailing newline (only if the body isn't empty,
        // so a fully blank file stays empty).
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    /// Lowercase hex SHA-256 of canonicalized source.
    #[must_use]
    pub fn compute_hash(source: &str) -> String {
        let canonical = Self::canonicalize(source);
        let digest = Sha256::digest(canonical.as_bytes());
        hex::encode(digest)
    }

    /// Add (or replace) an unsigned entry for non-authoritative hash
    /// inspection. If an entry with the same `name` exists it's overwritten
    /// so re-running this is idempotent.
    pub fn upsert_hash_only(&mut self, name: impl Into<String>, source: &str) {
        let name = name.into();
        let hash = Self::compute_hash(source);
        let entry = ManifestEntry {
            name,
            hash,
            signature: None,
        };
        self.replace_or_push(entry);
    }

    /// Add (or replace) a signed entry. The signature covers the BYTES
    /// of the hex hash string (not the canonical source bytes), so
    /// verification only needs the hash, not the source. This mirrors
    /// the `StepProof::sign_with` convention from M1.7 — signatures are
    /// always over hashes, never over content.
    pub fn upsert_signed(&mut self, name: impl Into<String>, source: &str, key: &SigningKey) {
        let name = name.into();
        let hash = Self::compute_hash(source);
        let signature: Signature = key.sign(hash.as_bytes());
        let entry = ManifestEntry {
            name,
            hash,
            signature: Some(hex::encode(signature.to_bytes())),
        };
        self.replace_or_push(entry);
    }

    fn replace_or_push(&mut self, entry: ManifestEntry) {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.name == entry.name) {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }

    /// Verify one entry against fresh source bytes.
    ///
    /// Returns:
    /// - `Ok(ManifestCheck::SignedOk)` — hash matches AND signature verifies
    ///   against the supplied public key.
    /// - `Err(ManifestError::HashMismatch)` — the source's canonical hash
    ///   doesn't match the manifest's hash. Source has drifted or the
    ///   manifest is stale.
    /// - `Err(ManifestError::MissingEntry)` — no entry by that name.
    /// - `Err(ManifestError::Signature(_))` — signature exists but is
    ///   malformed or doesn't verify.
    /// - `Err(ManifestError::SignatureRequired)` — hash matches, but the
    ///   entry is unsigned. Call `verify_entry_hash_only` for local hash checks.
    /// - `Err(ManifestError::PublicKeyRequired)` — entry is signed but no
    ///   verifying key was supplied.
    pub fn verify_entry(
        &self,
        name: &str,
        source: &str,
        verifying_key: Option<&VerifyingKey>,
    ) -> Result<ManifestCheck, ManifestError> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.name == name)
            .ok_or(ManifestError::MissingEntry)?;

        let actual_hash = Self::compute_hash(source);
        if actual_hash != entry.hash {
            return Err(ManifestError::HashMismatch);
        }

        match (&entry.signature, verifying_key) {
            (None, _) => Err(ManifestError::SignatureRequired),
            (Some(_), None) => Err(ManifestError::PublicKeyRequired),
            (Some(sig_hex), Some(key)) => {
                let sig_bytes = hex::decode(sig_hex)
                    .map_err(|_| ManifestError::Signature(SignatureError::InvalidEncoding))?;
                if sig_bytes.len() != ed25519_dalek::SIGNATURE_LENGTH {
                    return Err(ManifestError::Signature(SignatureError::InvalidLength));
                }
                let mut sig_array = [0u8; ed25519_dalek::SIGNATURE_LENGTH];
                sig_array.copy_from_slice(&sig_bytes);
                let signature = Signature::from_bytes(&sig_array);
                key.verify(entry.hash.as_bytes(), &signature)
                    .map(|()| ManifestCheck::SignedOk)
                    .map_err(|_| ManifestError::Signature(SignatureError::VerificationFailed))
            }
        }
    }

    /// Hash-only check — a local integrity helper for tests and explicit
    /// non-authoritative inspection. Same hash comparison as `verify_entry`
    /// minus signature handling, so a signed entry that has the right hash
    /// but a wrong or missing key still returns `Ok(...)`. Production paths
    /// use `verify_entry`.
    pub fn verify_entry_hash_only(
        &self,
        name: &str,
        source: &str,
    ) -> Result<ManifestCheck, ManifestError> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.name == name)
            .ok_or(ManifestError::MissingEntry)?;

        let actual_hash = Self::compute_hash(source);
        if actual_hash != entry.hash {
            return Err(ManifestError::HashMismatch);
        }
        // Hash-only mode reports HashOnlyOk regardless of whether the
        // entry is signed — by definition this mode ignores signatures.
        Ok(ManifestCheck::HashOnlyOk)
    }
}

/// Successful manifest-check outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestCheck {
    /// The hash matched without signature verification. This is returned only
    /// by explicit hash-only verification.
    HashOnlyOk,
    /// The entry was signed and the signature verified against the
    /// supplied key. Trust level is "signed by holder of the matching
    /// private key."
    SignedOk,
}

/// Errors from `StepConfigManifest::verify_entry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// No entry by the given name in this manifest.
    MissingEntry,
    /// Source canonical hash doesn't match the manifest's recorded hash.
    /// Source has drifted, or the manifest is stale.
    HashMismatch,
    /// Entry is signed but the caller supplied no verifying key. Either
    /// fix the caller to supply one (production path) or call
    /// `verify_entry_hash_only` for non-authoritative hash inspection.
    PublicKeyRequired,
    /// Entry is unsigned. Strict manifest verification requires a signature.
    SignatureRequired,
    /// Signature is present and a key was supplied, but the signature
    /// failed to decode or verify.
    Signature(SignatureError),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingEntry => write!(f, "no manifest entry by that name"),
            Self::HashMismatch => write!(f, "source canonical hash does not match manifest entry"),
            Self::PublicKeyRequired => {
                write!(f, "manifest entry is signed but no public key was supplied")
            }
            Self::SignatureRequired => write!(f, "manifest entry is unsigned"),
            Self::Signature(e) => write!(f, "signature error: {e}"),
        }
    }
}

impl std::error::Error for ManifestError {}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    /// Deterministic key for tests — keeps the test suite reproducible
    /// and avoids pulling `rand` in as a domain-crate test dep.
    fn fresh_key() -> SigningKey {
        SigningKey::from_bytes(&[42u8; 32])
    }

    fn other_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn canonicalize_strips_bom() {
        let with_bom = "\u{FEFF}name = \"linear\"\n";
        assert_eq!(
            StepConfigManifest::canonicalize(with_bom),
            "name = \"linear\"\n"
        );
    }

    #[test]
    fn canonicalize_normalizes_crlf_to_lf() {
        let crlf = "a = 1\r\nb = 2\r\n";
        let lf = "a = 1\nb = 2\n";
        assert_eq!(
            StepConfigManifest::canonicalize(crlf),
            StepConfigManifest::canonicalize(lf)
        );
    }

    #[test]
    fn canonicalize_strips_trailing_whitespace_per_line() {
        let messy = "a = 1   \nb = 2\t\n";
        assert_eq!(StepConfigManifest::canonicalize(messy), "a = 1\nb = 2\n");
    }

    #[test]
    fn canonicalize_strips_trailing_blank_lines() {
        let trailing = "a = 1\n\n\n\n";
        assert_eq!(StepConfigManifest::canonicalize(trailing), "a = 1\n");
    }

    #[test]
    fn canonicalize_empty_stays_empty() {
        assert_eq!(StepConfigManifest::canonicalize(""), "");
        assert_eq!(StepConfigManifest::canonicalize("\n\n\n"), "");
    }

    #[test]
    fn compute_hash_is_stable_across_whitespace_variants() {
        let a = "name = \"linear\"\nversion = 1\n";
        let b = "\u{FEFF}name = \"linear\"   \r\nversion = 1\t\r\n\n\n";
        assert_eq!(
            StepConfigManifest::compute_hash(a),
            StepConfigManifest::compute_hash(b),
            "canonicalization must absorb editor differences"
        );
    }

    #[test]
    fn compute_hash_differs_for_meaningful_edit() {
        // Adding a comment IS meaningful — hash must change.
        let a = "name = \"linear\"\n";
        let b = "# comment\nname = \"linear\"\n";
        assert_ne!(
            StepConfigManifest::compute_hash(a),
            StepConfigManifest::compute_hash(b)
        );
    }

    #[test]
    fn upsert_hash_only_then_verify_hash_only_round_trip() {
        let mut m = StepConfigManifest::new();
        let src = "name = \"linear\"\n";
        m.upsert_hash_only("linear", src);
        assert!(matches!(
            m.verify_entry_hash_only("linear", src),
            Ok(ManifestCheck::HashOnlyOk)
        ));
    }

    #[test]
    fn strict_verify_rejects_unsigned_entry() {
        let key = fresh_key();
        let mut m = StepConfigManifest::new();
        let src = "name = \"linear\"\n";
        m.upsert_hash_only("linear", src);

        let err = m
            .verify_entry("linear", src, Some(&key.verifying_key()))
            .expect_err("strict verification must reject unsigned entries");
        assert_eq!(err, ManifestError::SignatureRequired);
    }

    #[test]
    fn upsert_signed_then_verify_with_pubkey() {
        let key = fresh_key();
        let verifying = key.verifying_key();

        let mut m = StepConfigManifest::new();
        let src = "name = \"linear\"\n";
        m.upsert_signed("linear", src, &key);

        assert!(matches!(
            m.verify_entry("linear", src, Some(&verifying)),
            Ok(ManifestCheck::SignedOk)
        ));
    }

    #[test]
    fn signed_entry_without_pubkey_errors_in_strict_mode() {
        let key = fresh_key();
        let mut m = StepConfigManifest::new();
        m.upsert_signed("linear", "name = \"linear\"\n", &key);

        let err = m
            .verify_entry("linear", "name = \"linear\"\n", None)
            .expect_err("strict verify with no key must error");
        assert_eq!(err, ManifestError::PublicKeyRequired);
    }

    #[test]
    fn signed_entry_without_pubkey_works_in_hash_only_mode() {
        // Producer signed; consumer doesn't have the key but still wants
        // bit-rot protection. Hash-only mode lets them get exactly that.
        let key = fresh_key();
        let mut m = StepConfigManifest::new();
        m.upsert_signed("linear", "name = \"linear\"\n", &key);

        assert!(matches!(
            m.verify_entry_hash_only("linear", "name = \"linear\"\n"),
            Ok(ManifestCheck::HashOnlyOk)
        ));
    }

    #[test]
    fn hash_mismatch_detected() {
        let mut m = StepConfigManifest::new();
        m.upsert_hash_only("linear", "name = \"linear\"\n");
        // Edit the source.
        let drifted = "name = \"linear\"\nextra = true\n";
        assert_eq!(
            m.verify_entry_hash_only("linear", drifted),
            Err(ManifestError::HashMismatch)
        );
    }

    #[test]
    fn missing_entry_detected() {
        let m = StepConfigManifest::new();
        assert_eq!(
            m.verify_entry_hash_only("ghost", "anything"),
            Err(ManifestError::MissingEntry)
        );
    }

    #[test]
    fn signature_with_wrong_key_fails() {
        let signer = fresh_key();
        let other = other_key();

        let mut m = StepConfigManifest::new();
        let src = "name = \"linear\"\n";
        m.upsert_signed("linear", src, &signer);

        let err = m
            .verify_entry("linear", src, Some(&other.verifying_key()))
            .expect_err("verifying with the wrong key must error");
        assert!(
            matches!(
                err,
                ManifestError::Signature(SignatureError::VerificationFailed)
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn upsert_replaces_existing_entry_idempotent() {
        let mut m = StepConfigManifest::new();
        m.upsert_hash_only("linear", "name = \"linear\"\n");
        let first_hash = m.entries[0].hash.clone();

        // Same name, different source — should replace, not append.
        m.upsert_hash_only("linear", "name = \"linear\"\nversion = 1\n");
        assert_eq!(m.entries.len(), 1, "upsert must not append duplicates");
        assert_ne!(m.entries[0].hash, first_hash);
    }

    #[test]
    fn serde_roundtrip_preserves_signed_entries() {
        let key = fresh_key();
        let mut m = StepConfigManifest::new();
        m.public_key = Some(hex::encode(key.verifying_key().to_bytes()));
        m.upsert_signed("linear", "name = \"linear\"\n", &key);
        m.upsert_hash_only("github", "name = \"github\"\n");

        let toml_str = toml::to_string(&m).expect("manifest serializes to TOML");
        let back: StepConfigManifest = toml::from_str(&toml_str).expect("TOML round-trips");
        assert_eq!(back, m);
    }
}
