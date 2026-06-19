//! Auto-severity — LLM-judged Linear ticket priority (the "every ticket
//! should carry the right urgency" engine).
//!
//! Reads the Linear issue cache, asks TWO models (Opus 4.8 + GPT-5.5) to
//! judge each ticket's severity/priority from its title + description, and
//! reconciles their two verdicts into one proposed priority. It is the
//! offline/report half of the feature; the live half (a human confirming a
//! suggestion before it is posted) happens in the CLI / in-session MCP layer.
//!
//! ## Input
//!
//! A Linear issue cache JSON at `~/.claude/sentinel/linear-assigned.json`
//! (the same file [`crate::linear_pm_audit`] reads). The shape is permissive —
//! either a top-level array of issue objects or `{ "issues": [...] }`. Each
//! issue may carry:
//!
//! * `identifier` (e.g. `"FPCRM-606"`) — required to be classified
//! * `id` - the Linear issue UUID. The graph-backed apply path requires it.
//! * `title` — fed to the model
//! * `description` — fed to the model
//! * `priority` — current Linear priority (0=none, 1=urgent … 4=low)
//!
//! ## Linear priority scale
//!
//! Linear uses `0=none, 1=urgent, 2=high, 3=medium, 4=low`. A *lower* non-zero
//! number is *more* urgent. The models are asked for a `1..=4` severity (we
//! never propose 0 — "no priority" is the gap we fill, not a verdict).
//!
//! ## Report-only scan
//!
//! The application-layer scan is read-only: it classifies and reports proposed
//! priorities and mutates NOTHING. Graph-backed Linear mutation is owned by
//! `sentinel-infrastructure`, which consumes these proposal rows and checkpoints
//! the apply/skip decision before issuing any `issueUpdate`.
//!
//! Output is written to `~/.claude/sentinel/metrics/severity.json` (summary)
//! and `…severity.jsonl` (one row per proposal), idempotently.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use sentinel_domain::ports::{LlmModel, LlmPort, LlmRequest};

/// Max tokens for each model's severity verdict — a short JSON object.
const SEVERITY_MAX_TOKENS: u32 = 256;

/// One ticket's reconciled severity proposal, written as a JSONL row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeverityProposal {
    /// Linear issue UUID, when present in the cache. Required by the
    /// infrastructure graph-backed apply path.
    pub issue_id: Option<String>,
    pub identifier: String,
    pub title: String,
    /// Current Linear priority (`None` or `Some(0)` == no priority set).
    pub current_priority: Option<i64>,
    /// The reconciled proposed priority (1..=4).
    pub proposed_priority: i64,
    /// Human-readable rationale (taken from the more-urgent model's reasoning).
    pub reasoning: String,
    /// One of: `set` (gap-fill, no current priority), `suggest` (priority
    /// already set — suggest a change for human review), `agree` (priority
    /// already set and the proposal matches it — nothing to do).
    pub action: String,
    /// Opus 4.8's standalone verdict.
    pub opus_priority: i64,
    /// GPT-5.5's standalone verdict.
    pub gpt_priority: i64,
    /// `true` when both models returned the same priority.
    pub models_agreed: bool,
}

/// The full scan summary written to `severity.json`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SeveritySummary {
    pub tickets_scanned: usize,
    /// Tickets with no current priority that WOULD be set (or were, if applied).
    pub would_set: usize,
    /// Tickets with a current priority where the proposal differs (suggestions).
    pub would_suggest: usize,
    /// Tickets where the two models disagreed (opus != gpt).
    pub disagreements: usize,
    /// Mutations actually performed by a downstream graph-backed apply path.
    pub applied: usize,
    /// `true` when no Linear mutation was performed (read-only).
    pub report_only: bool,
}

/// Reconcile two model priority verdicts into one final priority.
///
/// Both inputs are `1..=4` (Linear: lower == more urgent). When the models
/// agree, that value is returned. When they disagree we pick the **more
/// urgent** of the two (the numerically smaller) — biasing toward higher
/// urgency is the conservative choice for a triage signal: under-prioritizing
/// a real fire costs more than over-prioritizing a minor ticket, which a human
/// can always down-rank.
#[must_use]
pub fn reconcile(opus: i64, gpt: i64) -> i64 {
    opus.min(gpt)
}

