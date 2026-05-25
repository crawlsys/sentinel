//! Pydantic-style fail-fast config validation (M4.8 / task #26,
//! `ContextForge` pattern).
//!
//! `SentinelConfig` is the canonical top-level config struct that
//! every component shares — request limits, SSRF policy, judge tier,
//! proof archive, signing. It loads from defaults + serde TOML +
//! (eventually) env-var overrides, and `validate()` runs every
//! sub-validator on startup, collecting *all* errors before
//! returning. The Pydantic shape — fail fast, fail loud, surface the
//! complete punch list — is the load-bearing property: misconfig that
//! surfaces three days later when a hook fires for the first time is
//! the failure mode this whole layer exists to prevent.
//!
//! # What's NOT in this commit (follow-up surface)
//!
//! - Env-var loading. `SENTINEL_ALLOW_PRIVATE=1` etc. is a fragile
//!   surface that touches each adapter and deserves its own commit
//!   so the env-var registry can be reviewed end-to-end.
//! - Per-component plumbing. Today every component reads its own
//!   `RequestLimits` / `SsrfPolicy` from wherever it likes; the
//!   migration to "every component reads from a shared
//!   `SentinelConfig`" is a refactor that lands skill-by-skill.
//!
//! Today's deliverable is the **shape + the rules**. Future commits
//! plug existing components into this struct without changing any
//! of the validation surface.

use serde::{Deserialize, Serialize};

use crate::judge::JudgeModel;
use crate::request_limits::RequestLimits;
use crate::ssrf::SsrfPolicy;

/// The top-level Sentinel config. All sub-configs are composable
/// — drop in a new `Default` and the parent picks it up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct SentinelConfig {
    #[serde(default)]
    pub request_limits: RequestLimits,
    #[serde(default)]
    pub ssrf: SsrfPolicy,
    #[serde(default)]
    pub judge: JudgeConfig,
    #[serde(default)]
    pub proof_archive: ProofArchiveConfig,
    #[serde(default)]
    pub signing: SigningConfig,
}


impl SentinelConfig {
    /// Run every sub-validator and return the complete punch list.
    ///
    /// Pydantic-style: collects every error, doesn't bail on first.
    /// `Ok(())` means no validation failures; `Err(errors)` lists
    /// every problem so a startup banner can surface them all at
    /// once (one round-trip with the operator instead of N).
    ///
    /// # Errors
    ///
    /// Returns `Err` with at least one `ConfigError` if any sub-config
    /// is invalid.
    pub fn validate(&self) -> Result<(), Vec<ConfigError>> {
        let mut errors = Vec::new();
        validate_request_limits(&self.request_limits, &mut errors);
        validate_ssrf(&self.ssrf, &mut errors);
        validate_judge(&self.judge, &mut errors);
        validate_proof_archive(&self.proof_archive, &mut errors);
        validate_signing(&self.signing, &mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Judge-tier configuration. Today carries the default tier (used
/// when a step config omits the `judge` field) and a flag controlling
/// whether software-only signing is acceptable for this deployment
/// (false ⇒ hardware signing required, ties into M1.10 follow-up).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeConfig {
    /// Default `JudgeModel` for steps that don't declare a tier.
    /// Defaults to [`JudgeModel::default_review_tier`] — Kimi K2.6.
    #[serde(default = "default_judge_tier")]
    pub default_tier: JudgeModel,
}

const fn default_judge_tier() -> JudgeModel {
    JudgeModel::default_review_tier()
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            default_tier: default_judge_tier(),
        }
    }
}

/// Proof-archive aging policy. Today only the aging window —
/// chains older than `aging_days` get summarized (or, in a future
/// commit, moved to a cold subdirectory). `None` ⇒ keep everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProofArchiveConfig {
    /// Aging window in days. None ⇒ no aging applied.
    #[serde(default)]
    pub aging_days: Option<u32>,
}

/// Signing-key configuration. Today the env-var name + a flag
/// requiring hardware backing (M1.10 follow-up). When
/// `hardware_required` is `true`, the signing backend trait must
/// produce a non-software variant or chain submission fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SigningConfig {
    /// Env-var name carrying the Ed25519 signing key. `None` ⇒
    /// signing disabled (chains rely on the hash-only integrity
    /// guarantee).
    #[serde(default)]
    pub signing_key_env: Option<String>,
    /// When `true`, software-only signing is rejected at chain
    /// submission. Composes with `signing_key_env` — software keys
    /// satisfy `signing_key_env.is_some()` but FAIL `hardware_required`.
    #[serde(default)]
    pub hardware_required: bool,
}

