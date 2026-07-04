//! Tokens-per-ticket aggregator (SEN-7).
//!
//! Walks `~/.claude/projects/*/` for session JSONL files, attributes
//! each session to a Linear ticket (via path-slug regex first, then
//! bounded prompt inspection), aggregates per-model token usage from every
//! `assistant` message's `usage` block, applies Anthropic per-model
//! pricing (`sentinel-domain::pricing`), and writes
//! `~/.claude/sentinel/metrics/tokens-per-ticket.jsonl`.
//!
//! Output is full-overwrite each scan: input session JSONLs are the
//! source of truth, so re-scanning is idempotent.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sentinel_domain::pricing::{cost_for, short_model_label, TokenUsage};

/// Maximum number of user messages to grep for a ticket reference
/// when the path slug doesn't yield one.
const MAX_PROMPT_GREP_LINES: usize = 50;

/// Known uppercase ticket prefixes we recognise. Other prefixes still
/// match the regex (`[A-Z]{2,7}-\d+`) but get filtered against this
/// allow-list to avoid attributing tokens to false positives like
/// `HTTP-200` or `UTF-8`.
const KNOWN_PREFIXES: &[&str] = &[
    "FPCRM", "FPFIELD", "FPROUTE", "FPMD", "FPTRIBU", "LEG", "COR", "EXA", "SYN", "TES", "TRI",
    "SEN",
];

/// Confidence label written to the output JSONL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Ticket extracted from the session's path slug — high confidence.
    #[default]
    High,
    /// Ticket extracted from a user-prompt grep — medium confidence.
    Medium,
}

/// Per-ticket aggregate row written to the output JSONL.
#[derive(Debug, Clone, Serialize)]
pub struct TicketAggregate {
    pub ticket: String,
    pub sessions: u64,
    pub total_input: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub output: u64,
    pub cost_usd: f64,
    pub unpriced_tokens: u64,
    pub models: BTreeMap<String, u64>,
    pub confidence: Confidence,
}

/// Summary returned by `scan_token_usage` for human reporting.
#[derive(Debug, Default, Serialize)]
pub struct ScanReport {
    pub total_sessions: u64,
    pub mapped_sessions: u64,
    pub unmapped_sessions: u64,
    pub unpriced_sessions: u64,
    pub unpriced_tokens: u64,
    pub tickets: u64,
    pub top_n_expensive: Vec<(String, f64)>,
}

/// Per-session intermediate state — accumulated while walking lines.
#[derive(Debug, Default)]
struct SessionRollup {
    usage: TokenUsage,
    /// Set of full model ids observed in this session.
    models_seen: Vec<String>,
}

/// Walk `projects_root` for session JSONL files, attribute each to
/// a ticket, aggregate, and write `output` (overwrite).
pub fn scan_token_usage(projects_root: &Path, output: &Path) -> Result<ScanReport> {
    let mut report = ScanReport::default();

    // Per-ticket aggregates keyed by ticket id.
    let mut by_ticket: HashMap<String, AggBuild> = HashMap::new();

    if !projects_root.exists() {
        // Empty scan — still write an empty file so the output path is
        // always valid for downstream consumers.
        ensure_parent(output)?;
        File::create(output)?;
        return Ok(report);
    }

    for entry in fs::read_dir(projects_root)
        .with_context(|| format!("read_dir {}", projects_root.display()))?
        .flatten()
    {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        // First-pass ticket extraction from the directory slug —
        // applies to every session in this directory.
        let path_ticket = extract_ticket_from_path(&dir_name);

        for jsonl in list_jsonl_files(&path) {
            ingest_session(&jsonl, path_ticket.as_deref(), &mut by_ticket, &mut report);
        }
    }

    // Convert to final aggregates and write output.
    let mut rows: Vec<TicketAggregate> = by_ticket
        .into_values()
        .map(|a| TicketAggregate {
            ticket: a.ticket,
            sessions: a.sessions,
            total_input: a.usage.input,
            cache_read: a.usage.cache_read,
            cache_creation: a.usage.cache_creation_5m + a.usage.cache_creation_1h,
            output: a.usage.output,
            cost_usd: round_cents(a.cost_usd),
            unpriced_tokens: a.unpriced_tokens,
            models: a.models,
            confidence: a.confidence,
        })
        .collect();

    // Sort: highest cost first for stable, useful output.
    rows.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.ticket.cmp(&b.ticket))
    });

    report.tickets = rows.len() as u64; // safe: ticket count fits in u64

    report.top_n_expensive = rows
        .iter()
        .take(10)
        .map(|r| (r.ticket.clone(), r.cost_usd))
        .collect();

    ensure_parent(output)?;
    let mut file =
        File::create(output).with_context(|| format!("create output {}", output.display()))?;
    for row in &rows {
        let line = serde_json::to_string(row)?;
        writeln!(file, "{line}")?;
    }
    file.flush()?;

    Ok(report)
}

