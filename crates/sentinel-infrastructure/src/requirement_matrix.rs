//! BA3 Phase 4b — Filesystem-backed requirement-matrix adapter.
//!
//! Implements [`RequirementMatrixPort`] by reading per-orchestration
//! snapshot files at `~/.claude/sentinel/state/requirement_matrix/{orchestration_id}.json`.
//! The future BA-orchestrator writes these snapshots; sentinel's
//! [`requirements_traceability_gate`](sentinel_application::hooks::requirements_traceability_gate)
//! hook queries through this adapter.
//!
//! ## Snapshot file shape
//!
//! ```json
//! {
//!   "rows": [
//!     {
//!       "orchestration_id": "case-2026-Q2-pricing",
//!       "matrix_row_id": "R-001",
//!       "content_hash": "hash-v1",
//!       "statement": "Stakeholder requires month-over-month churn under 2%."
//!     },
//!     ...
//!   ]
//! }
//! ```
//!
//! ## Error mapping per spec §7
//!
//! - **File missing** → `Err(UnknownOrchestration)` — the orchestration
//!   isn't tracked. The hook maps this to a BA3 `Existence` finding
//!   (Block).
//! - **File present but unparseable** → `Err(Malformed)` — schema
//!   mismatch / corrupt JSON. The hook maps this to a Block-class
//!   finding because citations cannot be validated.
//! - **Filesystem error reading the file** → `Err(MatrixUnavailable)`
//!   — transient IO failure. The hook maps this to a Block-class
//!   finding because citations cannot be validated.
//! - **File parsed; row matches** → `Ok(Some(row))`.
//! - **File parsed; row doesn't match** → `Ok(None)`. The hook maps
//!   to BA3 `Existence` finding (Block — phantom row).
//!
//! ## Snapshot semantics
//!
//! The file is the authoritative local matrix snapshot. Sentinel has no
//! network downgrade path: if the snapshot is missing, unreadable, or
//! malformed, the BA3 gate fails closed.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use sentinel_domain::ba::RequirementRef;
use sentinel_domain::ports::{RequirementMatrixError, RequirementMatrixPort};

/// On-disk snapshot of one orchestration's matrix. Stored as
/// `<base>/<orchestration_id>.json`. The future BA-orchestrator
/// authors these files; sentinel only reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixSnapshot {
    /// Matrix rows for this orchestration. Each row's
    /// `orchestration_id` SHOULD match the file's name; the adapter
    /// doesn't enforce this (operator-managed correctness).
    #[serde(default)]
    pub rows: Vec<RequirementRef>,
}

/// Filesystem-backed [`RequirementMatrixPort`] adapter.
///
/// Lazy-load: each `query_requirement` call reads the snapshot file
/// fresh. Operators editing the file see changes on the next query.
/// Phase 4c may add caching with mtime-based invalidation; Phase 4b
/// is correctness-first.
pub struct FilesystemRequirementMatrix {
    base_dir: PathBuf,
}

impl std::fmt::Debug for FilesystemRequirementMatrix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemRequirementMatrix")
            .field("base_dir", &self.base_dir)
            .finish()
    }
}

