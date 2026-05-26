//! Praefectus client surface for Sentinel.
//!
//! Per Fabrica ADR-001 §1 (Praefectus is per-machine) and ADR-002 §3
//! (Sentinel calls Praefectus via the typed port before signing
//! Catastrophic-class ProofBundles).
//!
//! The Praefectus runs in the consul-app process on the operator's
//! machine. Sentinel runs in the daemon process (typically the same
//! machine but a separate process). This module defines the Sentinel-
//! side client surface: a trait + an in-memory stub for development
//! and tests.
//!
//! **Production implementation (deferred):** an HTTP/IPC client that
//! talks to a Praefectus endpoint exposed by consul-app. Wire protocol
//! TBD — likely the same per-machine HTTP localhost pattern used by
//! sentinel-daemon's existing `/legatus/*` endpoints, but in the
//! reverse direction (Sentinel as client, consul-app as server).
//!
//! Why a Sentinel-side stub now:
//! - Hooks that need to populate `PhaseProof.actor` / `StepProof.actor`
//!   can take a `Arc<dyn PraefectusClient>` parameter today; production
//!   wires the real adapter, tests wire the stub.
//! - Hooks that gate Catastrophic actions can call
//!   `verify_voiceprint_witness` and treat the trait as the seam — no
//!   need to know which process holds the Praefectus.
//! - Keeps the actor-population work decoupled from the IPC wire
//!   design (which will likely be its own ADR).

use async_trait::async_trait;
use consul_domain::identity::republic::{OperatorId, RoleBinding, VoiceprintWitness};

/// Identifier of the escalation a witness is being verified against.
///
/// Mirrors `consul_domain::ports::praefectus::EscalationRef` — opaque
/// string handle so this client doesn't take a deeper dep on the
/// protocol crate's `EscalationKey` enum.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct EscalationRef(pub String);

/// Errors returned by [`PraefectusClient`] operations.
#[derive(Debug, thiserror::Error)]
pub enum PraefectusClientError {
    /// Verification failed for the named reason. Use this for
    /// cryptographic / payload-shape failures (the witness payload
    /// itself is invalid). For "the bearer token Sentinel sent is
    /// no longer accepted by Praefectus", prefer
    /// [`Self::TokenInvalid`] — operators rotate the token, they
    /// don't try a different witness.
    #[error("verification failed: {0}")]
    Verification(String),

    /// The bearer token in `--legatus-praefectus-token` /
    /// `LEGATUS_PRAEFECTUS_TOKEN` is no longer accepted by
    /// Praefectus (typically: expired, revoked, or rotated on the
    /// Praefectus side without updating sentinel's env). Distinct
    /// from `Verification` so operators see a clear "rotate your
    /// token" message instead of debugging a witness that may
    /// actually be fine.
    ///
    /// Carries the body Praefectus returned (often empty for 401)
    /// for diagnostics.
    #[error(
        "praefectus rejected the bearer token (401): {0} \
         — rotate LEGATUS_PRAEFECTUS_TOKEN and restart `sentinel daemon`"
    )]
    TokenInvalid(String),

    /// Praefectus is unreachable (process down, network partition, etc).
    /// Sentinel's Catastrophic gate treats this as a deny — fail closed.
    #[error("praefectus unreachable: {0}")]
    Unreachable(String),

    /// Operator has no known role binding for the requested session.
    #[error("no role binding for session: {0}")]
    NoRoleBinding(String),
}

/// Sentinel-side client to the operator's Praefectus.
///
/// All implementations are `Send + Sync` so hooks can hold `Arc<dyn
/// PraefectusClient>` across `tokio::spawn` boundaries.
#[async_trait]
pub trait PraefectusClient: Send + Sync {
    /// Verify a voiceprint witness against the named operator + escalation.
    ///
    /// Production implementations forward to the Praefectus's
    /// `verify_voiceprint_witness` over IPC. The local stub accepts
    /// based on internal state (test-controlled).
    async fn verify_voiceprint_witness(
        &self,
        witness: &VoiceprintWitness,
        expected_operator: OperatorId,
        expected_escalation: &EscalationRef,
    ) -> Result<(), PraefectusClientError>;

