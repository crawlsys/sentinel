//! Evidence adapter port + registry — THE BIBLE plumbing layer.
//!
//! The async use-case interface for [`evidence_adapter`](sentinel_domain::evidence_adapter)
//! framework. This is where adapters declare what claims they handle
//! and the registry dispatches a claim to the right adapter.
//!
//! Domain-layer types are pure data; this layer is async because real
//! adapters call out over HTTP (GitHub, Browserbase, Doppler). Async
//! at this level means infrastructure adapters can be sync OR async
//! by wrapping their work in an async fn — the trait doesn't care.
//!
//! # Adapter contract
//!
//! Each [`EvidenceAdapter`] declares:
//! - `name()` — stable identifier (e.g. `"github_api"`).
//! - `supports(claim_type)` — true when this adapter can produce a
//!   receipt for the given claim namespace.
//! - `fetch(claim)` — async; returns either a [`EvidenceReceipt`] or
//!   an [`AdapterError`].
//!
//! Multiple adapters may support the same claim type — the registry
//! returns the first match by registration order. Cross-vendor
//! verification (M3.3 multi-lens reviewer pattern) layers atop this
//! by registering N adapters for the same claim_type and asking the
//! registry for ALL receipts (not just the first); see
//! [`EvidenceAdapterRegistry::fetch_all`].
//!
//! # The self-attested fallback
//!
//! [`SelfAttestedAdapter`] is the catch-all. It "supports" every
//! claim type and returns a receipt with `verified=false`. This makes
//! "we know we don't know" a first-class chain entry rather than an
//! absence-of-data ambiguity. Corpus queries filter on
//! `verified == false` to find unverified claims and prioritize them
//! for real-adapter coverage. The self-attested adapter is registered
//! LAST so real adapters get first dibs.

use async_trait::async_trait;
use chrono::Utc;

use sentinel_domain::evidence_adapter::{AdapterError, EvidenceClaim, EvidenceReceipt};

/// Port the registry calls. Infrastructure adapters implement this
/// against their own external system.
#[async_trait]
pub trait EvidenceAdapter: Send + Sync {
    /// Stable adapter identifier — surfaced in `EvidenceReceipt::adapter_name`
    /// and used as part of the provenance hash.
    fn name(&self) -> &str;

    /// True when this adapter can produce a receipt for `claim_type`.
    /// Cheap; the registry calls it on every claim. Must be a pure
    /// function of the input string — adapters that vary support
    /// based on runtime state (e.g. "I support github.* unless my
    /// auth token is missing") should still return true here and
    /// fail loudly in `fetch` so the operator sees the auth error.
    fn supports(&self, claim_type: &str) -> bool;

    /// Fetch a receipt for the claim. Async because real adapters
    /// hit HTTP. Implementations MUST construct the receipt via
    /// [`EvidenceReceipt::new`] so the provenance hash is set
    /// correctly — calling code re-verifies via
    /// [`EvidenceReceipt::verify_provenance`] before persisting.
    async fn fetch(&self, claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError>;
}

/// Registry of adapters. Owns them via `Box<dyn EvidenceAdapter>`
/// because each adapter has its own state (HTTP client, credentials,
/// rate limiter) that doesn't fit a single concrete type.
pub struct EvidenceAdapterRegistry {
    adapters: Vec<Box<dyn EvidenceAdapter>>,
}

impl EvidenceAdapterRegistry {
    /// Empty registry. Most callers immediately register
    /// [`SelfAttestedAdapter`] then layer real adapters on top.
    #[must_use]
    pub fn new() -> Self {
        Self {
            adapters: Vec::new(),
        }
    }

    /// Build a registry pre-populated with the self-attested fallback.
    /// Convenience for tests + minimal production deployments.
    #[must_use]
    pub fn with_fallback() -> Self {
        let mut r = Self::new();
        r.register(Box::new(SelfAttestedAdapter::new()));
        r
    }

