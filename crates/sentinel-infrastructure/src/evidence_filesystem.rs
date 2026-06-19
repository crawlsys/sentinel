//! Filesystem evidence adapter — first concrete adapter for THE
//! BIBLE framework (#38, follow-up to commit b6ce03f which shipped
//! the trait + registry).
//!
//! Verifies claims of the form `"filesystem.<verb>"` by stat-ing
//! the named path on the local disk. The minimal, foundational
//! adapter — every other adapter (GitHub, Linear, Browserbase) is
//! some variant of "go ask the external system if X happened," and
//! the filesystem one proves the framework wires correctly without
//! pulling in HTTP / auth / network mocking.
//!
//! # Supported claim types
//!
//! - `filesystem.file_exists` — context `{ "path": "..." }`,
//!   verified iff the path exists AND is a file.
//! - `filesystem.dir_exists` — context `{ "path": "..." }`,
//!   verified iff the path exists AND is a directory.
//! - `filesystem.file_contains_hash` — context `{ "path": "...",
//!   "expected_sha256": "..." }`, verified iff the file's SHA-256
//!   matches `expected_sha256` byte-for-byte. The strongest
//!   filesystem claim — locks "this file's bytes are exactly what
//!   I claimed they are."
//!
//! # Path safety
//!
//! Paths are taken verbatim from the claim context — the adapter
//! does NOT resolve symlinks, expand `~`, or chroot. Callers that
//! emit filesystem claims are responsible for canonicalising the
//! path before passing it through.
//!
//! # `verified` field semantics
//!
//! Mirrors the trait contract: `verified: true` means the adapter
//! POSITIVELY CONFIRMED the claim. A `file_exists` claim against a
//! non-existent path produces `verified: false` (with the payload
//! showing `exists: false`) — NOT an `AdapterError`. The receipt
//! distinguishes "the file isn't there" (a legitimate negative
//! answer worth recording) from "I couldn't even check" (a
//! transient I/O error worth surfacing).

use async_trait::async_trait;
use chrono::Utc;
use sentinel_application::evidence_adapters::EvidenceAdapter;
use sentinel_domain::evidence_adapter::{AdapterError, EvidenceClaim, EvidenceReceipt};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Adapter name. Pinned as a constant so callers and tests can
/// reference without the indirect `.name()` call.
pub const ADAPTER_NAME: &str = "filesystem";

/// Claim-type prefix. The adapter accepts any
/// `filesystem.<verb>` claim and dispatches by verb in `fetch`.
pub const CLAIM_PREFIX: &str = "filesystem.";

/// Filesystem evidence adapter — implements [`EvidenceAdapter`] for
/// `filesystem.<verb>` claims.
///
/// Stateless: holds no fields, no auth, no HTTP client. The
/// `verified` field on every receipt is determined by stat()-ing
/// the path; failures of the stat itself become `AdapterError::Fetch`,
/// not silent unverified receipts.
#[derive(Debug, Default, Clone, Copy)]
pub struct FilesystemAdapter;

impl FilesystemAdapter {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn extract_path(claim: &EvidenceClaim) -> Result<PathBuf, AdapterError> {
        let path = claim
            .context
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AdapterError::Fetch(format!(
                    "filesystem adapter requires context.path: string for claim '{}'",
                    claim.claim_type
                ))
            })?;
        if path.is_empty() {
            return Err(AdapterError::Fetch(
                "filesystem adapter rejects empty context.path".to_string(),
            ));
        }
        Ok(PathBuf::from(path))
    }

    fn handle_file_exists(claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError> {
        let path = Self::extract_path(claim)?;
        let exists = path.exists();
        let is_file = exists && path.is_file();
        let size_bytes = if is_file {
            std::fs::metadata(&path).ok().map(|m| m.len())
        } else {
            None
        };
        let verified = is_file;
        let payload = serde_json::json!({
            "path": path.to_string_lossy(),
            "exists": exists,
            "is_file": is_file,
            "size_bytes": size_bytes,
        });
        Ok(EvidenceReceipt::new(
            ADAPTER_NAME.to_string(),
            claim,
            verified,
            payload,
            Utc::now(),
        ))
    }

    fn handle_dir_exists(claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError> {
        let path = Self::extract_path(claim)?;
        let exists = path.exists();
        let is_dir = exists && path.is_dir();
        let entry_count = if is_dir {
            std::fs::read_dir(&path).ok().map(|rd| rd.count() as u64)
        } else {
            None
        };
        let verified = is_dir;
        let payload = serde_json::json!({
            "path": path.to_string_lossy(),
            "exists": exists,
            "is_dir": is_dir,
            "entry_count": entry_count,
        });
        Ok(EvidenceReceipt::new(
            ADAPTER_NAME.to_string(),
            claim,
            verified,
            payload,
            Utc::now(),
        ))
    }

    fn handle_file_contains_hash(claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError> {
        let path = Self::extract_path(claim)?;
        let expected = claim
            .context
            .get("expected_sha256")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AdapterError::Fetch(
                    "filesystem.file_contains_hash requires context.expected_sha256: string"
                        .to_string(),
                )
            })?;

        let bytes = std::fs::read(&path).map_err(|e| {
            AdapterError::Fetch(format!(
                "filesystem.file_contains_hash: read('{}') failed: {e}",
                path.display()
            ))
        })?;
        let actual = compute_sha256(&bytes);
        let matches = actual.eq_ignore_ascii_case(expected);
        let payload = serde_json::json!({
            "path": path.to_string_lossy(),
            "actual_sha256": actual,
            "expected_sha256": expected,
            "matches": matches,
            "size_bytes": bytes.len() as u64,
        });
        Ok(EvidenceReceipt::new(
            ADAPTER_NAME.to_string(),
            claim,
            matches,
            payload,
            Utc::now(),
        ))
    }
}

