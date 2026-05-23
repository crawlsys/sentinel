//! Universal evidence adapter framework — THE BIBLE.
//!
//! The load-bearing claim of this whole architecture: **every step claim
//! requires external evidence or it didn't happen.** Today's `Evidence.custom`
//! is self-attestation — the agent saying "trust me, I did the thing." Real
//! proof needs third-party receipts:
//!
//! - A Browserbase session recording confirming a UI action.
//! - A GitHub API response confirming a PR opened.
//! - A Doppler audit-log entry confirming a secret rotated.
//! - A Linear webhook confirming a state transition.
//! - A Vercel deployment confirming the code reached production.
//!
//! The framework: a pluggable [`EvidenceAdapter`](crate::evidence_adapter)
//! port (the trait lives in `sentinel-application` because it's an async
//! use-case interface). Each adapter knows how to **fetch and validate**
//! external evidence for a given claim type. Step execution doesn't compose
//! evidence — it asks the adapter registry: "I'm claiming `git.pr_opened`;
//! here's my context; produce a receipt." The adapter calls out to where
//! the truth lives and returns an [`EvidenceReceipt`] that gets folded into
//! the StepProof's `Evidence.custom`.
//!
//! # The provenance hash
//!
//! Every receipt carries a `provenance_hash` derived from
//! `(adapter_name || claim_type || claim_context_hash || payload_hash)`.
//! This binds a receipt to:
//! - **Which adapter produced it** (so swapping a real adapter for a stub
//!   becomes detectable in the chain).
//! - **What was claimed** (so a receipt for "I opened PR #5" can't be
//!   replayed as a receipt for "I opened PR #7").
//! - **What payload was returned** (so the receipt body can't be edited
//!   post-hoc without invalidating the binding).
//!
//! Verifiers re-derive the hash and compare. Mismatch = rejected.
//!
//! # Why pure-domain
//!
//! No I/O, no async, no HTTP. The data shapes + the provenance-hash
//! function are pure. The async fetch trait lives one layer up so
//! infrastructure adapters (GitHub, Browserbase, etc.) plug in without
//! the domain crate gaining transitive deps on `reqwest` / `tokio`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A claim a step is making about something it did. Adapters consume
/// this to decide whether they can produce a receipt and to scope the
/// external lookup.
///
/// `claim_type` is the namespace key adapters dispatch on (e.g.
/// `"git.pr_opened"`, `"linear.transition"`, `"deploy.vercel.ready"`).
/// String identity rather than an enum because adapters live in
/// separate crates / repos and can't share a closed set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceClaim {
    /// Skill emitting the claim (e.g. `"git"`, `"linear"`).
    pub skill: String,

    /// Phase the step ran under.
    pub phase_id: String,

    /// Step ID within the phase.
    pub step_id: String,

    /// Claim namespace — the dispatch key for adapter selection.
    /// Convention: `"<skill_or_service>.<verb_or_event>"`.
    pub claim_type: String,

    /// Free-form structured context the adapter needs to find the
    /// receipt. Examples:
    /// - GitHub: `{ "owner": "...", "repo": "...", "pr_number": 42 }`
    /// - Browserbase: `{ "session_id": "..." }`
    /// - Doppler: `{ "project": "...", "config": "stg", "secret": "..." }`
    ///
    /// Adapters are responsible for validating the shape they expect;
    /// the framework treats it as opaque.
    #[serde(default)]
    pub context: serde_json::Value,
}

