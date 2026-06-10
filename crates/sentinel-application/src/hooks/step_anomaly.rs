//! Step Anomaly Hook (M1.9)
//!
//! AEGIS-borrowed pattern: 9-dimensional behavioral anomaly detection that
//! layers on top of [`step_judge`](crate::hooks::step_judge). Where
//! `step_judge` asks "did the work succeed?" against this single
//! invocation's evidence, `step_anomaly` asks "does this invocation
//! *look like* a normal run of this step?" against the historical
//! distribution of prior runs.
//!
//! The two answers are complementary: a step can pass the AI judge
//! (verdict.sufficient = true) while behaving anomalously (e.g. ran in
//! 30s when the typical run is 200ms). The judge thinks the work was
//! done; the anomaly detector flags that it was done in an unusual
//! way. Operators decide what to do with that signal — a high-stakes
//! step config can declare `block_on_anomaly = true` for refusal,
//! routine steps just record the anomaly to the proof for telemetry.
//!
//! # The 9 dimensions (AEGIS framework)
//!
//! 1. **Tool novelty** — agent invoked a tool it has never used before
//! 2. **Frequency spike** — step ran more than N times in a window
//! 3. **Argument shape drift** — `tool_input` JSON shape differs from prior runs
//! 4. **Payload size outliers** — `tool_input/result` bytes >> typical
//! 5. **Duration outliers** — step `duration_ms` >> typical
//! 6. **Sequence anomaly** — step ran out of expected order
//! 7. **Cost spike** — token spend / API cost >> typical
//! 8. **Risk escalation** — step elevated trust tier mid-chain
//! 9. **Session burst** — many steps fired in a short wall-clock window
//!
//! # M1.9 scope: framework + 3 detectors
//!
//! Today this module ships the [`AnomalyDetector`] trait, the
//! [`StepAnomalyReport`] aggregator, and three concrete detectors:
//! [`ArgumentShapeDriftDetector`], [`PayloadSizeDetector`],
//! [`DurationOutlierDetector`]. The other six dimensions have
//! placeholder structs with TODO bodies — extension points are
//! defined so future work fills them in without re-architecting.
//!
//! This is intentionally observation-only: the hook never returns
//! `HookOutput::deny`. Anomalies attach to the [`StepJudgeOutcome`]
//! flow as a side channel; downstream callers (M1.5
//! `submit_step_evidence`, M6 dashboard) consume the report and
//! decide what to do with it.

use std::collections::BTreeMap;

use sentinel_domain::evidence::Evidence;
use sentinel_domain::proof::{ProofChain, ProofEntry};
use sentinel_domain::step_proof::StepProof;
use serde::{Deserialize, Serialize};

/// One concrete anomaly observation. Multiple anomalies can fire on a
/// single step run — the [`StepAnomalyReport`] is the aggregate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Anomaly {
    /// Which of the 9 AEGIS dimensions this anomaly belongs to.
    pub dimension: AnomalyDimension,

    /// Severity: how far the observation deviated from baseline.
    /// 0.0 = no anomaly (shouldn't appear in the report); 1.0 = a single
    /// standard deviation; 3.0 = three-sigma; values >5 indicate
    /// catastrophic deviation.
    pub severity: f64,

    /// Human-readable explanation. Surfaced in dashboards and proof
    /// chain telemetry, NOT in judge prompts (judge sees the verdict,
    /// not the meta-observations).
    pub reasoning: String,

    /// The observed value (whatever shape — bytes, ms, JSON-shape hash)
    /// for cross-referencing in telemetry. Free-form so different
    /// detectors can stash type-specific data.
    #[serde(default)]
    pub observed: serde_json::Value,

    /// The baseline this observation deviated from. Same shape conventions
    /// as `observed` — what "normal" looked like for this step.
    #[serde(default)]
    pub baseline: serde_json::Value,
}

/// The 9 AEGIS anomaly dimensions. Used in `Anomaly::dimension` for
/// filtering and routing in dashboards.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum AnomalyDimension {
    ToolNovelty,
    FrequencySpike,
    ArgumentShapeDrift,
    PayloadSize,
    Duration,
    SequenceAnomaly,
    CostSpike,
    RiskEscalation,
    SessionBurst,
    /// 10th dimension (Praetorian-inspired): the step's output is
    /// near-identical to recent prior runs of the same step — a
    /// token-burning "stuck loop" where the agent re-emits the same
    /// result without making progress.
    OutputSimilarity,
}