#[async_trait]
impl EvidenceAdapter for FilesystemAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    fn supports(&self, claim_type: &str) -> bool {
        claim_type.starts_with(CLAIM_PREFIX)
    }

    async fn fetch(&self, claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError> {
        match claim.claim_type.as_str() {
            "filesystem.file_exists" => Self::handle_file_exists(claim),
            "filesystem.dir_exists" => Self::handle_dir_exists(claim),
            "filesystem.file_contains_hash" => Self::handle_file_contains_hash(claim),
            other => Err(AdapterError::Fetch(format!(
                "filesystem adapter doesn't know verb '{other}' (supported: file_exists, dir_exists, file_contains_hash)"
            ))),
        }
    }
}

fn compute_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Cheap helper to compute the expected hash for a path on disk.
/// Useful for tests and for callers building claims dynamically.
///
/// # Errors
///
/// Returns `AdapterError::Fetch` when the file can't be read.
pub fn sha256_of_file(path: &Path) -> Result<String, AdapterError> {
    let bytes = std::fs::read(path).map_err(|e| {
        AdapterError::Fetch(format!(
            "sha256_of_file: read('{}') failed: {e}",
            path.display()
        ))
    })?;
    Ok(compute_sha256(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(claim_type: &str, ctx: serde_json::Value) -> EvidenceClaim {
        EvidenceClaim {
            skill: "test".to_string(),
            phase_id: "p".to_string(),
            step_id: "s".to_string(),
            claim_type: claim_type.to_string(),
            context: ctx,
        }
    }

    #[tokio::test]
    async fn file_exists_verified_when_file_present() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"hello").unwrap();
        let adapter = FilesystemAdapter::new();
        let c = claim(
            "filesystem.file_exists",
            serde_json::json!({"path": tmp.path().to_string_lossy()}),
        );
        let r = adapter.fetch(&c).await.unwrap();
        assert!(r.verified);
        assert_eq!(r.adapter_name, ADAPTER_NAME);
        assert_eq!(r.payload["exists"], true);
        assert_eq!(r.payload["is_file"], true);
        assert_eq!(r.payload["size_bytes"], 5);
    }

    #[tokio::test]
    async fn file_exists_unverified_when_path_missing() {
        let adapter = FilesystemAdapter::new();
        let c = claim(
            "filesystem.file_exists",
            serde_json::json!({"path": "/this/path/never/exists/anywhere/xyz"}),
        );
        let r = adapter.fetch(&c).await.unwrap();
        assert!(!r.verified);
        assert_eq!(r.payload["exists"], false);
    }

    #[tokio::test]
    async fn file_exists_unverified_when_path_is_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let adapter = FilesystemAdapter::new();
        let c = claim(
            "filesystem.file_exists",
            serde_json::json!({"path": tmp.path().to_string_lossy()}),
        );
        let r = adapter.fetch(&c).await.unwrap();
        assert!(!r.verified, "directories are not files");
        assert_eq!(r.payload["exists"], true);
        assert_eq!(r.payload["is_file"], false);
    }

    #[tokio::test]
    async fn dir_exists_verified_when_dir_present() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hi").unwrap();
        std::fs::write(tmp.path().join("b.txt"), b"hi").unwrap();
        let adapter = FilesystemAdapter::new();
        let c = claim(
            "filesystem.dir_exists",
            serde_json::json!({"path": tmp.path().to_string_lossy()}),
        );
        let r = adapter.fetch(&c).await.unwrap();
        assert!(r.verified);
        assert_eq!(r.payload["is_dir"], true);
        assert_eq!(r.payload["entry_count"], 2);
    }

    #[tokio::test]
    async fn file_contains_hash_verified_when_match() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"sentinel").unwrap();
        let expected = sha256_of_file(tmp.path()).unwrap();
        let adapter = FilesystemAdapter::new();
        let c = claim(
            "filesystem.file_contains_hash",
            serde_json::json!({
                "path": tmp.path().to_string_lossy(),
                "expected_sha256": expected,
            }),
        );
        let r = adapter.fetch(&c).await.unwrap();
        assert!(r.verified);
        assert_eq!(r.payload["matches"], true);
        assert_eq!(r.payload["size_bytes"], 8);
    }

    #[tokio::test]
    async fn file_contains_hash_unverified_when_mismatch() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"sentinel").unwrap();
        let adapter = FilesystemAdapter::new();
        let c = claim(
            "filesystem.file_contains_hash",
            serde_json::json!({
                "path": tmp.path().to_string_lossy(),
                "expected_sha256": "00".repeat(32),
            }),
        );
        let r = adapter.fetch(&c).await.unwrap();
        assert!(!r.verified);
        assert_eq!(r.payload["matches"], false);
        assert_ne!(
            r.payload["actual_sha256"].as_str().unwrap(),
            r.payload["expected_sha256"].as_str().unwrap()
        );
    }

    #[tokio::test]
    async fn file_contains_hash_case_insensitive() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"x").unwrap();
        let lower = sha256_of_file(tmp.path()).unwrap();
        let upper = lower.to_uppercase();
        let adapter = FilesystemAdapter::new();
        let c = claim(
            "filesystem.file_contains_hash",
            serde_json::json!({
                "path": tmp.path().to_string_lossy(),
                "expected_sha256": upper,
            }),
        );
        let r = adapter.fetch(&c).await.unwrap();
        assert!(r.verified, "hash comparison must be case-insensitive");
    }

    #[tokio::test]
    async fn missing_path_field_surfaces_invalid_context() {
        let adapter = FilesystemAdapter::new();
        let c = claim("filesystem.file_exists", serde_json::json!({}));
        let err = adapter.fetch(&c).await.unwrap_err();
        assert!(matches!(err, AdapterError::Fetch(_)), "got {err:?}",);
    }

    #[tokio::test]
    async fn empty_path_rejected() {
        let adapter = FilesystemAdapter::new();
        let c = claim("filesystem.file_exists", serde_json::json!({"path": ""}));
        let err = adapter.fetch(&c).await.unwrap_err();
        assert!(matches!(err, AdapterError::Fetch(_)));
    }

    #[tokio::test]
    async fn unknown_verb_returns_fetch_error() {
        let adapter = FilesystemAdapter::new();
        let c = claim(
            "filesystem.frobnicate",
            serde_json::json!({"path": "/tmp/x"}),
        );
        let err = adapter.fetch(&c).await.unwrap_err();
        match err {
            AdapterError::Fetch(msg) => {
                assert!(msg.contains("frobnicate"));
                assert!(msg.contains("file_exists"));
            }
            other => panic!("expected Fetch error, got {other:?}"),
        }
    }

    #[test]
    fn supports_only_filesystem_prefix() {
        let adapter = FilesystemAdapter::new();
        assert!(adapter.supports("filesystem.file_exists"));
        assert!(adapter.supports("filesystem.frobnicate"));
        assert!(!adapter.supports("github.pr_merged"));
        assert!(!adapter.supports("file_exists"));
        assert!(!adapter.supports(""));
    }

    #[tokio::test]
    async fn provenance_hash_set_on_every_receipt() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"x").unwrap();
        let adapter = FilesystemAdapter::new();
        let c = claim(
            "filesystem.file_exists",
            serde_json::json!({"path": tmp.path().to_string_lossy()}),
        );
        let r = adapter.fetch(&c).await.unwrap();
        assert!(!r.provenance_hash.is_empty());
        assert!(r.verify_provenance(&c).is_ok());
    }

    #[tokio::test]
    async fn registry_dispatches_to_filesystem_adapter() {
        use sentinel_application::evidence_adapters::EvidenceAdapterRegistry;
        let mut reg = EvidenceAdapterRegistry::new();
        reg.register(Box::new(FilesystemAdapter::new()));
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"hi").unwrap();
        let c = claim(
            "filesystem.file_exists",
            serde_json::json!({"path": tmp.path().to_string_lossy()}),
        );
        let r = reg.fetch(&c).await.unwrap();
        assert_eq!(r.adapter_name, ADAPTER_NAME);
        assert!(r.verified);
    }
}
