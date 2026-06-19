//! Memory Verify Hook — verify stored memories against ground truth on `PreCompact`.
//!
//! Runs on `PreCompact` with a 24h cooldown. Scrolls Qdrant for memories not
//! verified in the last 7 days, extracts claims through the configured
//! OpenRouter-backed `LlmPort`, verifies `file_path` claims with `fs::exists()`,
//! and updates Qdrant payloads.
//!
//! ## Why `PreCompact`, not `SessionStart`
//!
//! Originally lived on `SessionStart`, but Qdrant scroll + per-memory LLM claim
//! extraction blocked startup for 5–20s on a cold cache. Moved to `PreCompact`
//! (background, non-critical path) so latency doesn't affect user-perceived
//! session readiness. The 24h cooldown means a long session typically verifies
//! once, on its first compaction event of the day.
//!
//! All network calls still run inside `run_async()` which enforces a 3-second
//! wall-clock timeout as a defence in depth.

use chrono::Utc;
use sentinel_domain::constants;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sentinel_domain::ports::{LlmModel, LlmPort, LlmRequest, VectorStorePort};
use std::path::PathBuf;
use tracing::{debug, warn};

use super::{run_async, FileSystemPort, HookContext};

/// Maximum memories to verify per session to limit API calls.
const MAX_VERIFY_PER_SESSION: usize = constants::MAX_VERIFY_PER_SESSION;

/// Memories not verified in this many days are eligible for re-verification.
const VERIFY_STALE_DAYS: i64 = constants::VERIFY_STALE_DAYS;

/// Qdrant collection shared with `memory_inject` / `memory_extract` / `memory_feedback`.
const COLLECTION: &str = "claude-memory";

/// 24h cooldown file path (via `FileSystemPort`).
fn cooldown_path(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    fs.home_dir().map(|h| {
        h.join(".claude")
            .join("sentinel")
            .join("state")
            .join("last-verify.txt")
    })
}

/// Check if 24h cooldown has elapsed.
fn check_cooldown(fs: &dyn FileSystemPort) -> bool {
    let path = match cooldown_path(fs) {
        Some(p) => p,
        None => return true,
    };
    let content = match fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return true, // No file = never run
    };
    let ts = match chrono::DateTime::parse_from_rfc3339(content.trim()) {
        Ok(t) => t.with_timezone(&Utc),
        Err(_) => return true,
    };
    let hours_elapsed = (Utc::now() - ts).num_hours();
    hours_elapsed >= 24
}

/// Write cooldown timestamp.
fn write_cooldown(fs: &dyn FileSystemPort) {
    let path = match cooldown_path(fs) {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = fs.create_dir_all(parent);
    }
    let _ = fs.write(&path, Utc::now().to_rfc3339().as_bytes());
}

/// A memory point from Qdrant scroll.
struct MemoryPoint {
    id: String,
    name: String,
    content: String,
    #[allow(dead_code)]
    last_verified_at: Option<String>,
}

struct VerificationRun {
    stale_count: usize,
    incomplete_reason: Option<String>,
}

impl VerificationRun {
    fn complete(stale_count: usize) -> Self {
        Self {
            stale_count,
            incomplete_reason: None,
        }
    }

    fn incomplete(reason: impl Into<String>) -> Self {
        Self {
            stale_count: 0,
            incomplete_reason: Some(reason.into()),
        }
    }
}

impl Default for VerificationRun {
    fn default() -> Self {
        Self::incomplete("memory verification did not complete within the hook timeout")
    }
}

/// A mechanically verifiable claim extracted by the configured LLM.
#[derive(Debug, serde::Deserialize)]
struct Claim {
    claim_type: String,
    #[allow(dead_code)]
    claim_text: String,
    verifiable_value: String,
}

/// Scroll Qdrant for memories not verified in the last N days (via `VectorStorePort`).
async fn scroll_unverified(vector_store: &dyn VectorStorePort) -> Result<Vec<MemoryPoint>, String> {
    let results = match vector_store.scroll(COLLECTION, None, 100).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Failed to scroll Qdrant");
            return Err(format!("Qdrant scroll failed: {e}"));
        }
    };

    let cutoff = Utc::now() - chrono::Duration::days(VERIFY_STALE_DAYS);

    Ok(results
        .into_iter()
        .filter_map(|p| {
            let id = p.id;
            let payload = &p.payload;
            let name = payload
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unnamed")
                .to_string();
            let content = payload
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let last_verified = payload
                .get("last_verified_at")
                .and_then(|v| v.as_str())
                .map(String::from);

            let needs_verify = match &last_verified {
                Some(ts) => match chrono::DateTime::parse_from_rfc3339(ts) {
                    Ok(dt) => dt.with_timezone(&Utc) < cutoff,
                    Err(_) => true,
                },
                None => true,
            };

            if !needs_verify {
                return None;
            }

            Some(MemoryPoint {
                id,
                name,
                content,
                last_verified_at: last_verified,
            })
        })
        .take(MAX_VERIFY_PER_SESSION)
        .collect())
}