    /// Look up the current `RoleBinding` for a given session, if any.
    ///
    /// Used by proof-construction hooks to populate `PhaseProof.actor`
    /// and `StepProof.actor`. Returns `None` when the session has not
    /// yet been bound to a Praefectus-issued role (typical for
    /// pre-Praefectus-wired deployments).
    async fn current_role_binding(
        &self,
        session_id: &str,
    ) -> Result<Option<RoleBinding>, PraefectusClientError>;
}

/// In-memory stub for development and tests.
///
/// Behavior:
/// - `verify_voiceprint_witness` returns `Ok(())` for any input unless
///   `set_fail_verification(true)` has been called.
/// - `current_role_binding` returns whatever was last installed for
///   the session via `set_role_binding`, or `None`.
///
/// **Critical:** this is NOT a security boundary. The real Praefectus
/// performs cryptographic verification; the stub accepts blindly so
/// the actor-population code path can be exercised in tests without
/// the full Praefectus deployment.
#[derive(Clone, Debug, Default)]
pub struct InMemoryPraefectusClient {
    inner: std::sync::Arc<std::sync::Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    fail_verification: bool,
    bindings: std::collections::HashMap<String, RoleBinding>,
}

impl InMemoryPraefectusClient {
    /// Construct an empty client.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Test toggle — make `verify_voiceprint_witness` fail.
    pub fn set_fail_verification(&self, fail: bool) {
        self.lock().fail_verification = fail;
    }

    /// Install a role binding for `session_id`. Subsequent calls to
    /// `current_role_binding(session_id)` will return `Some(binding)`.
    pub fn set_role_binding(&self, session_id: impl Into<String>, binding: RoleBinding) {
        self.lock().bindings.insert(session_id.into(), binding);
    }
}

#[async_trait]
impl PraefectusClient for InMemoryPraefectusClient {
    async fn verify_voiceprint_witness(
        &self,
        _witness: &VoiceprintWitness,
        _expected_operator: OperatorId,
        _expected_escalation: &EscalationRef,
    ) -> Result<(), PraefectusClientError> {
        if self.lock().fail_verification {
            return Err(PraefectusClientError::Verification(
                "stub-rejected via set_fail_verification".into(),
            ));
        }
        Ok(())
    }

    async fn current_role_binding(
        &self,
        session_id: &str,
    ) -> Result<Option<RoleBinding>, PraefectusClientError> {
        Ok(self.lock().bindings.get(session_id).cloned())
    }
}

// ============================================================
// HttpPraefectusClient (Path B) — production HTTP/IPC adapter
// pointing at consul-app's Praefectus endpoint.
//
// Endpoint shape (server-side endpoint deferred to consul-app commit):
//   POST {base_url}/praefectus/verify-witness
//     Body: { witness, expected_operator, expected_escalation }
//     Auth: Bearer <token>
//     200 → { "ok": true }
//     401 → PraefectusClientError::Verification (auth bad)
//     422 → PraefectusClientError::Verification (verification failed,
//           body carries reason)
//
//   POST {base_url}/praefectus/role-binding
//     Body: { session_id }
//     Auth: Bearer <token>
//     200 → { "binding": Some(RoleBinding) | None }
//     other → PraefectusClientError::NoRoleBinding | Unreachable
//
// Network errors (connect refused, timeout) → Unreachable. Sentinel's
// proof_engine treats Unreachable as None (best-effort) for actor
// lookup, but the Catastrophic gate (deferred) treats it as deny.
// ============================================================

/// Configuration for [`HttpPraefectusClient`].
#[derive(Clone, Debug)]
pub struct HttpPraefectusConfig {
    /// Base URL of the Praefectus HTTP endpoint (e.g.
    /// `http://127.0.0.1:9001`). Typically localhost since the
    /// Praefectus runs on the operator's machine per ADR-001 §1.
    pub base_url: String,
    /// Bearer token used to authenticate every request. Mirrors the
    /// sentinel-daemon /legatus/* auth pattern (per-instance random
    /// token written to a known path; client reads from disk at boot).
    pub bearer_token: String,
    /// Per-request timeout. Network operations that don't return
    /// within this window count as `Unreachable`.
    pub timeout: std::time::Duration,
}

impl HttpPraefectusConfig {
    /// Construct a config with a default 5-second timeout.
    #[must_use]
    pub fn new(base_url: impl Into<String>, bearer_token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            bearer_token: bearer_token.into(),
            timeout: std::time::Duration::from_secs(5),
        }
    }
}