/// Extract a `1..=4` priority from an LLM response.
///
/// The prompt asks for `{"priority":N,"reasoning":"..."}`, so we try to parse
/// that JSON first (tolerant of leading/trailing prose by scanning for the
/// first `{`…`}` span). If JSON parsing fails, we also accept the first
/// standalone `1`-`4` digit in the text. Returns `None` when nothing in range
/// is found.
#[must_use]
pub fn parse_priority(text: &str) -> Option<i64> {
    // 1. Try strict JSON, then a brace-delimited substring (models often wrap
    //    the object in markdown fences or a sentence).
    if let Some(p) = parse_priority_json(text) {
        return Some(p);
    }
    // 2. Secondary parse: the first bare 1-4 that is not part of a larger
    //    number.
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if (b'1'..=b'4').contains(&b) {
            let prev_digit = i > 0 && bytes[i - 1].is_ascii_digit();
            let next_digit = bytes.get(i + 1).is_some_and(u8::is_ascii_digit);
            if !prev_digit && !next_digit {
                return Some(i64::from(b - b'0'));
            }
        }
    }
    None
}

/// Reasoning text extracted from an LLM response, or `None`. Best-effort: reads
/// the `reasoning` field of the JSON object if present.
fn parse_reasoning(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    v.get("reasoning")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Parse a `priority` from the first JSON object embedded in `text`.
fn parse_priority_json(text: &str) -> Option<i64> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let p = v.get("priority")?.as_i64()?;
    (1..=4).contains(&p).then_some(p)
}

/// A normalized issue parsed out of the cache.
#[derive(Debug, Clone)]
struct Issue {
    id: Option<String>,
    identifier: String,
    title: String,
    description: String,
    priority: Option<i64>,
}

/// Run the auto-severity scan over `linear_cache`, write `output` (JSON) and
/// its `.jsonl` sibling (proposal rows), and return the summary.
///
/// For each ticket it asks `llm` twice (Opus then Codex/GPT-5.5), parses both
/// verdicts, reconciles them, and classifies the action. This function performs
/// NO Linear mutation. The graph-backed apply path consumes the generated
/// proposal rows downstream and must checkpoint its authorization decision
/// before any Linear write.
///
/// # Errors
/// Returns an error on cache-read / output-write failures, LLM failures, or any
/// ticket whose model verdicts cannot both be parsed. The downstream
/// graph-backed apply path must consume a complete proposal set, not a partial
/// report with silently skipped tickets.
pub async fn scan_severity(
    linear_cache: &Path,
    output: &Path,
    llm: &dyn LlmPort,
) -> Result<SeveritySummary> {
    let issues = load_issues(linear_cache)
        .with_context(|| format!("load linear cache {}", linear_cache.display()))?;

    let mut proposals: Vec<SeverityProposal> = Vec::new();
    let mut summary = SeveritySummary {
        report_only: true,
        ..Default::default()
    };

    for iss in &issues {
        summary.tickets_scanned += 1;

        let prompt = build_prompt(&iss.identifier, &iss.title, &iss.description);
        let opus_resp = complete(llm, LlmModel::Opus, &prompt)
            .await
            .with_context(|| format!("auto-severity Opus verdict failed for {}", iss.identifier))?;
        let gpt_resp = complete(llm, LlmModel::Codex, &prompt)
            .await
            .with_context(|| {
                format!(
                    "auto-severity GPT/Codex verdict failed for {}",
                    iss.identifier
                )
            })?;
        let opus_pri = parse_priority(&opus_resp).ok_or_else(|| {
            anyhow::anyhow!(
                "auto-severity Opus verdict for {} did not contain a parseable 1..=4 priority",
                iss.identifier
            )
        })?;
        let gpt_pri = parse_priority(&gpt_resp).ok_or_else(|| {
            anyhow::anyhow!(
                "auto-severity GPT/Codex verdict for {} did not contain a parseable 1..=4 priority",
                iss.identifier
            )
        })?;

        let proposed = reconcile(opus_pri, gpt_pri);
        let models_agreed = opus_pri == gpt_pri;
        if !models_agreed {
            summary.disagreements += 1;
        }

        // Reasoning from whichever model produced the more-urgent verdict (the
        // one that "won" the reconcile); otherwise use the other model's
        // reasoning. If neither model supplies reasoning, fail closed instead
        // of inventing evidence for a proposal.
        let reasoning = pick_reasoning(opus_pri, gpt_pri, &opus_resp, &gpt_resp)
            .with_context(|| format!("auto-severity reasoning missing for {}", iss.identifier))?;

        let has_priority = iss.priority.is_some_and(|p| p > 0);
        let action = if !has_priority {
            summary.would_set += 1;
            "set"
        } else if iss.priority == Some(proposed) {
            "agree"
        } else {
            summary.would_suggest += 1;
            "suggest"
        };

        proposals.push(SeverityProposal {
            issue_id: iss.id.clone(),
            identifier: iss.identifier.clone(),
            title: iss.title.clone(),
            current_priority: iss.priority,
            proposed_priority: proposed,
            reasoning,
            action: action.to_string(),
            opus_priority: opus_pri,
            gpt_priority: gpt_pri,
            models_agreed,
        });
    }

    write_outputs(&proposals, &summary, output)?;
    Ok(summary)
}

