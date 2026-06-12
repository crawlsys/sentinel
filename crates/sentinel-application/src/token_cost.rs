//! Per-model token cost with the cached-vs-uncached split (SEN-16).
//!
//! `roi.rs` answers "Claude $ vs human $"; this answers a different,
//! complementary question proven by hand in the launch report: **what did
//! the AI actually cost at metered API rates, and how much did prompt
//! caching save?**
//!
//! It reads the SEN-7 `tokens-per-ticket.jsonl` aggregate (which carries
//! `total_input` / `cache_read` / `cache_creation` / `output` token counts
//! plus a per-ticket `models` map), prices each token category at the
//! correct per-model published Anthropic rate, and reports:
//!
//! * **with-caching cost** — the real metered cost (cache reads at 0.1×
//!   input, cache writes at 1.25× input).
//! * **without-caching cost** — what the same work would cost if every
//!   cache-read and cache-write token were instead a full input token
//!   (i.e. the context re-sent uncached each turn).
//! * **cache savings** — the difference, in dollars and percent.
//!
//! ## Pricing
//!
//! Rates are per million tokens (MTok), from `platform.claude.com`
//! (verified 2026-06). The Opus 4.x family (4.5/4.6/4.7/4.8) shares one
//! price; Sonnet 4.x and Haiku 4.x are cheaper. A model string that
//! doesn't match a known family falls back to the Opus rate (the most
//! expensive — so an unknown model never *under*-reports cost) and is
//! counted in `unknown_model_tokens` for transparency.
//!
//! ## Model attribution
//!
//! SEN-7 records a `models` map per ticket but not a per-token split, so we
//! attribute a ticket's whole token volume to its dominant (most-sessions)
//! model. This is an approximation, documented as such; it is materially
//! correct because Opus dominates and the cheaper models are a rounding
//! error in these workloads. When a ticket has no usable model, it falls to
//! the Opus rate.
//!
//! Output: `~/.claude/sentinel/metrics/token-cost.{json,jsonl}`.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// Per-MTok USD rates for one model family.
#[derive(Debug, Clone, Copy)]
struct Rates {
    input: f64,
    output: f64,
    cache_write: f64,
    cache_read: f64,
}

/// Opus 4.x (4.5–4.8): $5 in / $25 out / $6.25 5m-cache-write / $0.50 read.
const OPUS: Rates = Rates {
    input: 5.0,
    output: 25.0,
    cache_write: 6.25,
    cache_read: 0.50,
};
/// Sonnet 4.x: $3 / $15 / $3.75 / $0.30.
const SONNET: Rates = Rates {
    input: 3.0,
    output: 15.0,
    cache_write: 3.75,
    cache_read: 0.30,
};
/// Haiku 4.x: $1 / $5 / $1.25 / $0.10.
const HAIKU: Rates = Rates {
    input: 1.0,
    output: 5.0,
    cache_write: 1.25,
    cache_read: 0.10,
};

/// Map a model string (e.g. `"opus-4-7"`, `"claude-sonnet-4-6"`) to its
/// rates. Unknown → Opus (never under-reports). Returns `(rates, known)`.
fn rates_for(model: &str) -> (Rates, bool) {
    let m = model.to_lowercase();
    if m.contains("opus") {
        (OPUS, true)
    } else if m.contains("sonnet") {
        (SONNET, true)
    } else if m.contains("haiku") {
        (HAIKU, true)
    } else {
        (OPUS, false)
    }
}

/// One SEN-7 token row.
#[derive(Debug, Clone)]
struct TokenRow {
    total_input: u64,
    cache_read: u64,
    cache_creation: u64,
    output: u64,
    model: String,
}

/// Per-model cost rollup.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ModelCost {
    pub tokens: u64,
    pub cached_usd: f64,
    pub uncached_usd: f64,
}

/// Full token-cost summary written to `token-cost.json`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct TokenCostSummary {
    pub tickets: usize,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    /// Real metered cost with prompt caching.
    pub cost_with_caching_usd: f64,
    /// Cost if every cache token were a full input token (no caching).
    pub cost_without_caching_usd: f64,
    /// `without - with` — what caching saved, in dollars.
    pub cache_savings_usd: f64,
    /// `cache_savings / without` as a fraction (0–1).
    pub cache_savings_fraction: f64,
    /// Tokens attributed to an unknown model (priced at the Opus rate).
    pub unknown_model_tokens: u64,
    pub by_model: BTreeMap<String, ModelCost>,
}