impl EvidenceClaim {
    /// Build the canonical hash of this claim's context. Used by the
    /// provenance hash so the receipt binds to the claim shape, not
    /// just the claim type.
    ///
    /// Determinism note: relies on `serde_json`'s default BTreeMap
    /// object key ordering. Same caveat as elsewhere in the chain —
    /// don't enable `preserve_order`.
    #[must_use]
    pub fn context_hash(&self) -> String {
        let json = serde_json::to_string(&self.context).expect("Value serialization is infallible");
        let mut hasher = Sha256::new();
        hasher.update(json.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

/// What an adapter returns after fetching a receipt.
///
/// `verified == true` means the adapter confirmed the claim against its
/// external source (the GitHub API returned the PR, Browserbase
/// returned a recording, etc). `verified == false` means the adapter
/// returned without a positive confirmation — usually the
/// [`SelfAttestedAdapter`] fallback declaring "no third-party check
/// available." Corpus queries filter on this bit to find unverified
/// chains and prioritize them for adapter coverage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceReceipt {
    /// Adapter that produced the receipt (e.g. `"github_api"`,
    /// `"browserbase"`, `"self_attested"`). Matched against
    /// `EvidenceAdapter::name()` at registration time.
    pub adapter_name: String,

    /// Did the external source confirm the claim?
    pub verified: bool,

    /// Adapter-specific payload — the actual receipt body. Examples:
    /// the GitHub PR JSON, a Browserbase recording URL + duration,
    /// the Doppler audit log entry. Free-form JSON because adapters
    /// own their own schema.
    pub payload: serde_json::Value,

    /// When the receipt was fetched. Set by the adapter from a
    /// system clock; pinning it in the receipt means corpus queries
    /// can filter on freshness ("show me chains where receipts are
    /// >24h old").
    pub fetched_at: DateTime<Utc>,

    /// Hash binding (adapter_name, claim_type, claim_context_hash,
    /// payload_hash). Verifiers re-derive and compare to detect
    /// silent receipt swaps. Computed via [`compute_provenance_hash`].
    pub provenance_hash: String,
}

/// Errors the adapter framework can surface. Adapter-specific errors
/// (HTTP failures, auth refusals) get wrapped in `Fetch` with a
/// human-readable message — typed errors for those would force
/// every adapter into the same error vocabulary, which doesn't match
/// the heterogeneous reality (GitHub's 403 isn't Browserbase's 403
/// isn't Doppler's 403).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterError {
    /// No adapter in the registry supports this claim type.
    NoAdapterForClaim(String),
    /// Adapter accepted the claim but the external source returned
    /// an error. The string is the adapter's free-form description.
    Fetch(String),
    /// Adapter returned a receipt but the provenance hash doesn't
    /// match what we'd derive — usually means the adapter is buggy
    /// or someone tampered with the receipt in transit.
    ProvenanceMismatch { expected: String, actual: String },
    /// Adapter declared it supports the claim type but rejected the
    /// specific context (e.g. required field missing). Message is
    /// adapter-supplied.
    BadClaimContext(String),
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAdapterForClaim(t) => {
                write!(f, "no adapter registered for claim type '{t}'")
            }
            Self::Fetch(s) => write!(f, "adapter fetch failed: {s}"),
            Self::ProvenanceMismatch { expected, actual } => write!(
                f,
                "provenance hash mismatch: expected {expected}, got {actual}",
            ),
            Self::BadClaimContext(s) => {
                write!(f, "adapter rejected claim context: {s}")
            }
        }
    }
}

impl std::error::Error for AdapterError {}