/// Aggregate of every anomaly fired for a single step run. Empty
/// `anomalies` means "no anomalies detected" — the canonical clean
/// case. The report is a side-channel artifact, not part of the
/// proof chain itself; future M1.9+ work can fold a hash of this
/// into the `StepProof`'s `evidence.custom` if customers want
/// anomaly-aware audit trails.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepAnomalyReport {
    /// Every anomaly observed during this step run, across all 9
    /// dimensions. Empty Vec = clean run.
    pub anomalies: Vec<Anomaly>,

    /// Aggregate severity = max severity across all dimensions. Lets
    /// step configs declare `block_on_anomaly_severity = 3.0` without
    /// the config layer iterating the array itself.
    #[serde(default)]
    pub max_severity: f64,
}

impl StepAnomalyReport {
    /// Convenience: returns `true` iff the report contains zero anomalies.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.anomalies.is_empty()
    }

    /// Add an anomaly; updates `max_severity`. Idempotent on identical
    /// duplicates would double-count; detectors are responsible for
    /// not firing the same dimension twice for one run.
    pub fn push(&mut self, anomaly: Anomaly) {
        if anomaly.severity > self.max_severity {
            self.max_severity = anomaly.severity;
        }
        self.anomalies.push(anomaly);
    }
}

/// One detector for one dimension. Detectors are stateless — they read
/// the current step's [`Evidence`] + the historical [`ProofChain`] and
/// emit at most one [`Anomaly`]. Detectors that need to fire multiple
/// anomalies should split into separate impls.
pub trait AnomalyDetector: Send + Sync {
    /// Which dimension this detector covers.
    fn dimension(&self) -> AnomalyDimension;

    /// Inspect the current step against historical prior steps. Returns
    /// `None` when the observation is within baseline; `Some(Anomaly)`
    /// when it deviates beyond the detector's threshold.
    fn detect(
        &self,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        current_evidence: &Evidence,
        current_duration_ms: u64,
        history: &ProofChain,
    ) -> Option<Anomaly>;
}

// ─── Detector 1: ArgumentShapeDriftDetector ────────────────────────────
//
// Compares the JSON shape (key set + nesting structure) of the current
// tool_input against the modal shape across prior runs of this step.
// "Shape" = sorted top-level keys, recursively. Different shape signals
// the agent invoked the step with a substantially different argument
// structure than past runs.