/// Extract claims from content using the configured LLM port.
async fn extract_claims(llm: &dyn LlmPort, content: &str) -> Result<Vec<Claim>, String> {
    let prompt = format!(
        r#"Extract verifiable claims from this text. Return a JSON array of objects with:
- "claim_type": one of "file_path", "url", "port", "linear_issue", "version", "count", "status"
- "claim_text": what the text claims (short)
- "verifiable_value": the specific checkable value (the path, URL, port number, issue ID, etc)

Only extract claims that can be mechanically verified. Skip opinions, descriptions, and subjective statements.

Text:
{content}

Return ONLY the JSON array, no other text."#
    );

    let text = match llm
        .complete(LlmRequest {
            model: LlmModel::Haiku,
            prompt,
            max_tokens: 2000,
        })
        .await
    {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "LLM claim extraction failed");
            return Err(format!("claim extraction failed: {e}"));
        }
    };

    // Strip markdown code fences (the model sometimes wraps JSON in ```json ... ```).
    let cleaned = text.trim();
    let cleaned = if cleaned.starts_with("```") {
        let inner = cleaned
            .trim_start_matches("```json")
            .trim_start_matches("```");
        inner.trim_end_matches("```").trim()
    } else {
        cleaned
    };

    serde_json::from_str(cleaned).map_err(|e| {
        debug!(error = %e, "Failed to parse claims from LLM");
        format!("claim extraction returned malformed JSON: {e}")
    })
}

/// Verify `file_path` claims with `fs.exists()` via `FileSystemPort`.
/// Returns (verified, `stale_reasons`).
fn verify_claims(fs: &dyn FileSystemPort, claims: &[Claim]) -> (bool, Vec<String>) {
    let mut stale_reasons = Vec::new();
    let mut any_stale = false;

    for claim in claims {
        if claim.claim_type != "file_path" {
            continue; // Only verify file paths in the hook (fast + no network)
        }

        let path = &claim.verifiable_value;

        // Try absolute path
        if fs.exists(std::path::Path::new(path)) {
            continue;
        }

        // Try expanding ~
        if path.starts_with("~/") || path.starts_with("~\\") {
            if let Some(home) = fs.home_dir() {
                let expanded = home.join(&path[2..]);
                if fs.exists(&expanded) {
                    continue;
                }
            }
        }

        // File not found = stale
        any_stale = true;
        stale_reasons.push(format!("File not found: {path}"));
    }

    (!any_stale || stale_reasons.is_empty(), stale_reasons)
}

/// Update Qdrant payload with verification results (via `VectorStorePort`).
async fn update_payload(
    vector_store: &dyn VectorStorePort,
    point_id: &str,
    verified: bool,
    stale_reason: Option<&str>,
) {
    let now = Utc::now().to_rfc3339();

    let reason_str = stale_reason.unwrap_or("").to_string();
    let payload = serde_json::json!({
        "verified": verified,
        "last_verified_at": now,
        "stale_reason": reason_str,
    });

    let ids = [point_id.to_string()];
    if let Err(e) = vector_store.set_payload(COLLECTION, &ids, payload).await {
        debug!(error = %e, "memory_verify set_payload failed");
    }
}