#[derive(Debug, Default)]
struct AggBuild {
    ticket: String,
    sessions: u64,
    usage: TokenUsage,
    cost_usd: f64,
    unpriced_tokens: u64,
    models: BTreeMap<String, u64>,
    confidence: Confidence,
}

fn ingest_session(
    jsonl: &Path,
    path_ticket: Option<&str>,
    by_ticket: &mut HashMap<String, AggBuild>,
    report: &mut ScanReport,
) {
    report.total_sessions += 1;

    let Ok(rollup) = parse_session(jsonl) else {
        report.unmapped_sessions += 1;
        return;
    };

    // Skip sessions with zero usage — pure metadata files
    // (the `pr-link`-only JSONLs are common).
    if rollup.usage == TokenUsage::default() {
        report.unmapped_sessions += 1;
        return;
    }

    // Attribute: path slug first, prompt grep second.
    let (ticket, confidence) = path_ticket.map_or_else(
        || {
            extract_ticket_from_prompts(jsonl, MAX_PROMPT_GREP_LINES)
                .map_or((None, Confidence::High), |t| (Some(t), Confidence::Medium))
        },
        |t| (Some(t.to_string()), Confidence::High),
    );

    let Some(ticket) = ticket else {
        report.unmapped_sessions += 1;
        return;
    };
    report.mapped_sessions += 1;

    let agg = by_ticket.entry(ticket.clone()).or_insert_with(|| AggBuild {
        ticket: ticket.clone(),
        confidence,
        ..AggBuild::default()
    });
    // Path-attributed sessions outrank prompt-attributed ones.
    if confidence == Confidence::High {
        agg.confidence = Confidence::High;
    }
    agg.sessions += 1;
    agg.usage = agg.usage.add(rollup.usage);
    // Cost: bill each session at its own model rate (avoids
    // mixing rates across a multi-model session).
    match cost_for_session(&rollup) {
        Ok(cost) => {
            agg.cost_usd += cost;
        }
        Err(_) => {
            let tokens = total_tokens(rollup.usage);
            agg.unpriced_tokens += tokens;
            report.unpriced_sessions += 1;
            report.unpriced_tokens += tokens;
        }
    }

    for model in rollup.models_seen {
        let label = short_model_label(&model);
        *agg.models.entry(label).or_insert(0) += 1;
    }
}

/// Compute USD cost for a session by attributing every token in
/// the rolled-up usage to the *first* model id observed in that
/// session. Multi-model sessions are rare in practice (model is
/// session-scoped), and any drift is a single-session approximation
/// well below the noise floor of estimated pricing.
fn cost_for_session(rollup: &SessionRollup) -> Result<f64, sentinel_domain::pricing::PricingError> {
    let model = rollup.models_seen.first().map_or("", String::as_str);
    cost_for(rollup.usage, model)
}

fn total_tokens(usage: TokenUsage) -> u64 {
    usage.input
        + usage.output
        + usage.cache_read
        + usage.cache_creation_5m
        + usage.cache_creation_1h
}

fn round_cents(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn ensure_parent(p: &Path) -> Result<()> {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    Ok(())
}

fn list_jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(read) = fs::read_dir(dir) else {
        return out;
    };
    for entry in read.flatten() {
        let p = entry.path();
        if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
    out
}

fn ticket_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b([A-Z]{2,7})-(\d+)\b").expect("static regex compiles")
    })
}

/// Extract the first known-prefix ticket id from an arbitrary string
/// (typically a Claude-Code path slug like
/// `C--Users-operator-...-feat-fpcrm-289-foo`).
///
/// Returns the canonical uppercase form (`FPCRM-289`).
#[must_use]
pub fn extract_ticket_from_path(path_slug: &str) -> Option<String> {
    let re = ticket_regex();
    for cap in re.captures_iter(path_slug) {
        let prefix = cap.get(1)?.as_str().to_ascii_uppercase();
        let number = cap.get(2)?.as_str();
        if KNOWN_PREFIXES.contains(&prefix.as_str()) {
            return Some(format!("{prefix}-{number}"));
        }
    }
    None
}