/// Compute a stable JSON-shape descriptor: sorted top-level keys joined
/// with `:`, recursing one level into nested objects. Deliberately not
/// a hash — humans can read shape descriptors in dashboards and spot
/// the difference between `{a,b,c}` and `{a,b,d}` without decoding.
fn shape_descriptor(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(std::string::String::as_str).collect();
            keys.sort_unstable();
            let inner = keys
                .iter()
                .map(|k| {
                    let child_shape = match map.get(*k) {
                        Some(serde_json::Value::Object(_)) => "{...}",
                        Some(serde_json::Value::Array(_)) => "[...]",
                        Some(serde_json::Value::String(_)) => "str",
                        Some(serde_json::Value::Number(_)) => "num",
                        Some(serde_json::Value::Bool(_)) => "bool",
                        _ => "?",
                    };
                    format!("{k}:{child_shape}")
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
        serde_json::Value::Array(_) => "[...]".to_string(),
        _ => "scalar".to_string(),
    }
}

/// Detector for [`AnomalyDimension::ArgumentShapeDrift`].
pub struct ArgumentShapeDriftDetector;

impl AnomalyDetector for ArgumentShapeDriftDetector {
    fn dimension(&self) -> AnomalyDimension {
        AnomalyDimension::ArgumentShapeDrift
    }

    fn detect(
        &self,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        current_evidence: &Evidence,
        _current_duration_ms: u64,
        history: &ProofChain,
    ) -> Option<Anomaly> {
        let current_input = current_evidence.custom.get("step_tool_input")?.clone();
        let current_shape = shape_descriptor(&current_input);

        // Build the modal shape across prior runs of this same
        // (skill, phase_id, step_id). At least one prior run is
        // required to compute a meaningful baseline.
        let mut shape_counts: BTreeMap<String, u64> = BTreeMap::new();
        for entry in &history.entries {
            if let ProofEntry::Step(s) = entry {
                if s.skill == skill && s.phase_id == phase_id && s.step_id == step_id {
                    if let Some(prior_input) = s.evidence.custom.get("step_tool_input") {
                        *shape_counts
                            .entry(shape_descriptor(prior_input))
                            .or_insert(0) += 1;
                    }
                }
            }
        }
        if shape_counts.is_empty() {
            return None; // first run — no baseline to deviate from
        }

        let modal_shape = shape_counts
            .iter()
            .max_by_key(|(_, c)| *c)
            .map(|(k, _)| k.clone())?;

        if current_shape == modal_shape {
            return None;
        }

        // Severity scales with how rare the *current* shape is in
        // history: never-seen-before = high, seen-once-before = low.
        let current_count = shape_counts.get(&current_shape).copied().unwrap_or(0);
        let total: u64 = shape_counts.values().sum();
        let novelty = if total == 0 {
            0.0
        } else {
            1.0 - (current_count as f64 / total as f64)
        };
        let severity = (novelty * 3.0).clamp(0.5, 5.0);

        Some(Anomaly {
            dimension: AnomalyDimension::ArgumentShapeDrift,
            severity,
            reasoning: format!(
                "tool_input shape '{current_shape}' differs from modal shape \
                 '{modal_shape}' (seen {current_count}/{total} prior runs)",
            ),
            observed: serde_json::json!({"shape": current_shape, "count": current_count}),
            baseline: serde_json::json!({"modal_shape": modal_shape, "total_prior": total}),
        })
    }
}

// ─── Detector 2: PayloadSizeDetector ───────────────────────────────────
//
// Flags when serialized tool_input or tool_result bytes are unusually
// large or small relative to historical runs. Threshold: 3x median
// (high-side outlier) or <0.2x median (low-side; truncated/missing
// payload).

/// Median size of a slice (caller pre-extracts sizes). Not a precise
/// median — for small samples (n<5) we just take the mean to avoid
/// degenerate single-sample baselines.
fn central_tendency(sizes: &[usize]) -> Option<f64> {
    if sizes.is_empty() {
        return None;
    }
    if sizes.len() < 5 {
        let sum: usize = sizes.iter().sum();
        return Some(sum as f64 / sizes.len() as f64);
    }
    let mut sorted = sizes.to_vec();
    sorted.sort_unstable();
    Some(sorted[sorted.len() / 2] as f64)
}

/// Detector for [`AnomalyDimension::PayloadSize`].
pub struct PayloadSizeDetector;

impl AnomalyDetector for PayloadSizeDetector {
    fn dimension(&self) -> AnomalyDimension {
        AnomalyDimension::PayloadSize
    }

    fn detect(
        &self,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        current_evidence: &Evidence,
        _current_duration_ms: u64,
        history: &ProofChain,
    ) -> Option<Anomaly> {
        let current_size = serde_json::to_string(&current_evidence.custom)
            .map_or(0, |s| s.len());
        if current_size == 0 {
            return None;
        }

        let prior_sizes: Vec<usize> = history
            .entries
            .iter()
            .filter_map(|e| match e {
                ProofEntry::Step(s)
                    if s.skill == skill && s.phase_id == phase_id && s.step_id == step_id =>
                {
                    serde_json::to_string(&s.evidence.custom)
                        .map(|st| st.len())
                        .ok()
                }
                _ => None,
            })
            .collect();

        let baseline = central_tendency(&prior_sizes)?;
        if baseline < 1.0 {
            return None;
        }

        let ratio = current_size as f64 / baseline;
        // Out-of-band thresholds: <0.2x or >3x baseline.
        if (0.2..=3.0).contains(&ratio) {
            return None;
        }

        let severity = if ratio > 3.0 {
            ((ratio - 3.0) / 3.0 + 1.0).clamp(1.0, 5.0)
        } else {
            // ratio < 0.2 — truncated/missing payload.
            ((0.2 - ratio) / 0.2 + 1.0).clamp(1.0, 5.0)
        };

        Some(Anomaly {
            dimension: AnomalyDimension::PayloadSize,
            severity,
            reasoning: format!(
                "evidence payload size {current_size} bytes vs baseline {baseline:.0} \
                 (ratio {ratio:.2}x — {} threshold)",
                if ratio > 3.0 {
                    "above 3x"
                } else {
                    "below 0.2x"
                },
            ),
            observed: serde_json::json!({"bytes": current_size}),
            baseline: serde_json::json!({"bytes": baseline, "n_prior": prior_sizes.len()}),
        })
    }
}

// ─── Detector 3: DurationOutlierDetector ───────────────────────────────
//
// Same shape as PayloadSize but on duration_ms instead of payload bytes.
// A step that suddenly takes 30 seconds when historical runs were 200ms
// is informative even when the verdict passes — could signal upstream
// API degradation, hung subprocess, or evidence-collection drift.

/// Detector for [`AnomalyDimension::Duration`].
pub struct DurationOutlierDetector;

impl AnomalyDetector for DurationOutlierDetector {
    fn dimension(&self) -> AnomalyDimension {
        AnomalyDimension::Duration
    }

    fn detect(
        &self,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        _current_evidence: &Evidence,
        current_duration_ms: u64,
        history: &ProofChain,
    ) -> Option<Anomaly> {
        if current_duration_ms == 0 {
            return None;
        }

        let prior_durations: Vec<usize> = history
            .entries
            .iter()
            .filter_map(|e| match e {
                ProofEntry::Step(s)
                    if s.skill == skill && s.phase_id == phase_id && s.step_id == step_id =>
                {
                    Some(s.duration_ms as usize)
                }
                _ => None,
            })
            .collect();

        let baseline = central_tendency(&prior_durations)?;
        if baseline < 1.0 {
            return None;
        }

        let ratio = current_duration_ms as f64 / baseline;
        // Duration anomaly: >5x baseline (slow-side only — fast steps
        // are usually a good sign, not a problem).
        if ratio <= 5.0 {
            return None;
        }

        let severity = ((ratio - 5.0) / 5.0 + 1.0).clamp(1.0, 5.0);

        Some(Anomaly {
            dimension: AnomalyDimension::Duration,
            severity,
            reasoning: format!(
                "step duration {current_duration_ms}ms vs baseline {baseline:.0}ms \
                 (ratio {ratio:.2}x — above 5x threshold)",
            ),
            observed: serde_json::json!({"duration_ms": current_duration_ms}),
            baseline: serde_json::json!({"duration_ms": baseline, "n_prior": prior_durations.len()}),
        })
    }
}

/// Detector for [`AnomalyDimension::OutputSimilarity`] — the stuck-loop
/// catcher. Compares the current step's output against recent prior runs
/// of the same `(skill, phase_id, step_id)`; if the output is ≥90%
/// similar (line-level Jaccard) to a recent run, the agent is likely
/// spinning — re-emitting the same result without progress. Severity
/// scales with how many recent runs collide.
pub struct OutputSimilarityDetector;

/// Read a step's output payload from `evidence.custom`. Outputs are stored
/// under `step_tool_result` (the seal-time convention); fall back to
/// `step_tool_output` for forward-compat. Returns the serialized JSON
/// string so the comparison is over a stable textual form.
fn step_output_text(evidence: &Evidence) -> Option<String> {
    evidence
        .custom
        .get("step_tool_result")
        .or_else(|| evidence.custom.get("step_tool_output"))
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .filter(|s| !s.is_empty() && s != "null")
}

/// Line-level Jaccard similarity of two texts: |A∩B| / |A∪B| over the
/// SET of non-blank trimmed lines. 1.0 = identical line sets, 0.0 =
/// disjoint. Cheap, dependency-free, and robust to reordering — the
/// right grain for "did the agent just repeat itself".
fn line_jaccard(a: &str, b: &str) -> f64 {
    use std::collections::BTreeSet;
    let set_a: BTreeSet<&str> = a.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let set_b: BTreeSet<&str> = b.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if set_a.is_empty() && set_b.is_empty() {
        return 1.0; // two empty outputs ARE identical
    }
    let inter = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// How many recent prior runs to compare against (the "loop window").
const SIMILARITY_WINDOW: usize = 3;
/// Similarity at/above which two outputs count as "the same".
const SIMILARITY_THRESHOLD: f64 = 0.90;

impl AnomalyDetector for OutputSimilarityDetector {
    fn dimension(&self) -> AnomalyDimension {
        AnomalyDimension::OutputSimilarity
    }

    fn detect(
        &self,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        current_evidence: &Evidence,
        _current_duration_ms: u64,
        history: &ProofChain,
    ) -> Option<Anomaly> {
        let current = step_output_text(current_evidence)?;

        // Most-recent-first prior outputs for the same step.
        let mut prior: Vec<String> = history
            .entries
            .iter()
            .rev()
            .filter_map(|e| match e {
                ProofEntry::Step(s)
                    if s.skill == skill && s.phase_id == phase_id && s.step_id == step_id =>
                {
                    step_output_text(&s.evidence)
                }
                _ => None,
            })
            .collect();
        prior.truncate(SIMILARITY_WINDOW);
        if prior.is_empty() {
            return None; // first run — no baseline
        }

        // Count how many of the recent window are ≥ threshold-similar.
        let collisions = prior
            .iter()
            .filter(|p| line_jaccard(&current, p) >= SIMILARITY_THRESHOLD)
            .count();
        if collisions == 0 {
            return None;
        }

        let max_sim = prior
            .iter()
            .map(|p| line_jaccard(&current, p))
            .fold(0.0_f64, f64::max);
        // Severity ramps with the number of colliding recent runs: a single
        // repeat is a nudge (≈2.0), the whole window repeating is a hard
        // stuck-loop signal (→5.0).
        let severity = (collisions as f64).mul_add(1.5, 1.0).clamp(2.0, 5.0);

        Some(Anomaly {
            dimension: AnomalyDimension::OutputSimilarity,
            severity,
            reasoning: format!(
                "step output is {:.0}% similar to {collisions} of the last \
                 {} run(s) — likely a stuck loop re-emitting the same result \
                 without progress",
                max_sim * 100.0,
                prior.len(),
            ),
            observed: serde_json::json!({"max_similarity": max_sim, "collisions": collisions}),
            baseline: serde_json::json!({"threshold": SIMILARITY_THRESHOLD, "window": prior.len()}),
        })
    }
}

// ─── Composite hook entry point ────────────────────────────────────────

/// Return the default set of detectors the hook ships with.
///
/// The other 6 dimensions are intentionally absent here — they're
/// filed as M1.9-followup deliverables. New detectors register by
/// implementing [`AnomalyDetector`] and being added to this list (or
/// passed in via a custom slice when callers want a different set).
#[must_use]
pub fn default_detectors() -> Vec<Box<dyn AnomalyDetector>> {
    vec![
        Box::new(ArgumentShapeDriftDetector),
        Box::new(PayloadSizeDetector),
        Box::new(DurationOutlierDetector),
        Box::new(OutputSimilarityDetector),
    ]
}

/// Run every detector against the current step run. Returns the
/// aggregate report.
///
/// Observation-only — never blocks. Caller (typically M1.5's
/// `submit_step_evidence`) decides what to do with the report:
/// attach to the `StepProof`'s `evidence.custom`, surface in dashboard
/// telemetry, or refuse to seal when severity exceeds a threshold.
#[must_use]
pub fn run_detectors(
    detectors: &[Box<dyn AnomalyDetector>],
    skill: &str,
    phase_id: &str,
    step_id: &str,
    current_evidence: &Evidence,
    current_duration_ms: u64,
    history: &ProofChain,
) -> StepAnomalyReport {
    let mut report = StepAnomalyReport::default();
    for detector in detectors {
        if let Some(anomaly) = detector.detect(
            skill,
            phase_id,
            step_id,
            current_evidence,
            current_duration_ms,
            history,
        ) {
            report.push(anomaly);
        }
    }
    report
}

#[allow(unused_imports)]
const fn _stub_unused_step_proof(_p: &StepProof) {}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sentinel_domain::judge::JudgeVerdict;
    use sentinel_domain::proof::GENESIS_HASH;

    /// Build a fake StepProof entry for seeding chain history.
    fn historical_step(
        skill: &str,
        phase_id: &str,
        step_id: &str,
        previous_hash: &str,
        custom: serde_json::Value,
        duration_ms: u64,
    ) -> StepProof {
        let mut evidence = Evidence::default();
        evidence.custom = custom;
        let evidence_hash = StepProof::compute_evidence_hash(&evidence);
        let artifact = serde_json::Value::Null;
        let artifact_hash = StepProof::compute_artifact_hash(&artifact);
        let combined_hash = StepProof::compute_combined_hash(
            step_id,
            phase_id,
            skill,
            &evidence_hash,
            &artifact_hash,
            previous_hash,
        );
        StepProof {
            step_id: step_id.into(),
            phase_id: phase_id.into(),
            skill: skill.into(),
            session_id: "test".into(),
            evidence,
            evidence_hash,
            artifact,
            artifact_hash,
            account_context: None,
            previous_hash: previous_hash.into(),
            combined_hash,
            judge_model: "sonnet".into(),
            judge_verdict: JudgeVerdict::pass(0.9, "ok"),
            signature: None,
            trace_context: None,
            started_at: Utc::now() - chrono::Duration::seconds(1),
            completed_at: Utc::now(),
            duration_ms,
        }
    }

    fn seeded_chain_uniform_shape(
        skill: &str,
        phase_id: &str,
        step_id: &str,
        n: usize,
    ) -> ProofChain {
        let mut chain = ProofChain::new(skill, "test");
        let mut prev = GENESIS_HASH.to_string();
        for i in 0..n {
            let custom = serde_json::json!({
                "step_tool_name": "Skill",
                "step_tool_input": {"ticket": format!("FPCRM-{i}")},
                "step_tool_result": {"ok": true},
            });
            let proof = historical_step(skill, phase_id, step_id, &prev, custom, 100);
            prev = proof.combined_hash.clone();
            chain.entries.push(ProofEntry::Step(proof));
        }
        chain
    }

    #[test]
    fn shape_descriptor_handles_objects_and_scalars() {
        let obj = serde_json::json!({"foo": "bar", "baz": 42});
        let s = shape_descriptor(&obj);
        // Keys appear sorted, regardless of input order.
        assert!(s.contains("baz:num"));
        assert!(s.contains("foo:str"));
        // Scalar inputs collapse to a single label.
        assert_eq!(
            shape_descriptor(&serde_json::Value::String("x".into())),
            "scalar"
        );
    }

    #[test]
    fn argument_shape_drift_silent_on_first_run() {
        // No history => no baseline => no anomaly. First run of any
        // step must be a clean report regardless of input shape.
        let chain = ProofChain::new("linear", "test");
        let mut evidence = Evidence::default();
        evidence.custom = serde_json::json!({
            "step_tool_input": {"ticket": "FPCRM-1"},
        });
        let result =
            ArgumentShapeDriftDetector.detect("linear", "claim", "1", &evidence, 100, &chain);
        assert!(result.is_none(), "first-run baseline must be empty");
    }

    #[test]
    fn argument_shape_drift_silent_when_shape_matches_modal() {
        // History: 5 prior runs all with {"ticket": str} shape.
        // Current run uses the same shape => no anomaly.
        let chain = seeded_chain_uniform_shape("linear", "claim", "1", 5);
        let mut evidence = Evidence::default();
        evidence.custom = serde_json::json!({
            "step_tool_input": {"ticket": "FPCRM-99"},
        });
        let result =
            ArgumentShapeDriftDetector.detect("linear", "claim", "1", &evidence, 100, &chain);
        assert!(result.is_none(), "matching modal shape must not fire");
    }

    #[test]
    fn argument_shape_drift_fires_when_shape_diverges() {
        let chain = seeded_chain_uniform_shape("linear", "claim", "1", 5);
        let mut evidence = Evidence::default();
        // New shape: keys are different from the modal {"ticket": str}.
        evidence.custom = serde_json::json!({
            "step_tool_input": {"branch": "main", "force": true},
        });
        let result =
            ArgumentShapeDriftDetector.detect("linear", "claim", "1", &evidence, 100, &chain);
        let anomaly = result.expect("drift must fire");
        assert_eq!(anomaly.dimension, AnomalyDimension::ArgumentShapeDrift);
        assert!(anomaly.severity > 0.0);
        assert!(anomaly.reasoning.contains("modal shape"));
    }

    #[test]
    fn payload_size_silent_within_threshold() {
        let chain = seeded_chain_uniform_shape("linear", "claim", "1", 5);
        let mut evidence = Evidence::default();
        // Same-ish payload size as historical runs.
        evidence.custom = serde_json::json!({
            "step_tool_name": "Skill",
            "step_tool_input": {"ticket": "FPCRM-99"},
            "step_tool_result": {"ok": true},
        });
        let result = PayloadSizeDetector.detect("linear", "claim", "1", &evidence, 100, &chain);
        assert!(result.is_none());
    }

    #[test]
    fn payload_size_fires_on_oversized_payload() {
        let chain = seeded_chain_uniform_shape("linear", "claim", "1", 5);
        let mut evidence = Evidence::default();
        // Massively oversized payload — 5KB of repeated junk.
        let bloat = "x".repeat(5000);
        evidence.custom = serde_json::json!({
            "step_tool_input": {"ticket": "FPCRM-99", "extra": bloat},
        });
        let result = PayloadSizeDetector.detect("linear", "claim", "1", &evidence, 100, &chain);
        let anomaly = result.expect("oversized payload must fire");
        assert_eq!(anomaly.dimension, AnomalyDimension::PayloadSize);
        assert!(anomaly.severity >= 1.0);
        assert!(anomaly.reasoning.contains("above 3x"));
    }

    #[test]
    fn duration_outlier_silent_within_5x_baseline() {
        let chain = seeded_chain_uniform_shape("linear", "claim", "1", 5);
        let evidence = Evidence::default();
        // 4x baseline (history is 100ms, this is 400ms) — under threshold.
        let result = DurationOutlierDetector.detect("linear", "claim", "1", &evidence, 400, &chain);
        assert!(result.is_none());
    }

    #[test]
    fn duration_outlier_fires_on_slow_step() {
        let chain = seeded_chain_uniform_shape("linear", "claim", "1", 5);
        let evidence = Evidence::default();
        // 30x baseline (3000ms vs 100ms) — way over threshold.
        let result =
            DurationOutlierDetector.detect("linear", "claim", "1", &evidence, 3000, &chain);
        let anomaly = result.expect("slow step must fire");
        assert_eq!(anomaly.dimension, AnomalyDimension::Duration);
        assert!(anomaly.severity >= 1.0);
        assert!(anomaly.reasoning.contains("ratio"));
    }

    #[test]
    fn run_detectors_returns_clean_report_when_all_silent() {
        let chain = ProofChain::new("linear", "test");
        let evidence = Evidence::default();
        let detectors = default_detectors();
        let report = run_detectors(&detectors, "linear", "claim", "1", &evidence, 100, &chain);
        assert!(report.is_clean());
        assert_eq!(report.max_severity, 0.0);
    }

    #[test]
    fn run_detectors_aggregates_multiple_anomalies() {
        // Seed a history that establishes baselines, then fire a step
        // that triggers BOTH payload-size AND duration outliers.
        let chain = seeded_chain_uniform_shape("linear", "claim", "1", 5);
        let mut evidence = Evidence::default();
        let bloat = "x".repeat(5000);
        evidence.custom = serde_json::json!({
            "step_tool_input": {"ticket": "FPCRM-99", "extra": bloat},
        });
        let detectors = default_detectors();
        let report = run_detectors(&detectors, "linear", "claim", "1", &evidence, 3000, &chain);
        assert!(!report.is_clean());
        assert!(
            report.anomalies.len() >= 2,
            "expected multiple anomalies, got {}",
            report.anomalies.len()
        );
        // max_severity must reflect the highest individual severity.
        let direct_max = report
            .anomalies
            .iter()
            .map(|a| a.severity)
            .fold(0.0_f64, f64::max);
        assert!((report.max_severity - direct_max).abs() < f64::EPSILON);
    }

    #[test]
    fn anomaly_serializes_with_kebab_case_dimension() {
        // Dimension serialization is part of the dashboard contract —
        // keep it stable across schema changes.
        let anomaly = Anomaly {
            dimension: AnomalyDimension::ArgumentShapeDrift,
            severity: 1.5,
            reasoning: "test".into(),
            observed: serde_json::Value::Null,
            baseline: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&anomaly).unwrap();
        assert!(
            json.contains(r#""dimension":"argument-shape-drift""#),
            "got: {json}"
        );
    }

    // ─── OutputSimilarity (stuck-loop) detector ───────────────────────────

    fn evidence_with_output(out: serde_json::Value) -> Evidence {
        let mut e = Evidence::default();
        e.custom = serde_json::json!({ "step_tool_result": out });
        e
    }

    /// Seed a chain where the same step emits the SAME output `n` times.
    fn seeded_chain_repeated_output(
        skill: &str,
        phase_id: &str,
        step_id: &str,
        out: &serde_json::Value,
        n: usize,
    ) -> ProofChain {
        let mut chain = ProofChain::new(skill, "test");
        let mut prev = GENESIS_HASH.to_string();
        for _ in 0..n {
            let custom = serde_json::json!({ "step_tool_result": out });
            let proof = historical_step(skill, phase_id, step_id, &prev, custom, 100);
            prev = proof.combined_hash.clone();
            chain.entries.push(ProofEntry::Step(proof));
        }
        chain
    }

    #[test]
    fn line_jaccard_basics() {
        assert!((line_jaccard("a\nb\nc", "a\nb\nc") - 1.0).abs() < f64::EPSILON);
        assert!((line_jaccard("a\nb\nc\nd", "a\nb\nc\ne") - 0.6).abs() < 1e-9); // 3∩ / 5∪
        assert!((line_jaccard("x\ny", "p\nq") - 0.0).abs() < f64::EPSILON);
        assert!((line_jaccard("", "") - 1.0).abs() < f64::EPSILON);
        // Whitespace/blank-line robust.
        assert!((line_jaccard("a\n\n b ", "a\nb") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn output_similarity_silent_on_first_run() {
        let chain = ProofChain::new("linear", "test");
        let ev = evidence_with_output(serde_json::json!({"msg": "doing the thing"}));
        let res = OutputSimilarityDetector.detect("linear", "p", "1", &ev, 100, &chain);
        assert!(res.is_none(), "first run has no baseline");
    }

    #[test]
    fn output_similarity_fires_on_repeated_output() {
        // Prior 3 runs all emitted the same multi-line output; current run
        // emits the same again → stuck loop.
        let out = serde_json::json!({"log": "line one\nline two\nline three\nstill stuck"});
        let chain = seeded_chain_repeated_output("linear", "p", "1", &out, 3);
        let ev = evidence_with_output(out.clone());
        let res = OutputSimilarityDetector.detect("linear", "p", "1", &ev, 100, &chain);
        let anomaly = res.expect("identical repeated output must fire");
        assert_eq!(anomaly.dimension, AnomalyDimension::OutputSimilarity);
        assert!(anomaly.severity >= 5.0, "whole window colliding → max severity");
        assert!(anomaly.reasoning.contains("stuck loop"), "{}", anomaly.reasoning);
    }

    #[test]
    fn output_similarity_silent_on_varied_output() {
        // Each prior run emitted a DIFFERENT output; current is different too.
        let mut chain = ProofChain::new("linear", "test");
        let mut prev = GENESIS_HASH.to_string();
        for i in 0..3 {
            let custom = serde_json::json!({
                "step_tool_result": {"log": format!("unique result number {i} with its own lines\nrow-{i}-a\nrow-{i}-b")}
            });
            let proof = historical_step("linear", "p", "1", &prev, custom, 100);
            prev = proof.combined_hash.clone();
            chain.entries.push(ProofEntry::Step(proof));
        }
        let ev = evidence_with_output(serde_json::json!({"log": "brand new distinct output\nfresh-a\nfresh-b"}));
        let res = OutputSimilarityDetector.detect("linear", "p", "1", &ev, 100, &chain);
        assert!(res.is_none(), "varied outputs must not flag a loop");
    }

    #[test]
    fn output_similarity_registered_in_defaults() {
        let dims: Vec<AnomalyDimension> = default_detectors().iter().map(|d| d.dimension()).collect();
        assert!(dims.contains(&AnomalyDimension::OutputSimilarity));
    }
}