/// Price `tokens_input` (SEN-7 JSONL) and write `output_summary` + its
/// `.jsonl` sibling (per-ticket cost rows). Returns the summary.
pub fn scan_token_cost(tokens_input: &Path, output_summary: &Path) -> Result<TokenCostSummary> {
    let rows = load_rows(tokens_input)
        .with_context(|| format!("load tokens input {}", tokens_input.display()))?;

    let mut summary = TokenCostSummary::default();
    let mut jsonl_rows: Vec<serde_json::Value> = Vec::new();

    for r in &rows {
        let (rates, known) = rates_for(&r.model);
        let family = model_family(&r.model);

        // With caching: each category at its own rate.
        let cached = per_mtok(r.total_input, rates.input)
            + per_mtok(r.output, rates.output)
            + per_mtok(r.cache_creation, rates.cache_write)
            + per_mtok(r.cache_read, rates.cache_read);

        // Without caching: cache-read + cache-write tokens become full input.
        let reinput = r.total_input + r.cache_read + r.cache_creation;
        let uncached = per_mtok(reinput, rates.input) + per_mtok(r.output, rates.output);

        let tok = r.total_input + r.cache_read + r.cache_creation + r.output;
        summary.tickets += 1;
        summary.total_tokens += tok;
        summary.input_tokens += r.total_input;
        summary.output_tokens += r.output;
        summary.cache_write_tokens += r.cache_creation;
        summary.cache_read_tokens += r.cache_read;
        summary.cost_with_caching_usd += cached;
        summary.cost_without_caching_usd += uncached;
        if !known {
            summary.unknown_model_tokens += tok;
        }

        let mc = summary.by_model.entry(family.clone()).or_default();
        mc.tokens += tok;
        mc.cached_usd += cached;
        mc.uncached_usd += uncached;

        jsonl_rows.push(serde_json::json!({
            "model": family,
            "tokens": tok,
            "cached_usd": cached,
            "uncached_usd": uncached,
        }));
    }

    summary.cache_savings_usd = summary.cost_without_caching_usd - summary.cost_with_caching_usd;
    if summary.cost_without_caching_usd > 0.0 {
        summary.cache_savings_fraction =
            summary.cache_savings_usd / summary.cost_without_caching_usd;
    }

    write_outputs(&jsonl_rows, &summary, output_summary)?;
    Ok(summary)
}

/// Dollars for `tokens` priced at `rate_per_mtok`.
fn per_mtok(tokens: u64, rate_per_mtok: f64) -> f64 {
    // Token counts are well below 2^53; cast precision is immaterial for $.
    #[allow(clippy::cast_precision_loss)]
    let t = tokens as f64;
    t / 1_000_000.0 * rate_per_mtok
}

/// Normalize a model string to a family label for the `by_model` rollup.
fn model_family(model: &str) -> String {
    let m = model.to_lowercase();
    if m.contains("opus") {
        "opus".into()
    } else if m.contains("sonnet") {
        "sonnet".into()
    } else if m.contains("haiku") {
        "haiku".into()
    } else {
        "unknown".into()
    }
}

/// Load and parse the SEN-7 JSONL. Skips blank/malformed lines. Picks each
/// ticket's dominant model from its `models` map (most sessions wins).
fn load_rows(path: &Path) -> Result<Vec<TokenRow>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        out.push(TokenRow {
            total_input: v.get("total_input").and_then(serde_json::Value::as_u64).unwrap_or(0),
            cache_read: v.get("cache_read").and_then(serde_json::Value::as_u64).unwrap_or(0),
            cache_creation: v
                .get("cache_creation")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            output: v.get("output").and_then(serde_json::Value::as_u64).unwrap_or(0),
            model: dominant_model(v.get("models")),
        });
    }
    Ok(out)
}