    /// Register an adapter. Order matters — the first adapter that
    /// `supports()` a claim handles it in [`fetch`]. Register real
    /// adapters BEFORE the self-attested fallback so the fallback
    /// only fires when nothing else applies.
    pub fn register(&mut self, adapter: Box<dyn EvidenceAdapter>) {
        self.adapters.push(adapter);
    }

    /// Number of registered adapters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.adapters.len()
    }

    /// True when no adapters are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    /// Adapter names in registration order. For diagnostics + the
    /// future dashboard "which adapters are wired up?" view.
    pub fn names(&self) -> Vec<&str> {
        self.adapters.iter().map(|a| a.name()).collect()
    }

    /// Fetch a receipt from the first adapter that supports the claim.
    ///
    /// Returns [`AdapterError::NoAdapterForClaim`] when nothing
    /// matches. With [`SelfAttestedAdapter`] registered (the common
    /// case), this never fires — the fallback supports everything.
    ///
    /// **Provenance verification** is performed against the receipt
    /// before return. A buggy adapter that builds the receipt with a
    /// stale provenance hash gets caught here, not on chain
    /// verification later.
    pub async fn fetch(&self, claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError> {
        for adapter in &self.adapters {
            if adapter.supports(&claim.claim_type) {
                let receipt = adapter.fetch(claim).await?;
                receipt.verify_provenance(claim)?;
                return Ok(receipt);
            }
        }
        Err(AdapterError::NoAdapterForClaim(claim.claim_type.clone()))
    }

    /// Fetch receipts from EVERY adapter that supports the claim.
    /// Used for cross-vendor verification — register e.g. GitHub
    /// API + Browserbase recording as both supporting `git.pr_opened`
    /// and get both receipts. Caller decides what "verified" means
    /// when multiple receipts disagree (M3.3 multi-lens pattern).
    ///
    /// Returns a vec of (adapter_name, Result<receipt, error>) so
    /// per-adapter failures don't mask other adapters' successes.
    /// Empty vec when no adapters support the claim type.
    pub async fn fetch_all(
        &self,
        claim: &EvidenceClaim,
    ) -> Vec<(String, Result<EvidenceReceipt, AdapterError>)> {
        let mut results = Vec::new();
        for adapter in &self.adapters {
            if adapter.supports(&claim.claim_type) {
                let result = match adapter.fetch(claim).await {
                    Ok(receipt) => match receipt.verify_provenance(claim) {
                        Ok(()) => Ok(receipt),
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                };
                results.push((adapter.name().to_string(), result));
            }
        }
        results
    }
}

impl Default for EvidenceAdapterRegistry {
    fn default() -> Self {
        Self::with_fallback()
    }
}

/// The catch-all adapter. Supports every claim type and returns a
/// receipt with `verified=false`. Makes "we know we don't know" a
/// first-class chain entry — corpus queries filter on
/// `verified == false` to find unverified claims and prioritize them
/// for real-adapter coverage.
///
/// Always register LAST so real adapters get first dibs.
pub struct SelfAttestedAdapter {
    name: String,
}

impl SelfAttestedAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            name: "self_attested".to_string(),
        }
    }

    /// Build with a custom name. Useful when an operator wants to
    /// distinguish "self-attested-but-from-trusted-context" (e.g.
    /// CI runner) from "self-attested-from-untrusted-context" so
    /// downstream queries can split on the name.
    #[must_use]
    pub fn with_name(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl Default for SelfAttestedAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EvidenceAdapter for SelfAttestedAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn supports(&self, _claim_type: &str) -> bool {
        true
    }

    async fn fetch(&self, claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError> {
        // Empty payload — nothing to attest, nothing to verify.
        // The receipt body still binds via the provenance hash so
        // the adapter identity + claim shape are recorded immutably.
        Ok(EvidenceReceipt::new(
            self.name.clone(),
            claim,
            false,
            serde_json::json!({
                "note": "no third-party verification available for this claim",
            }),
            Utc::now(),
        ))
    }
}

#[cfg(test)]
pub mod testing {
    //! Test doubles — a deterministic adapter that returns canned
    //! receipts and a recording adapter that captures every fetch
    //! call so tests can pin which adapters fired in which order.

    use super::*;
    use std::sync::Mutex;

    /// Returns the canned payload for any claim it supports. The
    /// `claim_types` set controls `supports()`. Useful for "given a
    /// happy path, the registry returns what GitHub would return."
    pub struct StubAdapter {
        pub name: String,
        pub claim_types: Vec<String>,
        pub verified: bool,
        pub payload: serde_json::Value,
    }

    impl StubAdapter {
        pub fn new(
            name: impl Into<String>,
            claim_types: Vec<String>,
            verified: bool,
            payload: serde_json::Value,
        ) -> Self {
            Self {
                name: name.into(),
                claim_types,
                verified,
                payload,
            }
        }
    }

    #[async_trait]
    impl EvidenceAdapter for StubAdapter {
        fn name(&self) -> &str {
            &self.name
        }

        fn supports(&self, claim_type: &str) -> bool {
            self.claim_types.iter().any(|t| t == claim_type)
        }

        async fn fetch(&self, claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError> {
            Ok(EvidenceReceipt::new(
                self.name.clone(),
                claim,
                self.verified,
                self.payload.clone(),
                Utc::now(),
            ))
        }
    }

    /// Always returns the supplied error. Pin "what happens when
    /// GitHub returns 503" without standing up a flaky HTTP fixture.
    pub struct FailingAdapter {
        pub name: String,
        pub claim_types: Vec<String>,
        pub error_message: String,
    }

    #[async_trait]
    impl EvidenceAdapter for FailingAdapter {
        fn name(&self) -> &str {
            &self.name
        }

        fn supports(&self, claim_type: &str) -> bool {
            self.claim_types.iter().any(|t| t == claim_type)
        }

        async fn fetch(&self, _claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError> {
            Err(AdapterError::Fetch(self.error_message.clone()))
        }
    }

    /// Records every `fetch` call. Tests inspect `.calls()` to
    /// assert which claim types reached the adapter.
    pub struct RecordingAdapter {
        pub name: String,
        pub claim_types: Vec<String>,
        calls: Mutex<Vec<EvidenceClaim>>,
    }

    impl RecordingAdapter {
        pub fn new(name: impl Into<String>, claim_types: Vec<String>) -> Self {
            Self {
                name: name.into(),
                claim_types,
                calls: Mutex::new(Vec::new()),
            }
        }

        pub fn calls(&self) -> Vec<EvidenceClaim> {
            self.calls.lock().expect("calls lock poisoned").clone()
        }
    }

    #[async_trait]
    impl EvidenceAdapter for RecordingAdapter {
        fn name(&self) -> &str {
            &self.name
        }

        fn supports(&self, claim_type: &str) -> bool {
            self.claim_types.iter().any(|t| t == claim_type)
        }

        async fn fetch(&self, claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError> {
            self.calls
                .lock()
                .expect("calls lock poisoned")
                .push(claim.clone());
            Ok(EvidenceReceipt::new(
                self.name.clone(),
                claim,
                true,
                serde_json::json!({ "recorded": true }),
                Utc::now(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn sample_claim(claim_type: &str) -> EvidenceClaim {
        EvidenceClaim {
            skill: "git".into(),
            phase_id: "open_pr".into(),
            step_id: "1".into(),
            claim_type: claim_type.into(),
            context: serde_json::json!({ "pr_number": 42 }),
        }
    }

    // ─── SelfAttestedAdapter ─────────────────────────────────────────

    #[tokio::test]
    async fn self_attested_supports_every_claim_type() {
        let a = SelfAttestedAdapter::new();
        assert!(a.supports("git.pr_opened"));
        assert!(a.supports("anything.at.all"));
        assert!(a.supports(""));
    }

    #[tokio::test]
    async fn self_attested_returns_unverified_receipt_with_valid_provenance() {
        let a = SelfAttestedAdapter::new();
        let claim = sample_claim("git.pr_opened");
        let receipt = a.fetch(&claim).await.unwrap();
        assert!(!receipt.verified);
        assert_eq!(receipt.adapter_name, "self_attested");
        assert!(receipt.verify_provenance(&claim).is_ok());
    }

    #[tokio::test]
    async fn self_attested_with_custom_name_uses_that_name() {
        // Operators distinguish trusted vs untrusted self-attestation
        // by giving the adapter a more specific name.
        let a = SelfAttestedAdapter::with_name("ci_self_attested");
        assert_eq!(a.name(), "ci_self_attested");
        let claim = sample_claim("anything");
        let receipt = a.fetch(&claim).await.unwrap();
        assert_eq!(receipt.adapter_name, "ci_self_attested");
    }

    // ─── EvidenceAdapterRegistry ─────────────────────────────────────

    #[tokio::test]
    async fn empty_registry_returns_no_adapter_error() {
        let r = EvidenceAdapterRegistry::new();
        let claim = sample_claim("git.pr_opened");
        let err = r.fetch(&claim).await.unwrap_err();
        match err {
            AdapterError::NoAdapterForClaim(t) => assert_eq!(t, "git.pr_opened"),
            other => panic!("expected NoAdapterForClaim, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn with_fallback_handles_any_claim_via_self_attested() {
        let r = EvidenceAdapterRegistry::with_fallback();
        let claim = sample_claim("totally.made.up.claim");
        let receipt = r.fetch(&claim).await.unwrap();
        assert!(!receipt.verified);
        assert_eq!(receipt.adapter_name, "self_attested");
    }

    #[tokio::test]
    async fn registry_dispatches_to_first_supporting_adapter() {
        // Real adapter for git.pr_opened; self-attested fallback
        // last. Registry must hit the real adapter, not the fallback.
        let mut r = EvidenceAdapterRegistry::new();
        r.register(Box::new(StubAdapter::new(
            "github_api",
            vec!["git.pr_opened".into()],
            true,
            serde_json::json!({ "url": "real" }),
        )));
        r.register(Box::new(SelfAttestedAdapter::new()));

        let receipt = r.fetch(&sample_claim("git.pr_opened")).await.unwrap();
        assert_eq!(receipt.adapter_name, "github_api");
        assert!(receipt.verified);
    }

    #[tokio::test]
    async fn registry_falls_through_to_self_attested_when_real_adapter_doesnt_support() {
        let mut r = EvidenceAdapterRegistry::new();
        r.register(Box::new(StubAdapter::new(
            "github_api",
            vec!["git.pr_opened".into()], // doesn't support linear.*
            true,
            serde_json::json!({}),
        )));
        r.register(Box::new(SelfAttestedAdapter::new()));

        let receipt = r.fetch(&sample_claim("linear.transition")).await.unwrap();
        assert_eq!(receipt.adapter_name, "self_attested");
        assert!(!receipt.verified);
    }

    #[tokio::test]
    async fn registry_propagates_adapter_fetch_errors() {
        let mut r = EvidenceAdapterRegistry::new();
        r.register(Box::new(FailingAdapter {
            name: "github_api".into(),
            claim_types: vec!["git.pr_opened".into()],
            error_message: "503 Service Unavailable".into(),
        }));

        let err = r.fetch(&sample_claim("git.pr_opened")).await.unwrap_err();
        match err {
            AdapterError::Fetch(s) => assert!(s.contains("503")),
            other => panic!("expected Fetch error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn registry_does_not_fall_through_after_real_adapter_fails() {
        // Critical: when GitHub returns 503, we surface the failure.
        // We do NOT silently fall through to the self-attested
        // fallback — that would mask real outages and let agents
        // claim verification that didn't happen.
        let mut r = EvidenceAdapterRegistry::new();
        r.register(Box::new(FailingAdapter {
            name: "github_api".into(),
            claim_types: vec!["git.pr_opened".into()],
            error_message: "503".into(),
        }));
        r.register(Box::new(SelfAttestedAdapter::new()));

        let result = r.fetch(&sample_claim("git.pr_opened")).await;
        assert!(matches!(result, Err(AdapterError::Fetch(_))));
    }

    // ─── fetch_all (cross-vendor verification) ───────────────────────

    #[tokio::test]
    async fn fetch_all_returns_receipts_from_every_supporting_adapter() {
        // Two adapters both support git.pr_opened — we want both
        // receipts so the caller can do cross-vendor verification.
        let mut r = EvidenceAdapterRegistry::new();
        r.register(Box::new(StubAdapter::new(
            "github_api",
            vec!["git.pr_opened".into()],
            true,
            serde_json::json!({ "from": "github" }),
        )));
        r.register(Box::new(StubAdapter::new(
            "browserbase",
            vec!["git.pr_opened".into()],
            true,
            serde_json::json!({ "from": "browserbase" }),
        )));

        let results = r.fetch_all(&sample_claim("git.pr_opened")).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "github_api");
        assert_eq!(results[1].0, "browserbase");
        assert!(results[0].1.is_ok());
        assert!(results[1].1.is_ok());
    }

    #[tokio::test]
    async fn fetch_all_returns_empty_when_no_adapter_supports_claim() {
        let r = EvidenceAdapterRegistry::new();
        let results = r.fetch_all(&sample_claim("nothing")).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn fetch_all_returns_per_adapter_results_when_some_fail() {
        // Cross-vendor: one adapter succeeds, one fails. The caller
        // gets both results so partial-failure can be policy-decided
        // (e.g. "1 of 2 verifications passed → still verified").
        let mut r = EvidenceAdapterRegistry::new();
        r.register(Box::new(StubAdapter::new(
            "github_api",
            vec!["git.pr_opened".into()],
            true,
            serde_json::json!({ "ok": true }),
        )));
        r.register(Box::new(FailingAdapter {
            name: "browserbase".into(),
            claim_types: vec!["git.pr_opened".into()],
            error_message: "timeout".into(),
        }));

        let results = r.fetch_all(&sample_claim("git.pr_opened")).await;
        assert_eq!(results.len(), 2);
        assert!(results[0].1.is_ok());
        assert!(results[1].1.is_err());
    }

    // ─── RecordingAdapter ────────────────────────────────────────────

    #[tokio::test]
    async fn recording_adapter_captures_each_fetch_call() {
        let recorder = RecordingAdapter::new("recorder", vec!["git.pr_opened".into()]);

        // Fan-out two distinct claims through fetch.
        let _ = recorder.fetch(&sample_claim("git.pr_opened")).await;
        let mut other = sample_claim("git.pr_opened");
        other.context = serde_json::json!({ "pr_number": 99 });
        let _ = recorder.fetch(&other).await;

        let calls = recorder.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].context["pr_number"], 42);
        assert_eq!(calls[1].context["pr_number"], 99);
    }

    // ─── Diagnostics + accessors ─────────────────────────────────────

    #[test]
    fn registry_names_lists_adapters_in_registration_order() {
        let mut r = EvidenceAdapterRegistry::new();
        r.register(Box::new(StubAdapter::new(
            "a",
            vec![],
            false,
            serde_json::json!({}),
        )));
        r.register(Box::new(StubAdapter::new(
            "b",
            vec![],
            false,
            serde_json::json!({}),
        )));
        r.register(Box::new(SelfAttestedAdapter::new()));
        assert_eq!(r.names(), vec!["a", "b", "self_attested"]);
    }

    #[test]
    fn registry_len_and_is_empty_track_size() {
        let mut r = EvidenceAdapterRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        r.register(Box::new(SelfAttestedAdapter::new()));
        assert!(!r.is_empty());
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn default_registry_includes_fallback() {
        let r = EvidenceAdapterRegistry::default();
        assert_eq!(r.len(), 1);
        assert_eq!(r.names()[0], "self_attested");
    }
}