/// The model prompt for one ticket. Asks for a strict JSON verdict.
fn build_prompt(identifier: &str, title: &str, description: &str) -> String {
    format!(
        "You are a software-triage severity judge for a Linear ticket. Read the \
         ticket and assign a priority on Linear's scale:\n\
         1 = Urgent (production down, data loss, security, or a hard imminent deadline)\n\
         2 = High (a broken core feature or a blocker for many users)\n\
         3 = Medium (a normal bug or feature; the default for most work)\n\
         4 = Low (cosmetic, nice-to-have, or easily deferred)\n\n\
         Lower numbers are MORE urgent. Pick exactly one of 1, 2, 3, or 4.\n\n\
         Ticket {identifier}\n\
         Title: {title}\n\
         Description:\n{description}\n\n\
         Respond with ONLY a JSON object, no prose:\n\
         {{\"priority\": <1-4>, \"reasoning\": \"<one short sentence>\"}}"
    )
}

/// Run one required model completion.
async fn complete(llm: &dyn LlmPort, model: LlmModel, prompt: &str) -> Result<String> {
    let req = LlmRequest {
        model,
        prompt: prompt.to_string(),
        max_tokens: SEVERITY_MAX_TOKENS,
    };
    llm.complete(req)
        .await
        .with_context(|| format!("auto-severity {model:?} completion failed"))
}

/// Pick the reasoning string from whichever model produced the winning (more
/// urgent) verdict; otherwise use the other model's reasoning.
fn pick_reasoning(opus_pri: i64, gpt_pri: i64, opus_resp: &str, gpt_resp: &str) -> Result<String> {
    let opus_reason = parse_reasoning(opus_resp);
    let gpt_reason = parse_reasoning(gpt_resp);
    // The "winner" is the more-urgent (smaller) number; ties prefer Opus.
    let (primary, secondary) = if opus_pri <= gpt_pri {
        (opus_reason, gpt_reason)
    } else {
        (gpt_reason, opus_reason)
    };
    primary
        .or(secondary)
        .ok_or_else(|| anyhow::anyhow!("no reasoning supplied by either model"))
}

/// Parse the permissive cache into normalized issues.
fn load_issues(path: &Path) -> Result<Vec<Issue>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse JSON {}", path.display()))?;
    let arr: &[serde_json::Value] = if let Some(a) = value.as_array() {
        a
    } else if let Some(a) = value.get("issues").and_then(serde_json::Value::as_array) {
        a
    } else {
        return Ok(Vec::new());
    };

    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let Some(identifier) = v
            .get("identifier")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        out.push(Issue {
            id: v
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            identifier,
            title: v
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            // Accept either `description` or, defensively, `body`.
            description: v
                .get("description")
                .or_else(|| v.get("body"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            priority: v.get("priority").and_then(serde_json::Value::as_i64),
        });
    }
    Ok(out)
}

