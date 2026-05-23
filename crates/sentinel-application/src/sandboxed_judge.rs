//! Sandboxed judge wrapper (sentinel #60 / M7.11 — EQTY Lab pattern).
//!
//! Wraps any [`JudgeService`] implementation in a layered sandbox that
//! constrains how much the inner judge can spend before its verdict
//! lands. The "EQTY Lab pattern" referenced in the task description
//! is the principle that AI judges — especially OSS ones called over
//! third-party providers — should not be trusted to behave nicely.
//! A judge that:
//!
//! - hangs forever waiting on a wedged provider
//! - returns a giant response that exhausts memory
//! - panics on malformed input
//!
//! …should not be able to bring down sentinel. The wrapper isolates
//! all three of those failure modes without spawning a separate OS
//! process (which would require IPC + serialization and a much larger
//! surface). Future work can layer real subprocess isolation on top —
//! the wrapper's interface is stable.
//!
//! ## What the sandbox enforces
//!
//! - **Wall-clock timeout** — each `evaluate` / `evaluate_step` /
//!   `evaluate_multi` call gets a maximum duration. Exceeding it
//!   yields a synthetic failure verdict (or `Err` from the trait
//!   method, depending on the call shape), never a hang.
//!
//! - **Panic isolation** — the inner judge is invoked via
//!   `tokio::spawn`, so a panic in the judge becomes a `JoinError`
//!   at this layer rather than unwinding the calling task. The
//!   sandbox converts the JoinError to a `sufficient=false` verdict
//!   tagged `PANIC:` so callers see something happened.
//!
//! - **Predictable error shape** — every sandboxed failure produces
//!   the same kind of result a per-judge error would in
//!   `evaluate_multi`: `sufficient=false`, confidence=0.0, reasoning
//!   prefixed with `SANDBOX:` (timeout) or `PANIC:` (panic). Callers
//!   that do strict checks can filter on those prefixes.
//!
//! ## What the sandbox does NOT enforce (yet)
//!
//! - Memory caps. The inner judge runs in the same process, so a
//!   judge that allocates 8GB of strings before returning still
//!   eats parent-process memory. Mitigating that needs a real
//!   subprocess; out of scope for this slice.
//! - Network egress controls. The judge talks to its provider over
//!   HTTPS through whatever client the implementer wired. SSRF
//!   protections layer one level up via the `ssrf` domain module.
//! - CPU caps. Same reason as memory.
//!
//! Documenting these limits explicitly so the next person reaching
//! for "sandboxed judge" doesn't assume it does more than it does.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::judge_service::JudgeService;
use sentinel_domain::evidence::Evidence;
use sentinel_domain::judge::{JudgeModel, JudgeVerdict};
use sentinel_domain::multi_judge::{JudgeTrustTier, MultiJudgeVerdict};

/// Default per-call timeout when none is supplied. 60 seconds is
/// generous for an LLM round-trip but firm enough that a wedged
/// provider doesn't hang sentinel indefinitely. Override per-instance
/// via [`SandboxedJudge::with_timeout`] when calling slower models
/// (e.g. Opus on long contexts).
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Sandboxed wrapper around any `JudgeService`. Enforces wall-clock
/// timeout + panic isolation around the inner judge.
pub struct SandboxedJudge {
    inner: Arc<dyn JudgeService>,
    timeout: Duration,
}