/// Production HTTP client implementing [`PraefectusClient`].
///
/// Posts JSON-encoded requests to a Praefectus endpoint exposed by
/// consul-app on localhost. Authentication via Bearer token mirroring
/// the existing sentinel-daemon `/legatus/*` pattern.
///
/// **Server-side endpoint deferred:** consul-app needs to expose the
/// matching `POST /praefectus/verify-witness` and
/// `POST /praefectus/role-binding` routes. Until then,
/// `HttpPraefectusClient` returns `Unreachable` against a missing
/// endpoint, and `ProofEngine.lookup_actor()` falls back to `None`
/// — proof construction stays unblocked.
#[derive(Clone, Debug)]
pub struct HttpPraefectusClient {
    config: HttpPraefectusConfig,
    http: reqwest::Client,
}

impl HttpPraefectusClient {
    /// Construct from a config. Builds a `reqwest::Client` with the
    /// configured timeout.
    ///
    /// # Errors
    /// Returns an error if `reqwest::Client::builder().build()` fails
    /// (typically TLS backend initialization issues).
    pub fn new(config: HttpPraefectusConfig) -> Result<Self, PraefectusClientError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| PraefectusClientError::Unreachable(format!("reqwest build: {e}")))?;
        Ok(Self { config, http })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.base_url.trim_end_matches('/'), path)
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.config.bearer_token)
    }
}

#[async_trait]
impl PraefectusClient for HttpPraefectusClient {
    async fn verify_voiceprint_witness(
        &self,
        witness: &VoiceprintWitness,
        expected_operator: OperatorId,
        expected_escalation: &EscalationRef,
    ) -> Result<(), PraefectusClientError> {
        let body = serde_json::json!({
            "witness": witness,
            "expected_operator": expected_operator,
            "expected_escalation": expected_escalation.0,
        });
        let resp = self
            .http
            .post(self.url("/praefectus/verify-witness"))
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .json(&body)
            .send()
            .await
            .map_err(|e| PraefectusClientError::Unreachable(e.to_string()))?;
        match resp.status().as_u16() {
            200 => Ok(()),
            401 => {
                // Surface as a dedicated variant so the witness
                // verifier adapter can log "rotate your token" at
                // warn instead of treating it like any other
                // verification failure.
                let body = resp.text().await.unwrap_or_default();
                Err(PraefectusClientError::TokenInvalid(body))
            }
            422 => {
                let body = resp.text().await.unwrap_or_else(|_| "no body".into());
                Err(PraefectusClientError::Verification(body))
            }
            other => Err(PraefectusClientError::Unreachable(format!(
                "unexpected status {other}"
            ))),
        }
    }