/// Pick the model with the most sessions from a `{ "model": count }` map.
fn dominant_model(models: Option<&serde_json::Value>) -> String {
    let Some(obj) = models.and_then(serde_json::Value::as_object) else {
        return String::new();
    };
    obj.iter()
        .filter(|(name, _)| !name.starts_with('<')) // skip "<synthetic>"
        .max_by_key(|(_, count)| count.as_u64().unwrap_or(0))
        .map(|(name, _)| name.clone())
        .unwrap_or_default()
}

fn write_outputs(
    rows: &[serde_json::Value],
    summary: &TokenCostSummary,
    output_summary: &Path,
) -> Result<()> {
    if let Some(parent) = output_summary.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create metrics dir {}", parent.display()))?;
    }
    let jsonl = output_summary.with_extension("jsonl");
    let mut f = File::create(&jsonl).with_context(|| format!("create {}", jsonl.display()))?;
    for row in rows {
        f.write_all(serde_json::to_string(row)?.as_bytes())?;
        f.write_all(b"\n")?;
    }
    fs::write(output_summary, serde_json::to_string_pretty(summary)?)
        .with_context(|| format!("write {}", output_summary.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn input(jsonl: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(jsonl.as_bytes()).unwrap();
        f
    }

    #[test]
    fn caching_is_cheaper_than_no_caching() {
        // 1M cache-read tokens: cached = $0.50, uncached (as input) = $5.00.
        let i = input(
            r#"{"total_input":0,"cache_read":1000000,"cache_creation":0,"output":0,"models":{"opus-4-8":1}}"#,
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_token_cost(i.path(), out.path()).unwrap();
        assert!((s.cost_with_caching_usd - 0.50).abs() < 1e-6);
        assert!((s.cost_without_caching_usd - 5.00).abs() < 1e-6);
        assert!((s.cache_savings_usd - 4.50).abs() < 1e-6);
        assert!((s.cache_savings_fraction - 0.9).abs() < 1e-6);
    }

    #[test]
    fn per_model_rates_differ() {
        // Same 1M output tokens, opus ($25) vs haiku ($5).
        let i = input(
            "{\"total_input\":0,\"cache_read\":0,\"cache_creation\":0,\"output\":1000000,\"models\":{\"opus-4-7\":1}}\n\
             {\"total_input\":0,\"cache_read\":0,\"cache_creation\":0,\"output\":1000000,\"models\":{\"claude-haiku-4-5\":1}}",
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_token_cost(i.path(), out.path()).unwrap();
        assert!((s.by_model["opus"].cached_usd - 25.0).abs() < 1e-6);
        assert!((s.by_model["haiku"].cached_usd - 5.0).abs() < 1e-6);
        assert!((s.cost_with_caching_usd - 30.0).abs() < 1e-6);
    }

    #[test]
    fn unknown_model_falls_to_opus_and_is_counted() {
        let i = input(
            r#"{"total_input":1000000,"cache_read":0,"cache_creation":0,"output":0,"models":{"mystery-model":1}}"#,
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_token_cost(i.path(), out.path()).unwrap();
        // 1M input at opus $5.
        assert!((s.cost_with_caching_usd - 5.0).abs() < 1e-6);
        assert_eq!(s.unknown_model_tokens, 1_000_000);
        assert!(s.by_model.contains_key("unknown"));
    }

    #[test]
    fn dominant_model_wins() {
        let i = input(
            r#"{"total_input":0,"cache_read":0,"cache_creation":0,"output":1000000,"models":{"opus-4-8":5,"claude-haiku-4-5":1,"<synthetic>":9}}"#,
        );
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_token_cost(i.path(), out.path()).unwrap();
        // opus has 5 sessions vs haiku 1; <synthetic> ignored → opus rate $25.
        assert!((s.cost_with_caching_usd - 25.0).abs() < 1e-6);
    }

    #[test]
    fn missing_input_is_empty_not_error() {
        let out = tempfile::NamedTempFile::new().unwrap();
        let s = scan_token_cost(Path::new("/nope/tokens.jsonl"), out.path()).unwrap();
        assert_eq!(s.tickets, 0);
        assert_eq!(s.total_tokens, 0);
    }
}