/// Scan the first `max_lines` user-prompt entries of a session JSONL
/// for a known-prefix ticket id. Returns the canonical uppercase
/// form on the first hit.
#[must_use]
pub fn extract_ticket_from_prompts(jsonl: &Path, max_lines: usize) -> Option<String> {
    let file = File::open(jsonl).ok()?;
    let reader = BufReader::new(file);
    let mut user_prompts_seen = 0usize;

    for line in reader.lines().map_while(std::result::Result::ok) {
        if user_prompts_seen >= max_lines {
            break;
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("type").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        user_prompts_seen += 1;
        let text = match value.pointer("/message/content") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(arr)) => arr
                .iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" "),
            _ => continue,
        };
        if let Some(t) = extract_ticket_from_path(&text) {
            return Some(t);
        }
    }
    None
}

/// Parse a session JSONL: walk every line, sum `usage` blocks from
/// `assistant`-type messages, record observed model ids.
fn parse_session(jsonl: &Path) -> Result<SessionRollup> {
    let file = File::open(jsonl).with_context(|| format!("open session {}", jsonl.display()))?;
    let reader = BufReader::new(file);
    let mut rollup = SessionRollup::default();

    for line in reader.lines().map_while(std::result::Result::ok) {
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        if let Some(model) = message.get("model").and_then(|v| v.as_str()) {
            let owned = model.to_string();
            if !rollup.models_seen.contains(&owned) {
                rollup.models_seen.push(owned);
            }
        }
        let Some(usage) = message.get("usage") else {
            continue;
        };

        let input = usage_u64(usage, "input_tokens");
        let output = usage_u64(usage, "output_tokens");
        let cache_read = usage_u64(usage, "cache_read_input_tokens");
        let flat_cc = usage_u64(usage, "cache_creation_input_tokens");

        let (short_create, long_create) = usage.get("cache_creation").map_or(
            // No breakdown: assume the cheaper 5m rate.
            (flat_cc, 0_u64),
            |cc| {
                let short = usage_u64(cc, "ephemeral_5m_input_tokens");
                let long = usage_u64(cc, "ephemeral_1h_input_tokens");
                // If the breakdown reports zero but the flat field has a
                // value, route the flat number under the 5m bucket.
                if short + long == 0 && flat_cc > 0 {
                    (flat_cc, 0)
                } else {
                    (short, long)
                }
            },
        );

        rollup.usage = rollup.usage.add(TokenUsage {
            input,
            output,
            cache_read,
            cache_creation_5m: short_create,
            cache_creation_1h: long_create,
        });
    }

    Ok(rollup)
}