impl Default for SigningConfig {
    fn default() -> Self {
        Self {
            signing_key_env: Some("SENTINEL_SIGNING_KEY".to_string()),
            hardware_required: false,
        }
    }
}

/// One validation failure. Each variant carries the offending value
/// + the rule that was broken so a deny-banner can render an
/// actionable line per error without parsing the enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `RequestLimits.max_evidence_bytes` exceeded the safety ceiling.
    EvidenceLimitTooLarge { actual: usize, ceiling: usize },
    /// `RequestLimits.max_artifact_bytes` exceeded the safety ceiling.
    ArtifactLimitTooLarge { actual: usize, ceiling: usize },
    /// `RequestLimits.window_seconds` is zero with rate limiting
    /// otherwise enabled — the math doesn't work.
    RateLimitWindowZero { max_calls_per_window: usize },
    /// SSRF allowlist entry has an invalid hostname (whitespace,
    /// empty, or contains spaces).
    SsrfAllowlistInvalidHost { entry: String },
    /// SSRF denylist entry has the same problem.
    SsrfDenylistInvalidHost { entry: String },
    /// Aging window is unreasonably large (sanity check, not data
    /// integrity — > 365 days is almost certainly a typo).
    ProofArchiveAgingTooLarge { actual_days: u32, ceiling_days: u32 },
    /// `signing.hardware_required = true` but no `signing_key_env`
    /// is set — there's no path to produce a signature at all.
    HardwareSigningRequiredButNoKeyEnv,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EvidenceLimitTooLarge { actual, ceiling } => write!(
                f,
                "request_limits.max_evidence_bytes = {actual} exceeds safety ceiling {ceiling}"
            ),
            Self::ArtifactLimitTooLarge { actual, ceiling } => write!(
                f,
                "request_limits.max_artifact_bytes = {actual} exceeds safety ceiling {ceiling}"
            ),
            Self::RateLimitWindowZero { max_calls_per_window } => write!(
                f,
                "request_limits.window_seconds = 0 but max_calls_per_window = {max_calls_per_window} (set window_seconds > 0 or set max_calls_per_window = 0 to disable rate limiting)"
            ),
            Self::SsrfAllowlistInvalidHost { entry } => {
                write!(f, "ssrf.allowlist contains invalid host '{entry}'")
            }
            Self::SsrfDenylistInvalidHost { entry } => {
                write!(f, "ssrf.denylist contains invalid host '{entry}'")
            }
            Self::ProofArchiveAgingTooLarge { actual_days, ceiling_days } => write!(
                f,
                "proof_archive.aging_days = {actual_days} exceeds sanity ceiling {ceiling_days} (probably a typo — chains older than a year are operational antiques)"
            ),
            Self::HardwareSigningRequiredButNoKeyEnv => f.write_str(
                "signing.hardware_required = true but signing.signing_key_env is None (no path to produce a signature)",
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// 100 MiB — the absolute upper bound on a single evidence blob.
/// Above this, judge token costs explode and downstream serde
/// round-trips slow into the timeout band of every consumer.
const EVIDENCE_BYTE_CEILING: usize = 100 * 1024 * 1024;

/// 10 MiB — same logic for the typed handoff artifact. Bigger
/// artifacts are an architectural smell (the chain shouldn't be
/// shuttling whole files; pointers to S3 / R2 fit better).
const ARTIFACT_BYTE_CEILING: usize = 10 * 1024 * 1024;

/// 365 days — proof-archive aging beyond a year is a typo; the
/// router-as-planner moat doesn't get measurably better with chains
/// older than a quarter.
const ARCHIVE_AGING_DAY_CEILING: u32 = 365;

fn validate_request_limits(rl: &RequestLimits, errors: &mut Vec<ConfigError>) {
    if rl.max_evidence_bytes > EVIDENCE_BYTE_CEILING {
        errors.push(ConfigError::EvidenceLimitTooLarge {
            actual: rl.max_evidence_bytes,
            ceiling: EVIDENCE_BYTE_CEILING,
        });
    }
    if rl.max_artifact_bytes > ARTIFACT_BYTE_CEILING {
        errors.push(ConfigError::ArtifactLimitTooLarge {
            actual: rl.max_artifact_bytes,
            ceiling: ARTIFACT_BYTE_CEILING,
        });
    }
    // Rate limiting enabled (max_calls_per_window > 0) requires a
    // non-zero window. window_seconds == 0 with calls_per_window > 0
    // is a divide-by-zero in any sliding-window math.
    if rl.window_seconds == 0 && rl.max_calls_per_window > 0 {
        errors.push(ConfigError::RateLimitWindowZero {
            max_calls_per_window: rl.max_calls_per_window,
        });
    }
}

fn validate_ssrf(s: &SsrfPolicy, errors: &mut Vec<ConfigError>) {
    for entry in &s.allowlist {
        if !is_valid_hostlike(entry) {
            errors.push(ConfigError::SsrfAllowlistInvalidHost {
                entry: entry.clone(),
            });
        }
    }
    for entry in &s.denylist {
        if !is_valid_hostlike(entry) {
            errors.push(ConfigError::SsrfDenylistInvalidHost {
                entry: entry.clone(),
            });
        }
    }
}

/// Lightweight hostname/IP literal validator. Accepts:
/// - Non-empty strings
/// - No whitespace anywhere
/// - No control characters
/// - No tabs / newlines
///
/// Doesn't try to be a full DNS-name validator (would false-reject
/// valid weird-but-real names). Just catches the obvious typos
/// (trailing space, embedded newline) that produce silent
/// configuration drift.
fn is_valid_hostlike(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

const fn validate_judge(_j: &JudgeConfig, _errors: &mut Vec<ConfigError>) {
    // `JudgeModel` is an enum — invalid variants are caught by serde
    // at deserialization time. Nothing further to validate today.
    // Future: when multi-judge tiers land (#82 Stage B), validate
    // that the per-tier model lists are non-empty and don't
    // duplicate.
}

fn validate_proof_archive(p: &ProofArchiveConfig, errors: &mut Vec<ConfigError>) {
    if let Some(days) = p.aging_days {
        if days > ARCHIVE_AGING_DAY_CEILING {
            errors.push(ConfigError::ProofArchiveAgingTooLarge {
                actual_days: days,
                ceiling_days: ARCHIVE_AGING_DAY_CEILING,
            });
        }
    }
}

fn validate_signing(s: &SigningConfig, errors: &mut Vec<ConfigError>) {
    if s.hardware_required && s.signing_key_env.is_none() {
        errors.push(ConfigError::HardwareSigningRequiredButNoKeyEnv);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_validates_clean() {
        let cfg = SentinelConfig::default();
        assert!(cfg.validate().is_ok(), "default must validate cleanly");
    }

    #[test]
    fn evidence_too_large_caught() {
        let mut cfg = SentinelConfig::default();
        cfg.request_limits.max_evidence_bytes = 200 * 1024 * 1024; // 200 MiB
        let errs = cfg.validate().unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0], ConfigError::EvidenceLimitTooLarge { .. }));
    }

    #[test]
    fn artifact_too_large_caught() {
        let mut cfg = SentinelConfig::default();
        cfg.request_limits.max_artifact_bytes = 50 * 1024 * 1024; // 50 MiB
        let errs = cfg.validate().unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, ConfigError::ArtifactLimitTooLarge { .. })));
    }

    #[test]
    fn rate_limit_window_zero_caught() {
        let mut cfg = SentinelConfig::default();
        cfg.request_limits.window_seconds = 0;
        cfg.request_limits.max_calls_per_window = 60;
        let errs = cfg.validate().unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, ConfigError::RateLimitWindowZero { .. })));
    }

    #[test]
    fn rate_limit_disabled_with_zero_calls_validates() {
        // window_seconds = 0 AND max_calls_per_window = 0 means rate
        // limiting is disabled — valid configuration.
        let mut cfg = SentinelConfig::default();
        cfg.request_limits.window_seconds = 0;
        cfg.request_limits.max_calls_per_window = 0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn ssrf_allowlist_whitespace_caught() {
        let mut cfg = SentinelConfig::default();
        cfg.ssrf.allowlist.push("evil with space".to_string());
        let errs = cfg.validate().unwrap_err();
        match &errs[0] {
            ConfigError::SsrfAllowlistInvalidHost { entry } => {
                assert_eq!(entry, "evil with space");
            }
            other => panic!("expected SsrfAllowlistInvalidHost, got {other:?}"),
        }
    }

    #[test]
    fn ssrf_denylist_empty_caught() {
        let mut cfg = SentinelConfig::default();
        cfg.ssrf.denylist.push(String::new());
        let errs = cfg.validate().unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, ConfigError::SsrfDenylistInvalidHost { .. })));
    }

    #[test]
    fn proof_archive_aging_too_large_caught() {
        let mut cfg = SentinelConfig::default();
        cfg.proof_archive.aging_days = Some(500);
        let errs = cfg.validate().unwrap_err();
        match &errs[0] {
            ConfigError::ProofArchiveAgingTooLarge { actual_days, .. } => {
                assert_eq!(*actual_days, 500);
            }
            other => panic!("expected ProofArchiveAgingTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn proof_archive_aging_none_validates() {
        let mut cfg = SentinelConfig::default();
        cfg.proof_archive.aging_days = None;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn hardware_required_but_no_env_caught() {
        let mut cfg = SentinelConfig::default();
        cfg.signing.hardware_required = true;
        cfg.signing.signing_key_env = None;
        let errs = cfg.validate().unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0], ConfigError::HardwareSigningRequiredButNoKeyEnv);
    }

    #[test]
    fn hardware_required_with_env_validates() {
        let mut cfg = SentinelConfig::default();
        cfg.signing.hardware_required = true;
        cfg.signing.signing_key_env = Some("SENTINEL_SIGNING_KEY".to_string());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn multiple_errors_collected_pydantic_style() {
        // The load-bearing invariant: validate() collects ALL errors,
        // not just the first. Bunch up 4 distinct rules and confirm.
        let mut cfg = SentinelConfig::default();
        cfg.request_limits.max_evidence_bytes = 200 * 1024 * 1024; // err 1
        cfg.request_limits.max_artifact_bytes = 50 * 1024 * 1024; // err 2
        cfg.ssrf.allowlist.push("bad host".to_string()); // err 3
        cfg.proof_archive.aging_days = Some(9999); // err 4

        let errs = cfg.validate().unwrap_err();
        assert_eq!(
            errs.len(),
            4,
            "Pydantic-style: every rule reports independently — got {errs:?}"
        );
        assert!(errs
            .iter()
            .any(|e| matches!(e, ConfigError::EvidenceLimitTooLarge { .. })));
        assert!(errs
            .iter()
            .any(|e| matches!(e, ConfigError::ArtifactLimitTooLarge { .. })));
        assert!(errs
            .iter()
            .any(|e| matches!(e, ConfigError::SsrfAllowlistInvalidHost { .. })));
        assert!(errs
            .iter()
            .any(|e| matches!(e, ConfigError::ProofArchiveAgingTooLarge { .. })));
    }

    #[test]
    fn config_error_display_includes_actual_values() {
        // Deny banners need actionable detail. Each Display impl must
        // include the offending value so an operator can search the
        // log for it.
        let err = ConfigError::EvidenceLimitTooLarge {
            actual: 200,
            ceiling: 100,
        };
        let msg = format!("{err}");
        assert!(msg.contains("200"));
        assert!(msg.contains("100"));
    }

    #[test]
    fn serde_round_trip_preserves_all_fields() {
        let mut cfg = SentinelConfig::default();
        cfg.proof_archive.aging_days = Some(90);
        cfg.signing.hardware_required = true;
        cfg.ssrf.allowlist.push("api.linear.app".to_string());

        let json = serde_json::to_string(&cfg).unwrap();
        let back: SentinelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.proof_archive.aging_days, Some(90));
        assert!(back.signing.hardware_required);
        assert!(back.ssrf.allowlist.contains(&"api.linear.app".to_string()));
    }

    #[test]
    fn toml_round_trip_preserves_all_fields() {
        // TOML is the production config format — pin the round-trip
        // explicitly. A TOML serializer change that drops a default
        // would corrupt existing configs silently.
        let mut cfg = SentinelConfig::default();
        cfg.proof_archive.aging_days = Some(30);
        let s = toml::to_string(&cfg).unwrap();
        let back: SentinelConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.proof_archive.aging_days, Some(30));
    }
}