impl SandboxedJudge {
    /// Wrap an existing judge with the default 60-second timeout.
    pub fn new(inner: Arc<dyn JudgeService>) -> Self {
        Self {
            inner,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    /// Wrap an existing judge with a custom timeout.
    pub fn with_timeout(inner: Arc<dyn JudgeService>, timeout: Duration) -> Self {
        Self { inner, timeout }
    }

    /// Synthetic verdict for "the sandbox killed the call." Tagged so
    /// callers that scan for SANDBOX:/PANIC: prefixes can filter.
    fn sandbox_timeout_verdict(timeout: Duration) -> JudgeVerdict {
        JudgeVerdict::fail(
            0.0,
            format!(
                "SANDBOX: judge call exceeded {}s timeout — treated as insufficient",
                timeout.as_secs()
            ),
            vec![],
        )
    }

    /// Synthetic verdict for "the inner judge panicked."
    fn sandbox_panic_verdict(detail: &str) -> JudgeVerdict {
        JudgeVerdict::fail(
            0.0,
            format!("PANIC: judge panicked inside sandbox: {detail}"),
            vec![],
        )
    }
}

#[async_trait]
impl JudgeService for SandboxedJudge {
    async fn evaluate(
        &self,
        skill: &str,
        phase_id: &str,
        phase_objectives: &str,
        evidence: &Evidence,
        model: JudgeModel,
    ) -> Result<JudgeVerdict> {
        let inner = Arc::clone(&self.inner);
        let skill = skill.to_string();
        let phase_id = phase_id.to_string();
        let objectives = phase_objectives.to_string();
        let evidence = evidence.clone();
        let timeout = self.timeout;

        // Spawn the inner call so a panic becomes a JoinError instead of
        // unwinding the caller's task.
        let handle = tokio::spawn(async move {
            inner
                .evaluate(&skill, &phase_id, &objectives, &evidence, model)
                .await
        });

        match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(Ok(verdict))) => Ok(verdict),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(join_err)) => {
                // Inner task panicked or was cancelled.
                Ok(Self::sandbox_panic_verdict(&join_err.to_string()))
            }
            Err(_elapsed) => Ok(Self::sandbox_timeout_verdict(timeout)),
        }
    }

    /// Override the default `evaluate_step` so step-level calls also
    /// honor the sandbox. Without this override, the default trait
    /// impl would route through `Self::evaluate` (which IS sandboxed)
    /// but lose any inner-judge-specific step prompting. Going
    /// through `inner.evaluate_step` preserves richer prompting AND
    /// applies the sandbox.
    async fn evaluate_step(
        &self,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        step_description: &str,
        evidence: &Evidence,
        model: JudgeModel,
    ) -> Result<JudgeVerdict> {
        let inner = Arc::clone(&self.inner);
        let skill = skill.to_string();
        let phase_id = phase_id.to_string();
        let step_id = step_id.to_string();
        let step_description = step_description.to_string();
        let evidence = evidence.clone();
        let timeout = self.timeout;

        let handle = tokio::spawn(async move {
            inner
                .evaluate_step(
                    &skill,
                    &phase_id,
                    &step_id,
                    &step_description,
                    &evidence,
                    model,
                )
                .await
        });

        match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(Ok(verdict))) => Ok(verdict),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(join_err)) => Ok(Self::sandbox_panic_verdict(&join_err.to_string())),
            Err(_elapsed) => Ok(Self::sandbox_timeout_verdict(timeout)),
        }
    }

    /// Override `evaluate_multi` so the sandbox applies to the inner
    /// judge's `evaluate_multi` (which may have an overridden parallel
    /// implementation worth preserving). Same wrap-with-timeout-and-
    /// panic-isolation pattern. Per-judge errors INSIDE the inner
    /// multi-call already become ERROR JudgeRuns by the trait's
    /// default contract; the sandbox layers on top of that.
    async fn evaluate_multi(
        &self,
        skill: &str,
        phase_id: &str,
        phase_objectives: &str,
        evidence: &Evidence,
        tier: JudgeTrustTier,
    ) -> Result<MultiJudgeVerdict> {
        let inner = Arc::clone(&self.inner);
        let skill = skill.to_string();
        let phase_id = phase_id.to_string();
        let objectives = phase_objectives.to_string();
        let evidence = evidence.clone();
        let timeout = self.timeout;

        let handle = tokio::spawn(async move {
            inner
                .evaluate_multi(&skill, &phase_id, &objectives, &evidence, tier)
                .await
        });

        match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(Ok(verdict))) => Ok(verdict),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(join_err)) => {
                // The inner multi-call panicked. Surface a multi-verdict
                // built from a single PANIC run — callers that consume
                // MultiJudgeVerdict still get a typed answer.
                let panic_run = sentinel_domain::multi_judge::JudgeRun {
                    model: tier
                        .judge_models()
                        .first()
                        .copied()
                        .unwrap_or(JudgeModel::Kimi),
                    verdict: Self::sandbox_panic_verdict(&join_err.to_string()),
                    cost_usd: None,
                    provider: Some("sandbox".to_string()),
                };
                Ok(MultiJudgeVerdict::synthesize(tier, vec![panic_run]))
            }
            Err(_elapsed) => {
                // Timeout — synthesize a single-run multi-verdict
                // marking SANDBOX timeout.
                let timeout_run = sentinel_domain::multi_judge::JudgeRun {
                    model: tier
                        .judge_models()
                        .first()
                        .copied()
                        .unwrap_or(JudgeModel::Kimi),
                    verdict: Self::sandbox_timeout_verdict(timeout),
                    cost_usd: None,
                    provider: Some("sandbox".to_string()),
                };
                Ok(MultiJudgeVerdict::synthesize(tier, vec![timeout_run]))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A judge that sleeps `delay` before returning. Used to test
    /// timeout enforcement.
    struct SlowJudge {
        delay: Duration,
    }

    #[async_trait]
    impl JudgeService for SlowJudge {
        async fn evaluate(
            &self,
            _skill: &str,
            _phase_id: &str,
            _phase_objectives: &str,
            _evidence: &Evidence,
            _model: JudgeModel,
        ) -> Result<JudgeVerdict> {
            tokio::time::sleep(self.delay).await;
            Ok(JudgeVerdict::pass(0.95, "slow but ok"))
        }
    }

    /// A judge that panics on the Nth call. Tests panic isolation.
    struct PanickingJudge {
        calls_until_panic: AtomicUsize,
    }

    #[async_trait]
    impl JudgeService for PanickingJudge {
        async fn evaluate(
            &self,
            _skill: &str,
            _phase_id: &str,
            _phase_objectives: &str,
            _evidence: &Evidence,
            _model: JudgeModel,
        ) -> Result<JudgeVerdict> {
            let remaining = self.calls_until_panic.fetch_sub(1, Ordering::SeqCst);
            if remaining == 0 {
                panic!("simulated judge panic");
            }
            Ok(JudgeVerdict::pass(0.9, "no panic this time"))
        }
    }

    /// A normal, fast judge — verifies the happy path doesn't pay
    /// timeout overhead.
    struct FastJudge;

    #[async_trait]
    impl JudgeService for FastJudge {
        async fn evaluate(
            &self,
            _skill: &str,
            _phase_id: &str,
            _phase_objectives: &str,
            _evidence: &Evidence,
            _model: JudgeModel,
        ) -> Result<JudgeVerdict> {
            Ok(JudgeVerdict::pass(0.91, "fast pass"))
        }
    }

    #[tokio::test]
    async fn fast_judge_passes_through_unchanged() {
        let sandbox = SandboxedJudge::new(Arc::new(FastJudge));
        let verdict = sandbox
            .evaluate(
                "linear",
                "claim",
                "fetch ticket",
                &Evidence::default(),
                JudgeModel::Kimi,
            )
            .await
            .unwrap();
        assert!(verdict.sufficient);
        assert!(verdict.confidence > 0.9);
        assert_eq!(verdict.reasoning, "fast pass");
    }

    #[tokio::test]
    async fn timeout_replaces_hang_with_synthetic_verdict() {
        let slow = SlowJudge {
            // Much longer than the sandbox timeout.
            delay: Duration::from_secs(5),
        };
        let sandbox = SandboxedJudge::with_timeout(Arc::new(slow), Duration::from_millis(50));
        let verdict = sandbox
            .evaluate(
                "linear",
                "claim",
                "fetch",
                &Evidence::default(),
                JudgeModel::Kimi,
            )
            .await
            .unwrap();
        assert!(!verdict.sufficient, "timeout must yield insufficient");
        assert!(
            verdict.reasoning.starts_with("SANDBOX:"),
            "reasoning must be SANDBOX-tagged: {}",
            verdict.reasoning
        );
        assert_eq!(verdict.confidence, 0.0);
    }

    #[tokio::test]
    async fn panic_in_inner_judge_becomes_panic_verdict() {
        let panicking = PanickingJudge {
            calls_until_panic: AtomicUsize::new(0), // first call panics
        };
        let sandbox = SandboxedJudge::new(Arc::new(panicking));
        let verdict = sandbox
            .evaluate(
                "linear",
                "claim",
                "fetch",
                &Evidence::default(),
                JudgeModel::Kimi,
            )
            .await
            .unwrap();
        assert!(!verdict.sufficient);
        assert!(
            verdict.reasoning.starts_with("PANIC:"),
            "panic must be PANIC-tagged: {}",
            verdict.reasoning
        );
    }

    #[tokio::test]
    async fn timeout_applies_to_evaluate_step_path_too() {
        let slow = SlowJudge {
            delay: Duration::from_secs(5),
        };
        let sandbox = SandboxedJudge::with_timeout(Arc::new(slow), Duration::from_millis(50));
        let verdict = sandbox
            .evaluate_step(
                "linear",
                "review",
                "3.L3",
                "open PR",
                &Evidence::default(),
                JudgeModel::Kimi,
            )
            .await
            .unwrap();
        assert!(!verdict.sufficient);
        assert!(verdict.reasoning.starts_with("SANDBOX:"));
    }

    #[tokio::test]
    async fn timeout_applies_to_evaluate_multi_path_too() {
        let slow = SlowJudge {
            delay: Duration::from_secs(5),
        };
        let sandbox = SandboxedJudge::with_timeout(Arc::new(slow), Duration::from_millis(50));
        let verdict = sandbox
            .evaluate_multi(
                "linear",
                "review",
                "open PR",
                &Evidence::default(),
                JudgeTrustTier::Critical,
            )
            .await
            .unwrap();
        // The whole multi-call timed out → a single SANDBOX run was
        // synthesized; the multi-verdict reflects that.
        assert!(!verdict.sufficient);
        assert_eq!(verdict.individuals.len(), 1);
        assert!(verdict.individuals[0]
            .verdict
            .reasoning
            .starts_with("SANDBOX:"));
        assert_eq!(verdict.individuals[0].provider.as_deref(), Some("sandbox"));
    }

    #[tokio::test]
    async fn inner_judge_anyhow_errors_propagate_as_err() {
        // Anyhow errors from the inner judge (e.g. "no API key") must
        // still propagate as Err — they're configuration errors, not
        // sandbox-isolation events. The sandbox only catches HANGS
        // and PANICS.
        struct ErrorJudge;
        #[async_trait]
        impl JudgeService for ErrorJudge {
            async fn evaluate(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &Evidence,
                _: JudgeModel,
            ) -> Result<JudgeVerdict> {
                anyhow::bail!("no API key configured")
            }
        }
        let sandbox = SandboxedJudge::new(Arc::new(ErrorJudge));
        let err = sandbox
            .evaluate(
                "linear",
                "claim",
                "fetch",
                &Evidence::default(),
                JudgeModel::Kimi,
            )
            .await
            .expect_err("config errors must propagate as Err");
        assert!(err.to_string().contains("no API key"));
    }
}