    async fn current_role_binding(
        &self,
        session_id: &str,
    ) -> Result<Option<RoleBinding>, PraefectusClientError> {
        let body = serde_json::json!({ "session_id": session_id });
        let resp = self
            .http
            .post(self.url("/praefectus/role-binding"))
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .json(&body)
            .send()
            .await
            .map_err(|e| PraefectusClientError::Unreachable(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(PraefectusClientError::NoRoleBinding(session_id.to_string()));
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            binding: Option<RoleBinding>,
        }
        let parsed: Resp = resp
            .json()
            .await
            .map_err(|e| PraefectusClientError::Unreachable(format!("body parse: {e}")))?;
        Ok(parsed.binding)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use chrono::Utc;
    use consul_domain::identity::republic::{
        AuxiliumId, BusinessId, CenturionId, ChallengeNonce, ConstitutionVersion, MilesId, ProofId,
        SpecContractRef,
    };
    use uuid::Uuid;

    use super::*;

    fn op() -> OperatorId {
        OperatorId::from_uuid(Uuid::from_bytes([0xA0; 16]))
    }

    fn witness() -> VoiceprintWitness {
        VoiceprintWitness {
            operator: op(),
            utterance_audio_hash: [0x11; 32],
            utterance_transcript: "approve refund 4471".into(),
            challenge_nonce: ChallengeNonce::from_bytes([0x77; 16]),
            signature: [0x22; 64],
            signed_at: Utc::now(),
        }
    }

    fn binding() -> RoleBinding {
        RoleBinding {
            miles: MilesId::new(),
            auxilium: AuxiliumId::new("support-auxilium"),
            centurion: CenturionId::new(),
            spec_contract: SpecContractRef::new("support-auxilium.refund-miles@1.0.0"),
            constitution_version: ConstitutionVersion::new("1.0.0"),
            operator: op(),
            business: BusinessId::new(),
            authorized_at: Utc::now(),
            authorized_by_proof: ProofId::new(),
        }
    }

    #[tokio::test]
    async fn stub_accepts_verification_by_default() {
        let c = InMemoryPraefectusClient::new();
        let result = c
            .verify_voiceprint_witness(&witness(), op(), &EscalationRef("e1".into()))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn stub_rejects_when_toggle_set() {
        let c = InMemoryPraefectusClient::new();
        c.set_fail_verification(true);
        let result = c
            .verify_voiceprint_witness(&witness(), op(), &EscalationRef("e1".into()))
            .await;
        assert!(matches!(
            result,
            Err(PraefectusClientError::Verification(_))
        ));
    }

    #[tokio::test]
    async fn role_binding_lookup_returns_none_for_unknown_session() {
        let c = InMemoryPraefectusClient::new();
        let result = c.current_role_binding("unknown").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn role_binding_lookup_returns_installed_binding() {
        let c = InMemoryPraefectusClient::new();
        let b = binding();
        c.set_role_binding("sess-1", b.clone());
        let result = c.current_role_binding("sess-1").await.unwrap();
        assert_eq!(result.unwrap().auxilium, b.auxilium);
    }

    #[tokio::test]
    async fn role_binding_isolated_per_session() {
        let c = InMemoryPraefectusClient::new();
        c.set_role_binding("sess-a", binding());
        let r = c.current_role_binding("sess-b").await.unwrap();
        assert!(r.is_none());
    }

    // ── HttpPraefectusClient ───────────────────────────────────────

    #[test]
    fn http_config_construction() {
        let cfg = HttpPraefectusConfig::new("http://127.0.0.1:9001", "tok123");
        assert_eq!(cfg.base_url, "http://127.0.0.1:9001");
        assert_eq!(cfg.bearer_token, "tok123");
        assert_eq!(cfg.timeout, std::time::Duration::from_secs(5));
    }

    #[test]
    fn http_client_builds_cleanly() {
        let cfg = HttpPraefectusConfig::new("http://127.0.0.1:9001", "tok123");
        let client = HttpPraefectusClient::new(cfg);
        assert!(client.is_ok());
    }

    #[test]
    fn http_url_construction_strips_trailing_slash() {
        let cfg = HttpPraefectusConfig::new("http://127.0.0.1:9001/", "tok");
        let c = HttpPraefectusClient::new(cfg).unwrap();
        assert_eq!(
            c.url("/praefectus/verify-witness"),
            "http://127.0.0.1:9001/praefectus/verify-witness"
        );
    }

    #[test]
    fn http_url_construction_without_trailing_slash() {
        let cfg = HttpPraefectusConfig::new("http://127.0.0.1:9001", "tok");
        let c = HttpPraefectusClient::new(cfg).unwrap();
        assert_eq!(
            c.url("/praefectus/role-binding"),
            "http://127.0.0.1:9001/praefectus/role-binding"
        );
    }

    #[test]
    fn http_auth_header_format() {
        let cfg = HttpPraefectusConfig::new("http://127.0.0.1:9001", "tok456");
        let c = HttpPraefectusClient::new(cfg).unwrap();
        assert_eq!(c.auth_header(), "Bearer tok456");
    }

    #[tokio::test]
    async fn http_unreachable_endpoint_returns_unreachable_error() {
        // Bind to a port we know is closed (port 1 is privileged + reserved).
        // The request should immediately fail at connect with Unreachable.
        let cfg = HttpPraefectusConfig {
            base_url: "http://127.0.0.1:1".into(),
            bearer_token: "tok".into(),
            timeout: std::time::Duration::from_millis(200),
        };
        let c = HttpPraefectusClient::new(cfg).unwrap();
        let r = c.current_role_binding("test-session").await;
        assert!(matches!(r, Err(PraefectusClientError::Unreachable(_))));
    }
}