fn write_outputs(
    proposals: &[SeverityProposal],
    summary: &SeveritySummary,
    output: &Path,
) -> Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create metrics dir {}", parent.display()))?;
    }
    let jsonl = output.with_extension("jsonl");
    let mut f = File::create(&jsonl).with_context(|| format!("create {}", jsonl.display()))?;
    for p in proposals {
        f.write_all(serde_json::to_string(p)?.as_bytes())?;
        f.write_all(b"\n")?;
    }
    fs::write(output, serde_json::to_string_pretty(summary)?)
        .with_context(|| format!("write {}", output.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::port_errors::LlmError;
    use std::sync::Mutex;

    /// A mock `LlmPort` that returns canned responses in order, per model.
    /// Opus and Codex draw from separate queues so a test can drive a
    /// disagreement (different verdicts for the same ticket).
    struct MockLlm {
        opus: Mutex<Vec<String>>,
        codex: Mutex<Vec<String>>,
    }

    impl MockLlm {
        fn new(opus: &[&str], codex: &[&str]) -> Self {
            Self {
                opus: Mutex::new(opus.iter().rev().map(|s| (*s).to_string()).collect()),
                codex: Mutex::new(codex.iter().rev().map(|s| (*s).to_string()).collect()),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmPort for MockLlm {
        async fn complete(
            &self,
            request: LlmRequest,
        ) -> Result<String, sentinel_domain::port_errors::LlmError> {
            let q = match request.model {
                LlmModel::Opus => &self.opus,
                _ => &self.codex,
            };
            q.lock().unwrap().pop().ok_or_else(|| {
                LlmError::Unavailable(format!("missing canned response for {:?}", request.model))
            })
        }
    }

    fn cache(json: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn reconcile_picks_more_urgent_on_disagreement() {
        // Lower == more urgent: a 1-vs-3 split resolves to 1.
        assert_eq!(reconcile(1, 3), 1);
        assert_eq!(reconcile(4, 2), 2);
        // Agreement returns the shared value.
        assert_eq!(reconcile(3, 3), 3);
    }

    #[test]
    fn parse_priority_handles_json_bare_number_and_garbage() {
        // Strict JSON.
        assert_eq!(
            parse_priority(r#"{"priority":2,"reasoning":"broken core feature"}"#),
            Some(2)
        );
        // JSON wrapped in markdown fences / prose.
        assert_eq!(
            parse_priority("Here you go:\n```json\n{\"priority\": 1}\n```"),
            Some(1)
        );
        // Bare number secondary parse.
        assert_eq!(
            parse_priority("I'd rate this a 4 — cosmetic only."),
            Some(4)
        );
        // Out-of-range JSON priority -> no JSON match; the digit scan finds
        // none in 1..=4 (7 is rejected) -> None.
        assert_eq!(parse_priority(r#"{"priority":7}"#), None);
        // Pure garbage.
        assert_eq!(parse_priority("no idea, sorry"), None);
        // A larger number must not yield a stray in-range digit.
        assert_eq!(parse_priority("issue 1234 is hard"), None);
    }

    #[tokio::test]
    async fn report_only_scan_classifies_set_vs_suggest_and_applies_nothing() {
        // T-1: no priority -> "set". T-2: priority 4, models say 2 -> "suggest".
        // T-3: priority 3, models say 3 -> "agree".
        let c = cache(
            r#"[
                {"id":"u1","identifier":"S-1","title":"prod down","description":"500s everywhere","priority":0},
                {"id":"u2","identifier":"S-2","title":"typo","description":"label wrong","priority":4},
                {"id":"u3","identifier":"S-3","title":"normal bug","description":"edge case","priority":3}
            ]"#,
        );
        // Both models agree per ticket: S-1 -> 1, S-2 -> 2, S-3 -> 3.
        let llm = MockLlm::new(
            &[
                r#"{"priority":1,"reasoning":"production outage"}"#,
                r#"{"priority":2,"reasoning":"broken feature"}"#,
                r#"{"priority":3,"reasoning":"ordinary bug"}"#,
            ],
            &[
                r#"{"priority":1,"reasoning":"prod down"}"#,
                r#"{"priority":2,"reasoning":"core broken"}"#,
                r#"{"priority":3,"reasoning":"normal"}"#,
            ],
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_severity(c.path(), out.path(), &llm).await.unwrap();

        assert_eq!(s.tickets_scanned, 3);
        assert_eq!(s.would_set, 1); // S-1
        assert_eq!(s.would_suggest, 1); // S-2 (4 → 2)
        assert_eq!(s.disagreements, 0);
        assert_eq!(s.applied, 0); // report-only scan mutated nothing
        assert!(s.report_only);

        let jsonl = std::fs::read_to_string(out.path().with_extension("jsonl")).unwrap();
        let rows = jsonl
            .lines()
            .map(serde_json::from_str::<SeverityProposal>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(rows[0].identifier, "S-1");
        assert_eq!(rows[0].issue_id.as_deref(), Some("u1"));
    }

    #[tokio::test]
    async fn disagreement_is_counted_and_reconciled_to_more_urgent() {
        let c =
            cache(r#"[{"id":"u9","identifier":"D-1","title":"x","description":"y","priority":0}]"#);
        // Opus says 3, GPT says 1 -> reconcile to 1, disagreement recorded.
        let llm = MockLlm::new(
            &[r#"{"priority":3,"reasoning":"looks routine"}"#],
            &[r#"{"priority":1,"reasoning":"actually a security hole"}"#],
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_severity(c.path(), out.path(), &llm).await.unwrap();
        assert_eq!(s.disagreements, 1);
        assert_eq!(s.would_set, 1);
        // Read the JSONL row back to confirm the reconciled value + reasoning.
        let jsonl = std::fs::read_to_string(out.path().with_extension("jsonl")).unwrap();
        assert!(jsonl.contains("\"proposed_priority\":1"));
        assert!(jsonl.contains("security hole")); // winner's reasoning
    }

    #[tokio::test]
    async fn unparseable_verdicts_fail_closed_before_writing_proposals() {
        let c =
            cache(r#"[{"id":"u0","identifier":"G-1","title":"x","description":"y","priority":0}]"#);
        let llm = MockLlm::new(&["no idea"], &["dunno"]);
        let out = tempfile::NamedTempFile::new().unwrap();
        let err = scan_severity(c.path(), out.path(), &llm)
            .await
            .expect_err("unparseable model verdicts must fail closed");
        assert!(
            err.to_string().contains("G-1")
                && err
                    .to_string()
                    .contains("did not contain a parseable 1..=4 priority"),
            "unexpected error: {err:#}"
        );
        assert!(
            !out.path().with_extension("jsonl").exists(),
            "failed scan must not write partial proposal rows"
        );
    }

    #[tokio::test]
    async fn missing_reasoning_fails_closed_before_writing_proposals() {
        let c =
            cache(r#"[{"id":"u0","identifier":"R-1","title":"x","description":"y","priority":0}]"#);
        let llm = MockLlm::new(&[r#"{"priority":2}"#], &[r#"{"priority":2}"#]);
        let out = tempfile::NamedTempFile::new().unwrap();
        let err = scan_severity(c.path(), out.path(), &llm)
            .await
            .expect_err("reasoning-free model verdicts must fail closed");
        assert!(
            err.to_string().contains("R-1") && err.to_string().contains("reasoning"),
            "unexpected error: {err:#}"
        );
        assert!(
            !out.path().with_extension("jsonl").exists(),
            "failed scan must not write partial proposal rows"
        );
    }

    #[tokio::test]
    async fn missing_cache_is_empty_not_error() {
        let llm = MockLlm::new(&[], &[]);
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_severity(Path::new("/nonexistent/cache.json"), out.path(), &llm)
            .await
            .unwrap();
        assert_eq!(s.tickets_scanned, 0);
    }
}