/// Process `PreCompact` — verify stale memories against ground truth.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    // 1. Check 24h cooldown
    if !check_cooldown(ctx.fs) {
        debug!("Memory verify cooldown active — skipping");
        return HookOutput::allow();
    }

    // 2. Require a configured vector store (Qdrant config now owned by the
    //    infrastructure adapter — no more local ~/.qdrant/config.json read).
    let vector_store = if let Some(vs) = ctx.vector_store {
        vs
    } else {
        debug!("No vector store configured — skipping memory verify");
        return HookOutput::allow();
    };

    // 3. LLM port — required for claim extraction.
    let llm = if let Some(l) = ctx.llm {
        l
    } else {
        let msg =
            "[Qdrant Memory] memory verification skipped: OpenRouter LLM port is not configured";
        debug!("{msg}");
        return HookOutput::inject_context(HookEvent::PreCompact, msg);
    };

    let fs = ctx.fs;

    // 4. Run async verification.
    let verification = run_async(async {
        let memories = match scroll_unverified(vector_store).await {
            Ok(memories) => memories,
            Err(reason) => return VerificationRun::incomplete(reason),
        };
        if memories.is_empty() {
            debug!("No memories need verification");
            return VerificationRun::complete(0);
        }

        debug!(count = memories.len(), "Verifying memories");

        let mut stale = 0usize;

        for memory in &memories {
            let claims = match extract_claims(llm, &memory.content).await {
                Ok(claims) => claims,
                Err(reason) => {
                    update_payload(vector_store, &memory.id, false, Some(&reason)).await;
                    stale += 1;
                    debug!(name = %memory.name, reason = %reason, "Memory verification failed closed");
                    continue;
                }
            };

            if claims.is_empty() {
                // No claims — mark as verified (nothing to disprove)
                update_payload(vector_store, &memory.id, true, None).await;
                continue;
            }

            let (all_ok, reasons) = verify_claims(fs, &claims);

            if all_ok {
                update_payload(vector_store, &memory.id, true, None).await;
            } else {
                let reason = reasons.join("; ");
                update_payload(vector_store, &memory.id, false, Some(&reason)).await;
                stale += 1;
                debug!(name = %memory.name, reason = %reason, "Memory flagged as stale");
            }
        }

        VerificationRun::complete(stale)
    });

    let stale_count = verification.stale_count;

    // 5. Write cooldown only after the verification run actually completed.
    if let Some(reason) = verification.incomplete_reason {
        let msg = format!("[Qdrant Memory] memory verification incomplete: {reason}");
        return HookOutput::inject_context(HookEvent::PreCompact, &msg);
    }

    write_cooldown(ctx.fs);

    // 6. Inject context if stale memories found
    if stale_count > 0 {
        let msg = format!("[Qdrant Memory] {stale_count} memories flagged as potentially stale");
        let _ = input; // suppress unused warning
        return HookOutput::inject_context(HookEvent::PreCompact, &msg);
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::port_errors::{FileSystemError, LlmError, VectorStoreError};
    use sentinel_domain::ports::{LlmRequest, VectorPoint, VectorScrollResult};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    struct StubLlm {
        result: Result<String, LlmError>,
    }

    #[async_trait::async_trait]
    impl LlmPort for StubLlm {
        async fn complete(&self, _request: LlmRequest) -> Result<String, LlmError> {
            self.result.clone()
        }
    }

    struct TempHomeFs {
        home: PathBuf,
    }

    impl TempHomeFs {
        fn new(home: PathBuf) -> Self {
            Self { home }
        }
    }

    impl FileSystemPort for TempHomeFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }

        fn read_to_string(&self, path: &Path) -> Result<String, FileSystemError> {
            Ok(std::fs::read_to_string(path)?)
        }

        fn write(&self, path: &Path, content: &[u8]) -> Result<(), FileSystemError> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Ok(std::fs::write(path, content)?)
        }

        fn create_dir_all(&self, path: &Path) -> Result<(), FileSystemError> {
            Ok(std::fs::create_dir_all(path)?)
        }

        fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>, FileSystemError> {
            Ok(std::fs::read_dir(path)?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect())
        }

        fn exists(&self, path: &Path) -> bool {
            path.exists()
        }

        fn is_dir(&self, path: &Path) -> bool {
            path.is_dir()
        }

        fn metadata(&self, path: &Path) -> Result<std::fs::Metadata, FileSystemError> {
            Ok(std::fs::metadata(path)?)
        }

        fn append(&self, path: &Path, content: &[u8]) -> Result<(), FileSystemError> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            file.write_all(content)?;
            Ok(())
        }
    }

    struct CapturingVectorStore {
        scroll_result: Result<Vec<VectorScrollResult>, VectorStoreError>,
        payloads: Mutex<Vec<(Vec<String>, serde_json::Value)>>,
    }

    impl CapturingVectorStore {
        fn with_points(points: Vec<VectorScrollResult>) -> Self {
            Self {
                scroll_result: Ok(points),
                payloads: Mutex::new(Vec::new()),
            }
        }

        fn with_scroll_error(error: VectorStoreError) -> Self {
            Self {
                scroll_result: Err(error),
                payloads: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl VectorStorePort for CapturingVectorStore {
        async fn upsert_points(
            &self,
            _collection: &str,
            _points: Vec<VectorPoint>,
        ) -> Result<(), VectorStoreError> {
            Ok(())
        }

        async fn scroll(
            &self,
            _collection: &str,
            _filter: Option<serde_json::Value>,
            _limit: u32,
        ) -> Result<Vec<VectorScrollResult>, VectorStoreError> {
            self.scroll_result.clone()
        }

        async fn set_payload(
            &self,
            _collection: &str,
            point_ids: &[String],
            payload: serde_json::Value,
        ) -> Result<(), VectorStoreError> {
            self.payloads
                .lock()
                .expect("payload lock")
                .push((point_ids.to_vec(), payload));
            Ok(())
        }
    }

    #[tokio::test]
    async fn extract_claims_returns_error_for_malformed_json() {
        let llm = StubLlm {
            result: Ok("not json".to_string()),
        };

        let err = extract_claims(&llm, "memory").await.unwrap_err();

        assert!(err.contains("malformed JSON"), "got: {err}");
    }

    #[tokio::test]
    async fn extract_claims_distinguishes_valid_empty_claim_set() {
        let llm = StubLlm {
            result: Ok("[]".to_string()),
        };

        let claims = extract_claims(&llm, "subjective note").await.unwrap();

        assert!(claims.is_empty());
    }

    #[tokio::test]
    async fn extract_claims_returns_error_for_llm_failure() {
        let llm = StubLlm {
            result: Err(LlmError::Unavailable("no route".to_string())),
        };

        let err = extract_claims(&llm, "memory").await.unwrap_err();

        assert!(err.contains("claim extraction failed"), "got: {err}");
    }

    #[test]
    fn process_marks_memory_unverified_when_extraction_is_malformed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let fs = TempHomeFs::new(temp.path().to_path_buf());
        let vector_store = CapturingVectorStore::with_points(vec![VectorScrollResult {
            id: "memory-1".to_string(),
            payload: serde_json::json!({
                "name": "repo fact",
                "content": "The build file is at /definitely/not/json."
            }),
        }]);
        let llm = StubLlm {
            result: Ok("not json".to_string()),
        };
        let git = crate::hooks::test_support::StubGit;
        let process_port = crate::hooks::test_support::StubProcess;
        let memory_mcp = crate::hooks::test_support::StubMemoryMcp;
        let env = crate::hooks::test_support::StubEnv::new();
        let ctx = HookContext {
            git: &git,
            vector_store: Some(&vector_store),
            fs: &fs,
            process: &process_port,
            llm: Some(&llm),
            memory_mcp: &memory_mcp,
            env: &env,
            linear_lookup: None,
        };

        let output = process(&HookInput::default(), &ctx);

        let updates = vector_store.payloads.lock().expect("payload lock");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, vec!["memory-1".to_string()]);
        assert_eq!(updates[0].1["verified"], false);
        assert!(updates[0].1["last_verified_at"].as_str().is_some());
        let reason = updates[0].1["stale_reason"].as_str().unwrap_or_default();
        assert!(reason.contains("malformed JSON"), "got: {reason}");

        let context = output
            .hook_specific_output
            .and_then(|hso| hso.additional_context)
            .unwrap_or_default();
        assert!(context.contains("1 memories flagged"), "got: {context}");
    }

    #[test]
    fn process_injects_context_when_llm_port_is_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let fs = TempHomeFs::new(temp.path().to_path_buf());
        let vector_store = CapturingVectorStore::with_points(vec![]);
        let git = crate::hooks::test_support::StubGit;
        let process_port = crate::hooks::test_support::StubProcess;
        let memory_mcp = crate::hooks::test_support::StubMemoryMcp;
        let env = crate::hooks::test_support::StubEnv::new();
        let ctx = HookContext {
            git: &git,
            vector_store: Some(&vector_store),
            fs: &fs,
            process: &process_port,
            llm: None,
            memory_mcp: &memory_mcp,
            env: &env,
            linear_lookup: None,
        };

        let output = process(&HookInput::default(), &ctx);

        let context = output
            .hook_specific_output
            .and_then(|hso| hso.additional_context)
            .unwrap_or_default();
        assert!(
            context.contains("OpenRouter LLM port is not configured"),
            "got: {context}"
        );
    }

    #[test]
    fn process_reports_incomplete_when_qdrant_scroll_fails() {
        let temp = tempfile::tempdir().expect("tempdir");
        let fs = TempHomeFs::new(temp.path().to_path_buf());
        let vector_store =
            CapturingVectorStore::with_scroll_error(VectorStoreError::Backend("boom".to_string()));
        let llm = StubLlm {
            result: Ok("[]".to_string()),
        };
        let git = crate::hooks::test_support::StubGit;
        let process_port = crate::hooks::test_support::StubProcess;
        let memory_mcp = crate::hooks::test_support::StubMemoryMcp;
        let env = crate::hooks::test_support::StubEnv::new();
        let ctx = HookContext {
            git: &git,
            vector_store: Some(&vector_store),
            fs: &fs,
            process: &process_port,
            llm: Some(&llm),
            memory_mcp: &memory_mcp,
            env: &env,
            linear_lookup: None,
        };

        let output = process(&HookInput::default(), &ctx);

        let context = output
            .hook_specific_output
            .and_then(|hso| hso.additional_context)
            .unwrap_or_default();
        assert!(
            context.contains("memory verification incomplete"),
            "got: {context}"
        );
        assert!(context.contains("Qdrant scroll failed"), "got: {context}");
        assert!(
            !cooldown_path(&fs).is_some_and(|path| fs.exists(&path)),
            "incomplete verification must not write cooldown"
        );
    }
}