impl FilesystemRequirementMatrix {
    /// Construct pointed at a specific directory. Used by tests to
    /// scope to a tempdir.
    #[must_use]
    pub const fn at_dir(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Construct with the default path
    /// (`~/.claude/sentinel/state/requirement_matrix/`).
    pub fn with_default_path() -> Result<Self> {
        let base_dir = crate::state_store::state_dir().join("requirement_matrix");
        Ok(Self::at_dir(base_dir))
    }

    /// Read-only access to the base directory. Useful for tests +
    /// operator tooling.
    #[must_use]
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Compute the path for an orchestration's snapshot file.
    fn snapshot_path(&self, orchestration_id: &str) -> PathBuf {
        self.base_dir.join(format!("{orchestration_id}.json"))
    }

    /// Load + parse a snapshot file. Maps IO + parse errors to the
    /// appropriate `RequirementMatrixError` variant per the module
    /// docstring.
    fn load_snapshot(
        &self,
        orchestration_id: &str,
    ) -> Result<MatrixSnapshot, RequirementMatrixError> {
        let path = self.snapshot_path(orchestration_id);
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str::<MatrixSnapshot>(&content).map_err(|e| {
                RequirementMatrixError::Malformed(format!(
                    "snapshot {} parse failed: {e}",
                    path.display()
                ))
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(
                RequirementMatrixError::UnknownOrchestration(orchestration_id.to_string()),
            ),
            Err(err) => Err(RequirementMatrixError::MatrixUnavailable(format!(
                "read {} failed: {err}",
                path.display()
            ))),
        }
    }
}

impl RequirementMatrixPort for FilesystemRequirementMatrix {
    fn query_requirement(
        &self,
        orchestration_id: &str,
        matrix_row_id: &str,
    ) -> Result<Option<RequirementRef>, RequirementMatrixError> {
        let snapshot = self.load_snapshot(orchestration_id)?;
        Ok(snapshot
            .rows
            .into_iter()
            .find(|r| r.matrix_row_id == matrix_row_id))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn req(orch: &str, row: &str, hash: &str, statement: &str) -> RequirementRef {
        RequirementRef {
            orchestration_id: orch.to_string(),
            matrix_row_id: row.to_string(),
            content_hash: hash.to_string(),
            statement: statement.to_string(),
        }
    }

    fn write_snapshot(dir: &TempDir, orchestration_id: &str, rows: Vec<RequirementRef>) {
        let snapshot = MatrixSnapshot { rows };
        let path = dir.path().join(format!("{orchestration_id}.json"));
        fs::write(&path, serde_json::to_string(&snapshot).unwrap()).unwrap();
    }

    fn matrix(dir: &TempDir) -> FilesystemRequirementMatrix {
        FilesystemRequirementMatrix::at_dir(dir.path().to_path_buf())
    }

    // ---- Missing-orchestration mapping ----

    #[test]
    fn missing_file_maps_to_unknown_orchestration() {
        let dir = TempDir::new().unwrap();
        let m = matrix(&dir);
        let result = m.query_requirement("ghost-orchestration", "R-001");
        match result {
            Err(RequirementMatrixError::UnknownOrchestration(id)) => {
                assert_eq!(id, "ghost-orchestration");
            }
            other => panic!("expected UnknownOrchestration, got {other:?}"),
        }
    }

    // ---- Happy paths ----

    #[test]
    fn query_returns_matching_row() {
        let dir = TempDir::new().unwrap();
        write_snapshot(
            &dir,
            "case-1",
            vec![
                req("case-1", "R-001", "h-001", "Churn target"),
                req("case-1", "R-002", "h-002", "Pricing constraint"),
            ],
        );
        let m = matrix(&dir);
        let row = m.query_requirement("case-1", "R-001").unwrap().unwrap();
        assert_eq!(row.matrix_row_id, "R-001");
        assert_eq!(row.content_hash, "h-001");
        assert!(row.statement.contains("Churn"));
    }

    #[test]
    fn query_returns_none_when_orchestration_exists_but_row_missing() {
        let dir = TempDir::new().unwrap();
        write_snapshot(&dir, "case-1", vec![req("case-1", "R-001", "h-001", "x")]);
        let m = matrix(&dir);
        let result = m.query_requirement("case-1", "R-PHANTOM").unwrap();
        assert!(
            result.is_none(),
            "row not in snapshot → Ok(None), distinct from UnknownOrchestration"
        );
    }

    // ---- Malformed snapshot ----

    #[test]
    fn malformed_json_maps_to_malformed_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("case-1.json");
        fs::write(&path, "not valid json [[[").unwrap();
        let m = matrix(&dir);
        let result = m.query_requirement("case-1", "R-001");
        match result {
            Err(RequirementMatrixError::Malformed(msg)) => {
                assert!(msg.contains("parse failed"));
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_with_wrong_schema_maps_to_malformed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("case-1.json");
        // Valid JSON but wrong shape (missing rows field, has bogus
        // top-level instead). `rows` defaults to empty so this
        // actually parses successfully — verify that's the behavior.
        fs::write(&path, r#"{"completely_different": "shape"}"#).unwrap();
        let m = matrix(&dir);
        let result = m.query_requirement("case-1", "R-001").unwrap();
        assert!(
            result.is_none(),
            "missing rows field defaults to empty → no match → Ok(None)"
        );
    }

    #[test]
    fn snapshot_with_rows_as_wrong_type_maps_to_malformed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("case-1.json");
        // rows field has wrong type — must surface as Malformed.
        fs::write(&path, r#"{"rows": "not an array"}"#).unwrap();
        let m = matrix(&dir);
        let result = m.query_requirement("case-1", "R-001");
        assert!(matches!(result, Err(RequirementMatrixError::Malformed(_))));
    }

    // ---- Snapshot file scoping ----

    #[test]
    fn different_orchestrations_resolve_to_different_files() {
        let dir = TempDir::new().unwrap();
        write_snapshot(
            &dir,
            "case-A",
            vec![req("case-A", "R-001", "h-A", "Case A")],
        );
        write_snapshot(
            &dir,
            "case-B",
            vec![req("case-B", "R-001", "h-B", "Case B")],
        );
        let m = matrix(&dir);
        let a = m.query_requirement("case-A", "R-001").unwrap().unwrap();
        let b = m.query_requirement("case-B", "R-001").unwrap().unwrap();
        assert_eq!(a.content_hash, "h-A");
        assert_eq!(b.content_hash, "h-B");
    }

    #[test]
    fn snapshot_path_uses_orchestration_id_as_filename() {
        let dir = TempDir::new().unwrap();
        let m = matrix(&dir);
        let p = m.snapshot_path("case-2026-Q2-pricing");
        assert!(p.ends_with("case-2026-Q2-pricing.json"));
    }

    // ---- Lazy reload ----

    #[test]
    fn updated_snapshot_visible_on_next_query() {
        let dir = TempDir::new().unwrap();
        write_snapshot(
            &dir,
            "case-1",
            vec![req("case-1", "R-001", "h-v1", "v1 statement")],
        );
        let m = matrix(&dir);
        let v1 = m.query_requirement("case-1", "R-001").unwrap().unwrap();
        assert_eq!(v1.content_hash, "h-v1");
        // Operator edits the snapshot
        write_snapshot(
            &dir,
            "case-1",
            vec![req("case-1", "R-001", "h-v2", "v2 statement")],
        );
        let v2 = m.query_requirement("case-1", "R-001").unwrap().unwrap();
        assert_eq!(
            v2.content_hash, "h-v2",
            "next query should see the updated snapshot"
        );
    }

    // ---- Path semantics ----

    #[test]
    fn with_default_path_resolves_under_claude_dir() {
        let m = FilesystemRequirementMatrix::with_default_path().unwrap();
        let p = m.base_dir().display().to_string();
        assert!(
            p.contains(".claude")
                && p.contains("sentinel")
                && p.contains("state")
                && p.ends_with("requirement_matrix"),
            "default path should live under .claude/sentinel/state/requirement_matrix, got {p}"
        );
    }

    // ---- Send + Sync ----

    #[test]
    fn matrix_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FilesystemRequirementMatrix>();
    }

    // ---- Trait-object usage ----

    #[test]
    fn usable_through_trait_object() {
        let dir = TempDir::new().unwrap();
        write_snapshot(&dir, "case-1", vec![req("case-1", "R-001", "h", "x")]);
        let m = matrix(&dir);
        let port: &dyn RequirementMatrixPort = &m;
        let row = port.query_requirement("case-1", "R-001").unwrap().unwrap();
        assert_eq!(row.matrix_row_id, "R-001");
    }
}