/// Compute the provenance hash from its four components.
///
/// SHA-256 over `adapter_name || claim_type || context_hash || payload_hash`.
/// Order is fixed (changing it invalidates every existing receipt;
/// don't reorder without a migration).
///
/// Pure function — verifiers re-derive and compare. The hash is hex-
/// encoded for storage; same convention as proof-chain hashes.
#[must_use]
pub fn compute_provenance_hash(
    adapter_name: &str,
    claim_type: &str,
    context_hash: &str,
    payload_hash: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(adapter_name.as_bytes());
    hasher.update(claim_type.as_bytes());
    hasher.update(context_hash.as_bytes());
    hasher.update(payload_hash.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Hash a payload value the same way `EvidenceClaim::context_hash`
/// hashes the context. Receipt builders call this to derive the
/// `payload_hash` portion of the provenance hash.
#[must_use]
pub fn compute_payload_hash(payload: &serde_json::Value) -> String {
    let json = serde_json::to_string(payload).expect("Value serialization is infallible");
    let mut hasher = Sha256::new();
    hasher.update(json.as_bytes());
    format!("{:x}", hasher.finalize())
}

impl EvidenceReceipt {
    /// Build a receipt with the provenance hash auto-computed from
    /// the claim and payload. The common construction path; explicit
    /// hash passing is reserved for verifiers re-deriving.
    #[must_use]
    pub fn new(
        adapter_name: impl Into<String>,
        claim: &EvidenceClaim,
        verified: bool,
        payload: serde_json::Value,
        fetched_at: DateTime<Utc>,
    ) -> Self {
        let adapter_name = adapter_name.into();
        let payload_hash = compute_payload_hash(&payload);
        let context_hash = claim.context_hash();
        let provenance_hash = compute_provenance_hash(
            &adapter_name,
            &claim.claim_type,
            &context_hash,
            &payload_hash,
        );
        Self {
            adapter_name,
            verified,
            payload,
            fetched_at,
            provenance_hash,
        }
    }

    /// Re-derive the provenance hash from claim + receipt inputs and
    /// compare to the stored value. Returns `Ok(())` on match, the
    /// mismatch error otherwise.
    ///
    /// Verifiers call this when reading a chain back from disk or
    /// receiving one over the wire. The cost is one SHA-256 — cheap
    /// enough to do on every receipt during chain verification.
    pub fn verify_provenance(&self, claim: &EvidenceClaim) -> Result<(), AdapterError> {
        let payload_hash = compute_payload_hash(&self.payload);
        let context_hash = claim.context_hash();
        let expected = compute_provenance_hash(
            &self.adapter_name,
            &claim.claim_type,
            &context_hash,
            &payload_hash,
        );
        if expected == self.provenance_hash {
            Ok(())
        } else {
            Err(AdapterError::ProvenanceMismatch {
                expected,
                actual: self.provenance_hash.clone(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts() -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    fn sample_claim() -> EvidenceClaim {
        EvidenceClaim {
            skill: "git".into(),
            phase_id: "open_pr".into(),
            step_id: "1".into(),
            claim_type: "git.pr_opened".into(),
            context: serde_json::json!({
                "owner": "garysomerhalder",
                "repo": "sentinel",
                "pr_number": 42,
            }),
        }
    }

    fn sample_payload() -> serde_json::Value {
        serde_json::json!({ "url": "https://github.com/garysomerhalder/sentinel/pull/42" })
    }

    // ─── EvidenceClaim.context_hash ──────────────────────────────────

    #[test]
    fn context_hash_is_deterministic() {
        // Same context, same hash. Across machines, across runs.
        let claim = sample_claim();
        let h1 = claim.context_hash();
        let h2 = claim.context_hash();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64, "SHA-256 hex is 64 chars");
    }

    #[test]
    fn context_hash_differs_when_context_differs() {
        let mut a = sample_claim();
        let mut b = sample_claim();
        a.context = serde_json::json!({ "pr_number": 42 });
        b.context = serde_json::json!({ "pr_number": 43 });
        assert_ne!(a.context_hash(), b.context_hash());
    }

    #[test]
    fn null_context_hashes_to_a_stable_value() {
        // Edge case: `context` defaults to Value::Null when omitted.
        // Hash must still be deterministic so adapters that don't
        // require context still get a valid provenance hash.
        let mut claim = sample_claim();
        claim.context = serde_json::Value::Null;
        let h = claim.context_hash();
        assert_eq!(h.len(), 64);
    }

    // ─── compute_provenance_hash ─────────────────────────────────────

    #[test]
    fn provenance_hash_is_deterministic() {
        let h1 = compute_provenance_hash("a", "b", "c", "d");
        let h2 = compute_provenance_hash("a", "b", "c", "d");
        assert_eq!(h1, h2);
    }

    #[test]
    fn provenance_hash_changes_with_each_component() {
        // Each input contributes — swapping any one breaks the hash.
        let base = compute_provenance_hash("a", "b", "c", "d");
        assert_ne!(base, compute_provenance_hash("a2", "b", "c", "d"));
        assert_ne!(base, compute_provenance_hash("a", "b2", "c", "d"));
        assert_ne!(base, compute_provenance_hash("a", "b", "c2", "d"));
        assert_ne!(base, compute_provenance_hash("a", "b", "c", "d2"));
    }

    #[test]
    fn provenance_hash_order_matters() {
        // Catch the "swap arguments" footgun — if two components have
        // the same string value, reordering MUST still produce a
        // different hash.
        let h1 = compute_provenance_hash("x", "y", "z", "w");
        let h2 = compute_provenance_hash("y", "x", "z", "w");
        assert_ne!(h1, h2);
    }

    // ─── EvidenceReceipt::new + verify_provenance ────────────────────

    #[test]
    fn new_populates_provenance_hash() {
        let claim = sample_claim();
        let receipt = EvidenceReceipt::new("github_api", &claim, true, sample_payload(), ts());
        assert_eq!(receipt.provenance_hash.len(), 64);
        assert!(receipt.verify_provenance(&claim).is_ok());
    }

    #[test]
    fn verify_provenance_succeeds_on_round_trip() {
        // Build a receipt → serialize to JSON → deserialize →
        // re-verify. Provenance hash must survive the trip.
        let claim = sample_claim();
        let original = EvidenceReceipt::new("github_api", &claim, true, sample_payload(), ts());
        let json = serde_json::to_string(&original).unwrap();
        let restored: EvidenceReceipt = serde_json::from_str(&json).unwrap();
        assert!(restored.verify_provenance(&claim).is_ok());
    }

    #[test]
    fn verify_provenance_detects_payload_tampering() {
        // Receipt issued; attacker swaps the payload body.
        // Re-verification must catch the substitution.
        let claim = sample_claim();
        let mut receipt = EvidenceReceipt::new("github_api", &claim, true, sample_payload(), ts());
        receipt.payload = serde_json::json!({ "url": "https://attacker.example.com/fake" });
        let err = receipt.verify_provenance(&claim).unwrap_err();
        assert!(matches!(err, AdapterError::ProvenanceMismatch { .. }));
    }

    #[test]
    fn verify_provenance_detects_adapter_swap() {
        // A receipt from `github_api` can't be re-labeled as coming
        // from `self_attested` — the provenance hash binds the
        // adapter identity.
        let claim = sample_claim();
        let mut receipt = EvidenceReceipt::new("github_api", &claim, true, sample_payload(), ts());
        receipt.adapter_name = "self_attested".into();
        let err = receipt.verify_provenance(&claim).unwrap_err();
        assert!(matches!(err, AdapterError::ProvenanceMismatch { .. }));
    }

    #[test]
    fn verify_provenance_detects_claim_replay() {
        // Receipt for PR #42 cannot be re-presented as a receipt for
        // PR #43 — the context is part of the hash.
        let claim_42 = sample_claim();
        let receipt = EvidenceReceipt::new("github_api", &claim_42, true, sample_payload(), ts());
        // Verify against a different claim (different pr_number).
        let mut claim_43 = sample_claim();
        claim_43.context = serde_json::json!({
            "owner": "garysomerhalder",
            "repo": "sentinel",
            "pr_number": 43,
        });
        let err = receipt.verify_provenance(&claim_43).unwrap_err();
        assert!(matches!(err, AdapterError::ProvenanceMismatch { .. }));
    }

    #[test]
    fn verify_provenance_detects_claim_type_swap() {
        // Same context but different claim_type — verification must
        // reject. Otherwise an adapter that proves "PR opened" could
        // be replayed as proving "PR merged."
        let claim_a = EvidenceClaim {
            claim_type: "git.pr_opened".into(),
            ..sample_claim()
        };
        let receipt = EvidenceReceipt::new("github_api", &claim_a, true, sample_payload(), ts());
        let claim_b = EvidenceClaim {
            claim_type: "git.pr_merged".into(),
            ..sample_claim()
        };
        let err = receipt.verify_provenance(&claim_b).unwrap_err();
        assert!(matches!(err, AdapterError::ProvenanceMismatch { .. }));
    }

    #[test]
    fn verified_false_still_gets_provenance_hash() {
        // The self-attested fallback returns verified=false. It still
        // gets a provenance hash — the hash binds adapter+claim+payload
        // even when the adapter is admitting it didn't really verify.
        // This makes "we know we don't know" a first-class chain entry
        // rather than an absence-of-data ambiguity.
        let claim = sample_claim();
        let receipt =
            EvidenceReceipt::new("self_attested", &claim, false, serde_json::json!({}), ts());
        assert!(!receipt.verified);
        assert_eq!(receipt.provenance_hash.len(), 64);
        assert!(receipt.verify_provenance(&claim).is_ok());
    }

    // ─── Serde shape ─────────────────────────────────────────────────

    #[test]
    fn claim_serializes_with_optional_context_default() {
        // EvidenceClaim's `context` defaults to Null when omitted.
        // Pre-framework chains being migrated must still load.
        let json = r#"{
            "skill": "git",
            "phase_id": "p",
            "step_id": "1",
            "claim_type": "x"
        }"#;
        let claim: EvidenceClaim = serde_json::from_str(json).unwrap();
        assert_eq!(claim.context, serde_json::Value::Null);
    }

    #[test]
    fn adapter_error_display_uses_concrete_messages() {
        // Hook deny messages surface Display. Pin that they include
        // the actual claim type / hashes for diagnostic clarity.
        let e = AdapterError::NoAdapterForClaim("git.pr_opened".into());
        assert!(format!("{e}").contains("git.pr_opened"));
        let e = AdapterError::ProvenanceMismatch {
            expected: "abc".into(),
            actual: "def".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("abc"));
        assert!(s.contains("def"));
    }
}