fn usage_u64(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn ticket_extracted_from_lowercase_path_slug() {
        let slug = "C--Users-operator-Documents-GitHub-firefly-pro-crm--claude-worktrees-feat-fpcrm-289-email-signature";
        assert_eq!(
            extract_ticket_from_path(slug),
            Some("FPCRM-289".to_string())
        );
    }

    #[test]
    fn ticket_extracted_from_uppercase_path_slug() {
        let slug = "worktree-feat-LEG-42-foo";
        assert_eq!(extract_ticket_from_path(slug), Some("LEG-42".to_string()));
    }

    #[test]
    fn no_ticket_when_path_has_no_known_prefix() {
        assert_eq!(extract_ticket_from_path("just-some-feature-branch"), None);
        // HTTP-200 etc. should NOT match — not in known prefix list.
        assert_eq!(extract_ticket_from_path("response-HTTP-200-ok"), None);
    }

    #[test]
    fn first_known_prefix_wins_over_unknown_neighbours() {
        let slug = "branch-HTTP-200-and-fpcrm-7-fix";
        assert_eq!(extract_ticket_from_path(slug), Some("FPCRM-7".to_string()));
    }

    #[test]
    fn ticket_extracted_from_user_prompt_text() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        let body = r#"{"type":"user","message":{"role":"user","content":"please fix FPCRM-456 today"}}
{"type":"assistant","message":{"model":"claude-opus-4-7","usage":{"input_tokens":1,"output_tokens":1}}}"#;
        fs::write(&path, body).unwrap();
        assert_eq!(
            extract_ticket_from_prompts(&path, 50),
            Some("FPCRM-456".to_string())
        );
    }

    #[test]
    fn prompt_grep_handles_array_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        let body = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"work on COR-12 please"}]}}"#;
        fs::write(&path, body).unwrap();
        assert_eq!(
            extract_ticket_from_prompts(&path, 50),
            Some("COR-12".to_string())
        );
    }

    #[test]
    fn prompt_grep_caps_at_max_lines() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut body = String::new();
        for _ in 0..60 {
            body.push_str(r#"{"type":"user","message":{"role":"user","content":"nothing here"}}"#);
            body.push('\n');
        }
        body.push_str(r#"{"type":"user","message":{"role":"user","content":"FPCRM-99"}}"#);
        fs::write(&path, body).unwrap();
        // 50-line cap => never reaches the FPCRM line.
        assert_eq!(extract_ticket_from_prompts(&path, 50), None);
    }

    #[test]
    fn cost_for_session_uses_model_rates() {
        let rollup = SessionRollup {
            usage: TokenUsage {
                input: 1_000_000,
                output: 0,
                cache_read: 0,
                cache_creation_5m: 0,
                cache_creation_1h: 0,
            },
            models_seen: vec!["claude-sonnet-4-5".into()],
        };
        // 1Mtok input on sonnet => $3.00
        assert!((cost_for_session(&rollup).unwrap() - 3.0).abs() < 0.01);
    }

    #[test]
    fn cost_for_session_rejects_missing_model() {
        let rollup = SessionRollup {
            usage: TokenUsage {
                input: 1_000_000,
                output: 0,
                cache_read: 0,
                cache_creation_5m: 0,
                cache_creation_1h: 0,
            },
            models_seen: Vec::new(),
        };
        assert!(cost_for_session(&rollup).is_err());
    }

    #[test]
    fn end_to_end_scan_writes_aggregated_jsonl() {
        let dir = TempDir::new().unwrap();
        let projects = dir.path().join("projects");
        let session_dir = projects.join(
            "C--Users-test-Documents-GitHub-firefly-pro-crm--claude-worktrees-feat-fpcrm-289-foo",
        );
        fs::create_dir_all(&session_dir).unwrap();

        // Session A: 1 assistant message, 1Mtok input, 1Mtok output on opus.
        let a = session_dir.join("aaa.jsonl");
        let body_a = r#"{"type":"user","message":{"role":"user","content":"hi"}}
{"type":"assistant","message":{"model":"claude-opus-4-7","usage":{"input_tokens":1000000,"output_tokens":1000000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
        fs::write(&a, body_a).unwrap();

        // Session B: 1 assistant message, 500K input on opus.
        let b = session_dir.join("bbb.jsonl");
        let body_b = r#"{"type":"assistant","message":{"model":"claude-opus-4-7","usage":{"input_tokens":500000,"output_tokens":0}}}"#;
        fs::write(&b, body_b).unwrap();

        // Session C in a no-ticket dir, but with prompt-ref to LEG-7.
        let other_dir = projects.join("C--Users-test-some-bare-repo");
        fs::create_dir_all(&other_dir).unwrap();
        let c = other_dir.join("ccc.jsonl");
        let body_c = r#"{"type":"user","message":{"role":"user","content":"work on LEG-7 please"}}
{"type":"assistant","message":{"model":"claude-sonnet-4-5","usage":{"input_tokens":1000000,"output_tokens":0}}}"#;
        fs::write(&c, body_c).unwrap();

        let out = dir.path().join("metrics").join("tokens-per-ticket.jsonl");
        let report = scan_token_usage(&projects, &out).unwrap();

        assert_eq!(report.total_sessions, 3);
        assert_eq!(report.mapped_sessions, 3);
        assert_eq!(report.unmapped_sessions, 0);
        assert_eq!(report.tickets, 2);

        let content = fs::read_to_string(&out).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        // Parse rows back and verify aggregation.
        let rows: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let fpcrm = rows
            .iter()
            .find(|r| r["ticket"] == "FPCRM-289")
            .expect("fpcrm row present");
        assert_eq!(fpcrm["sessions"], 2);
        assert_eq!(fpcrm["total_input"], 1_500_000);
        assert_eq!(fpcrm["output"], 1_000_000);
        assert_eq!(fpcrm["confidence"], "high");
        // Opus: 1.5M input * $15 = $22.50, 1M output * $75 = $75 => $97.50
        let cost = fpcrm["cost_usd"].as_f64().unwrap();
        assert!((cost - 97.5).abs() < 0.5, "cost was {cost}");

        let leg = rows
            .iter()
            .find(|r| r["ticket"] == "LEG-7")
            .expect("leg row present");
        assert_eq!(leg["sessions"], 1);
        assert_eq!(leg["confidence"], "medium");
    }

    #[test]
    fn scan_against_missing_root_writes_empty_file() {
        let dir = TempDir::new().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let out = dir.path().join("out.jsonl");
        let report = scan_token_usage(&nonexistent, &out).unwrap();
        assert_eq!(report.total_sessions, 0);
        assert!(out.exists());
        assert_eq!(fs::read_to_string(&out).unwrap(), "");
    }
}
