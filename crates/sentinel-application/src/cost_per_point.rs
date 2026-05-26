//! Tokens-per-story-point + cost-per-story-point analyzer (SEN-13).
//!
//! Joins SEN-7's `~/.claude/sentinel/metrics/tokens-per-ticket.jsonl`
//! (token + cost aggregates per Linear ticket) with a Linear issue
//! cache (which may carry a `estimate` field per ticket) to compute:
//!
//! * `tokens_per_point = total_tokens / estimate`
//! * `cost_per_point   = cost_usd     / estimate`
//!
//! Per-ticket rows are bucketed by estimate (nearest of 1/2/3/5/8/16)
//! and we compute p25/p50/p75/p90 across `cost_per_point` and
//! `tokens_per_point` per bucket, plus a "high-vs-low" drift ratio
//! (bucket-8 median / bucket-2 median) that surfaces non-linear
//! estimating curves — if 8-pt tickets cost > 5x what 2-pt tickets
//! cost per point, sizing has drifted.
//!
//! The Linear cache shape is permissive: we accept either a top-level
//! JSON array of issue objects, or `{ "issues": [...] }`. The
//! `estimate` field is optional. If 0 tickets in the cache carry an
//! estimate, the analyzer reports that honestly — we never fabricate
//! point values. (Acceptance criterion "All N estimated tickets
//! analyzed" simply becomes "0 of N (no estimate data)" when the
//! cache lacks the field, which is the correct answer given current
//! state.)

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// Valid Linear estimate buckets. We round any non-matching estimate
/// to the nearest of these (ties round up, so 6 → 8 not 5).
pub const BUCKETS: &[u8] = &[1, 2, 3, 5, 8, 16];

/// Drift alarm threshold: if the bucket-8 median `cost_per_point` is
/// more than this multiple of the bucket-2 median, the estimate
/// curve is non-linear and the team is mis-sizing one or both ends.
pub const DRIFT_ALARM_THRESHOLD: f64 = 5.0;

/// Per-ticket row written to `cost-per-point.jsonl`.
#[derive(Debug, Clone, Serialize)]
pub struct CostPerPointRow {
    pub ticket: String,
    pub estimate: f64,
    pub bucket: u8,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub tokens_per_point: f64,
    pub cost_per_point: f64,
}

/// Aggregated per-bucket statistics.
#[derive(Debug, Clone, Serialize, Default)]
pub struct BucketStats {
    pub n: usize,
    pub cost_p25: f64,
    pub cost_p50: f64,
    pub cost_p75: f64,
    pub cost_p90: f64,
    pub tokens_p25: f64,
    pub tokens_p50: f64,
    pub tokens_p75: f64,
    pub tokens_p90: f64,
}

/// Summary written to `cost-per-point-summary.json`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CostPerPointSummary {
    pub tickets_total: usize,
    pub tickets_with_estimate: usize,
    pub buckets: BTreeMap<u8, BucketStats>,
    /// `Some(ratio)` when both bucket-8 and bucket-2 carry data.
    pub drift_ratio_high_vs_low: Option<f64>,
    /// Convenience: `drift_ratio_high_vs_low > DRIFT_ALARM_THRESHOLD`.
    pub drift_alarm: bool,
}

/// In-memory report returned by `scan_cost_per_point` for human reporting.
#[derive(Debug, Clone, Default)]
pub struct CostPerPointReport {
    pub tickets_analyzed: usize,
    pub tickets_with_estimate: usize,
    pub buckets: BTreeMap<u8, BucketStats>,
    pub drift_ratio_high_vs_low: Option<f64>,
    pub drift_alarm: bool,
}

/// Walk `tokens_input` (JSONL from SEN-7), join against a Linear
/// issue cache at `linear_cache`, and write `output_jsonl` +
/// `output_summary`. See module doc for full algorithm.
pub fn scan_cost_per_point(
    tokens_input: &Path,
    linear_cache: &Path,
    output_jsonl: &Path,
    output_summary: &Path,
) -> Result<CostPerPointReport> {
    // Phase 1: load the SEN-7 tokens-per-ticket rows.
    let token_rows = load_token_rows(tokens_input)
        .with_context(|| format!("load tokens input {}", tokens_input.display()))?;

    // Phase 2: load Linear issue cache → ticket → estimate map.
    let estimate_by_ticket = load_estimates(linear_cache)
        .with_context(|| format!("load linear cache {}", linear_cache.display()))?;

    // Phase 3: join, filter to tickets with both token data + estimate.
    let mut rows: Vec<CostPerPointRow> = Vec::new();
    for tr in &token_rows {
        let Some(estimate) = estimate_by_ticket.get(&tr.ticket).copied() else {
            continue;
        };
        // Defensive: ignore zero/negative estimates (Linear allows 0).
        if !estimate.is_finite() || estimate <= 0.0 {
            continue;
        }
        let total_tokens = tr.total_input + tr.cache_read + tr.cache_creation + tr.output;
        // Safe: token totals are well below 2^53; cast precision is
        // not material for percentile reporting.
        #[allow(clippy::cast_precision_loss)]
        let total_tokens_f = total_tokens as f64;
        let bucket = nearest_bucket(estimate);
        rows.push(CostPerPointRow {
            ticket: tr.ticket.clone(),
            estimate,
            bucket,
            total_tokens,
            cost_usd: tr.cost_usd,
            tokens_per_point: total_tokens_f / estimate,
            cost_per_point: tr.cost_usd / estimate,
        });
    }

    // Phase 4: bucket aggregation.
    let mut by_bucket: HashMap<u8, Vec<&CostPerPointRow>> = HashMap::new();
    for row in &rows {
        by_bucket.entry(row.bucket).or_default().push(row);
    }

    let mut buckets: BTreeMap<u8, BucketStats> = BTreeMap::new();
    for &b in BUCKETS {
        if let Some(items) = by_bucket.get(&b) {
            buckets.insert(b, compute_bucket_stats(items));
        }
    }

    // Phase 5: drift ratio (bucket 8 median / bucket 2 median).
    let drift_ratio = match (buckets.get(&8), buckets.get(&2)) {
        (Some(hi), Some(lo)) if lo.cost_p50 > 0.0 => Some(hi.cost_p50 / lo.cost_p50),
        _ => None,
    };
    let drift_alarm = drift_ratio.is_some_and(|r| r > DRIFT_ALARM_THRESHOLD);

    // Phase 6: write outputs (overwrite — input is source of truth).
    ensure_parent(output_jsonl)?;
    let mut jsonl_file =
        File::create(output_jsonl).with_context(|| format!("create {}", output_jsonl.display()))?;
    // Sort: descending cost_per_point so the most expensive points
    // surface first when scanning the JSONL by hand.
    let mut sorted_rows = rows.clone();
    sorted_rows.sort_by(|a, b| {
        b.cost_per_point
            .partial_cmp(&a.cost_per_point)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.ticket.cmp(&b.ticket))
    });
    for row in &sorted_rows {
        let line = serde_json::to_string(row)?;
        writeln!(jsonl_file, "{line}")?;
    }
    jsonl_file.flush()?;

    let summary = CostPerPointSummary {
        tickets_total: token_rows.len(),
        tickets_with_estimate: rows.len(),
        buckets: buckets.clone(),
        drift_ratio_high_vs_low: drift_ratio,
        drift_alarm,
    };
    ensure_parent(output_summary)?;
    let mut summary_file = File::create(output_summary)
        .with_context(|| format!("create {}", output_summary.display()))?;
    serde_json::to_writer_pretty(&mut summary_file, &summary)?;
    summary_file.flush()?;

    Ok(CostPerPointReport {
        tickets_analyzed: token_rows.len(),
        tickets_with_estimate: rows.len(),
        buckets,
        drift_ratio_high_vs_low: drift_ratio,
        drift_alarm,
    })
}

/// Round `estimate` to the nearest valid bucket.
///
/// Ties round UP (so 6 → 8 not 5; 4 → 5 not 3; 12 → 16 not 8). This
/// mirrors how teams usually escalate — when the true effort sits
/// between two Fibonacci values, the larger size is the safer
/// commitment.
#[must_use]
pub fn nearest_bucket(estimate: f64) -> u8 {
    let mut best = BUCKETS[0];
    let mut best_dist = (f64::from(BUCKETS[0]) - estimate).abs();
    for &b in &BUCKETS[1..] {
        let d = (f64::from(b) - estimate).abs();
        // Strictly less → take it; equal (tie) → take the larger.
        // Iterating ascending means a tie with the prior bucket
        // already updates `best` to the larger one here.
        if d < best_dist || (d - best_dist).abs() < f64::EPSILON {
            best = b;
            best_dist = d;
        }
    }
    best
}

/// Linear-interpolated percentile (numpy default `linear`).
/// Returns 0.0 for empty input. `p` is in `0.0..=100.0`.
#[must_use]
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let p = p.clamp(0.0, 100.0);
    // Safe: sorted.len() ≥ 2 here, fits in f64 without precision loss
    // for any realistic ticket count (<< 2^53).
    #[allow(clippy::cast_precision_loss)]
    let n_minus_1 = (sorted.len() - 1) as f64;
    let rank = p / 100.0 * n_minus_1;
    // Safe: rank is in [0, n-1], fits a u32; cast to usize is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let low = rank.floor() as usize;
    let high = (low + 1).min(sorted.len() - 1);
    let frac = rank - rank.floor();
    sorted[low] + frac * (sorted[high] - sorted[low])
}

fn compute_bucket_stats(items: &[&CostPerPointRow]) -> BucketStats {
    let mut costs: Vec<f64> = items.iter().map(|r| r.cost_per_point).collect();
    let mut tokens: Vec<f64> = items.iter().map(|r| r.tokens_per_point).collect();
    costs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    tokens.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    BucketStats {
        n: items.len(),
        cost_p25: percentile(&costs, 25.0),
        cost_p50: percentile(&costs, 50.0),
        cost_p75: percentile(&costs, 75.0),
        cost_p90: percentile(&costs, 90.0),
        tokens_p25: percentile(&tokens, 25.0),
        tokens_p50: percentile(&tokens, 50.0),
        tokens_p75: percentile(&tokens, 75.0),
        tokens_p90: percentile(&tokens, 90.0),
    }
}

/// Single-ticket projection of a SEN-7 row — only the fields we need.
#[derive(Debug, Clone)]
struct TokenRow {
    ticket: String,
    total_input: u64,
    cache_read: u64,
    cache_creation: u64,
    output: u64,
    cost_usd: f64,
}

fn load_token_rows(path: &Path) -> Result<Vec<TokenRow>> {
    let mut rows = Vec::new();
    if !path.exists() {
        return Ok(rows);
    }
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(std::result::Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(ticket) = value.get("ticket").and_then(|v| v.as_str()) else {
            continue;
        };
        rows.push(TokenRow {
            ticket: ticket.to_string(),
            total_input: u64_field(&value, "total_input"),
            cache_read: u64_field(&value, "cache_read"),
            cache_creation: u64_field(&value, "cache_creation"),
            output: u64_field(&value, "output"),
            cost_usd: value
                .get("cost_usd")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
        });
    }
    Ok(rows)
}

/// Load Linear cache, building a `ticket → estimate` map. Accepts:
///
/// * a top-level JSON array of issue objects (`[{...}, ...]`)
/// * an object with an `issues` key (`{"issues": [...]}`)
///
/// The estimate is read from any of these field names (in order):
/// `estimate`, `estimateValue`, `estimate_value`, `points`,
/// `story_points`, `storyPoints`. The ticket id is read from
/// `identifier` first, then `id`. Issues with no estimate or
/// non-numeric estimates are simply not added to the map.
///
/// If the file does not exist, returns an empty map (the analyzer
/// reports `tickets_with_estimate: 0` rather than failing — the
/// honest answer when no estimate cache is on disk yet).
pub fn load_estimates(path: &Path) -> Result<HashMap<String, f64>> {
    let mut map = HashMap::new();
    if !path.exists() {
        return Ok(map);
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse JSON {}", path.display()))?;

    let issues: &[serde_json::Value] = if let Some(arr) = value.as_array() {
        arr
    } else if let Some(arr) = value.get("issues").and_then(serde_json::Value::as_array) {
        arr
    } else {
        return Ok(map);
    };

    for issue in issues {
        let Some(ticket) = ticket_id_from_issue(issue) else {
            continue;
        };
        let Some(estimate) = estimate_from_issue(issue) else {
            continue;
        };
        if !estimate.is_finite() || estimate <= 0.0 {
            continue;
        }
        map.insert(ticket, estimate);
    }
    Ok(map)
}

fn ticket_id_from_issue(issue: &serde_json::Value) -> Option<String> {
    issue
        .get("identifier")
        .and_then(|v| v.as_str())
        .or_else(|| {
            // Fallback: only treat `id` as a ticket id if it looks
            // like one (PREFIX-NUMBER) — Linear UUIDs would
            // otherwise pollute the map.
            issue.get("id").and_then(|v| v.as_str()).filter(|s| {
                s.chars().any(|c| c == '-')
                    && s.split('-')
                        .next()
                        .is_some_and(|p| p.chars().all(char::is_alphabetic) && !p.is_empty())
            })
        })
        .map(str::to_string)
}

fn estimate_from_issue(issue: &serde_json::Value) -> Option<f64> {
    const FIELDS: &[&str] = &[
        "estimate",
        "estimateValue",
        "estimate_value",
        "points",
        "story_points",
        "storyPoints",
    ];
    for field in FIELDS {
        let v = issue.get(*field)?;
        if v.is_null() {
            continue;
        }
        if let Some(n) = v.as_f64() {
            return Some(n);
        }
        if let Some(s) = v.as_str() {
            if let Ok(n) = s.parse::<f64>() {
                return Some(n);
            }
        }
    }
    None
}

fn u64_field(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0)
}

fn ensure_parent(p: &Path) -> Result<()> {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn nearest_bucket_exact_matches() {
        for &b in BUCKETS {
            assert_eq!(nearest_bucket(f64::from(b)), b);
        }
    }

    #[test]
    fn nearest_bucket_rounds_to_closest() {
        // Closer to 3 than 5.
        assert_eq!(nearest_bucket(3.4), 3);
        // Equal distance 4 → ties round UP to 5.
        assert_eq!(nearest_bucket(4.0), 5);
        // 6 closer to 5 than 8.
        assert_eq!(nearest_bucket(6.0), 5);
        // 7 closer to 8 than 5.
        assert_eq!(nearest_bucket(7.0), 8);
        // 12 equidistant from 8 and 16 → ties round UP.
        assert_eq!(nearest_bucket(12.0), 16);
        // Above largest bucket clamps.
        assert_eq!(nearest_bucket(100.0), 16);
        // Below smallest clamps.
        assert_eq!(nearest_bucket(0.5), 1);
    }

    #[test]
    fn percentile_known_values() {
        let v: Vec<f64> = (1..=10).map(f64::from).collect();
        // numpy default 'linear' over [1..10]
        assert!((percentile(&v, 25.0) - 3.25).abs() < 1e-9);
        assert!((percentile(&v, 50.0) - 5.5).abs() < 1e-9);
        assert!((percentile(&v, 75.0) - 7.75).abs() < 1e-9);
        assert!((percentile(&v, 90.0) - 9.1).abs() < 1e-9);
    }

    #[test]
    fn percentile_edge_cases() {
        assert!((percentile(&[], 50.0)).abs() < 1e-9);
        assert!((percentile(&[42.0], 50.0) - 42.0).abs() < 1e-9);
        assert!((percentile(&[1.0, 2.0], 0.0) - 1.0).abs() < 1e-9);
        assert!((percentile(&[1.0, 2.0], 100.0) - 2.0).abs() < 1e-9);
    }

    fn write_tokens(dir: &Path, rows: &[(&str, u64, u64, u64, u64, f64)]) -> std::path::PathBuf {
        use std::fmt::Write as _;
        let path = dir.join("tokens.jsonl");
        let mut s = String::new();
        for (ticket, ti, cr, cc, out, cost) in rows {
            writeln!(
                &mut s,
                "{{\"ticket\":\"{ticket}\",\"sessions\":1,\"total_input\":{ti},\"cache_read\":{cr},\"cache_creation\":{cc},\"output\":{out},\"cost_usd\":{cost},\"models\":{{\"opus-4-7\":1}},\"confidence\":\"high\"}}"
            )
            .unwrap();
        }
        fs::write(&path, s).unwrap();
        path
    }

    fn write_linear_cache(dir: &Path, issues: &[(&str, Option<f64>)]) -> std::path::PathBuf {
        let path = dir.join("linear.json");
        let arr: Vec<serde_json::Value> = issues
            .iter()
            .map(|(id, est)| {
                let mut m = serde_json::Map::new();
                m.insert("identifier".into(), serde_json::Value::String((*id).into()));
                m.insert(
                    "estimate".into(),
                    est.map_or(serde_json::Value::Null, |e| {
                        serde_json::Number::from_f64(e)
                            .map_or(serde_json::Value::Null, serde_json::Value::Number)
                    }),
                );
                serde_json::Value::Object(m)
            })
            .collect();
        fs::write(&path, serde_json::to_string(&arr).unwrap()).unwrap();
        path
    }

    #[test]
    fn empty_estimates_returns_clean_report() {
        let dir = TempDir::new().unwrap();
        let tokens = write_tokens(
            dir.path(),
            &[
                ("FPCRM-1", 1_000_000, 0, 0, 0, 15.0),
                ("FPCRM-2", 1_000_000, 0, 0, 0, 15.0),
            ],
        );
        // 0 tickets carry estimates.
        let linear = write_linear_cache(dir.path(), &[("FPCRM-1", None), ("FPCRM-2", None)]);
        let out_jsonl = dir.path().join("cpp.jsonl");
        let out_summary = dir.path().join("cpp.json");
        let report = scan_cost_per_point(&tokens, &linear, &out_jsonl, &out_summary).unwrap();
        assert_eq!(report.tickets_analyzed, 2);
        assert_eq!(report.tickets_with_estimate, 0);
        assert!(report.buckets.is_empty());
        assert!(report.drift_ratio_high_vs_low.is_none());
        assert!(!report.drift_alarm);
        // Output JSONL exists and is empty.
        assert!(out_jsonl.exists());
        assert_eq!(fs::read_to_string(&out_jsonl).unwrap(), "");
        // Summary parses back.
        let summary: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&out_summary).unwrap()).unwrap();
        assert_eq!(summary["tickets_with_estimate"], 0);
    }

    #[test]
    fn drift_alarm_triggers_when_high_bucket_costs_six_x() {
        let dir = TempDir::new().unwrap();
        // 5 bucket-2 tickets at cost_per_point = $1
        // 5 bucket-8 tickets at cost_per_point = $6
        // Drift ratio = 6.0 > 5.0 threshold → alarm.
        let mut token_rows = Vec::new();
        for i in 0..5 {
            token_rows.push((
                format!("LO-{i}"),
                0_u64,
                0_u64,
                0_u64,
                0_u64,
                2.0_f64, // estimate=2 → cost/pt = 2/2 = $1
            ));
        }
        for i in 0..5 {
            token_rows.push((
                format!("HI-{i}"),
                0_u64,
                0_u64,
                0_u64,
                0_u64,
                48.0_f64, // estimate=8 → cost/pt = 48/8 = $6
            ));
        }
        // Now write tokens + linear cache.
        let tokens_refs: Vec<(&str, u64, u64, u64, u64, f64)> = token_rows
            .iter()
            .map(|(t, a, b, c, d, e)| (t.as_str(), *a, *b, *c, *d, *e))
            .collect();
        let tokens = write_tokens(dir.path(), &tokens_refs);
        let mut issues: Vec<(&str, Option<f64>)> = Vec::new();
        for i in 0..5 {
            // Hold owned strings outside this loop via leak; tests only.
            let s = Box::leak(format!("LO-{i}").into_boxed_str());
            issues.push((s, Some(2.0)));
        }
        for i in 0..5 {
            let s = Box::leak(format!("HI-{i}").into_boxed_str());
            issues.push((s, Some(8.0)));
        }
        let linear = write_linear_cache(dir.path(), &issues);
        let out_jsonl = dir.path().join("cpp.jsonl");
        let out_summary = dir.path().join("cpp.json");
        let report = scan_cost_per_point(&tokens, &linear, &out_jsonl, &out_summary).unwrap();
        assert_eq!(report.tickets_analyzed, 10);
        assert_eq!(report.tickets_with_estimate, 10);
        let lo = report.buckets.get(&2).expect("bucket 2 present");
        let hi = report.buckets.get(&8).expect("bucket 8 present");
        assert_eq!(lo.n, 5);
        assert_eq!(hi.n, 5);
        assert!((lo.cost_p50 - 1.0).abs() < 1e-9);
        assert!((hi.cost_p50 - 6.0).abs() < 1e-9);
        let ratio = report.drift_ratio_high_vs_low.expect("ratio computed");
        assert!((ratio - 6.0).abs() < 1e-9);
        assert!(report.drift_alarm, "drift alarm should fire when ratio > 5");
    }

    #[test]
    fn bucket_assignment_routes_estimates_to_canonical_buckets() {
        let dir = TempDir::new().unwrap();
        // estimate 5 → bucket 5; estimate 6 → bucket 5 (closer than 8).
        let tokens = write_tokens(
            dir.path(),
            &[
                ("A-1", 0, 0, 0, 0, 50.0),
                ("A-2", 0, 0, 0, 0, 60.0),
                ("A-3", 0, 0, 0, 0, 70.0),
            ],
        );
        let issues: Vec<(&str, Option<f64>)> = vec![
            ("A-1", Some(5.0)),
            ("A-2", Some(6.0)),
            ("A-3", Some(7.0)), // closer to 8
        ];
        let linear = write_linear_cache(dir.path(), &issues);
        let out_jsonl = dir.path().join("cpp.jsonl");
        let out_summary = dir.path().join("cpp.json");
        let report = scan_cost_per_point(&tokens, &linear, &out_jsonl, &out_summary).unwrap();
        assert_eq!(report.tickets_with_estimate, 3);
        // Read the JSONL back and check bucket assignments.
        let body = fs::read_to_string(&out_jsonl).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        let rows: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let bucket_for = |id: &str| -> u64 {
            rows.iter().find(|r| r["ticket"] == id).unwrap()["bucket"]
                .as_u64()
                .unwrap()
        };
        assert_eq!(bucket_for("A-1"), 5);
        assert_eq!(bucket_for("A-2"), 5);
        assert_eq!(bucket_for("A-3"), 8);
    }

    #[test]
    fn linear_cache_with_issues_object_shape_works() {
        let dir = TempDir::new().unwrap();
        let tokens = write_tokens(dir.path(), &[("X-1", 0, 0, 0, 0, 10.0)]);
        // Object shape: {"issues": [...]}
        let path = dir.path().join("linear.json");
        let body = serde_json::json!({
            "updated_at": "2026-04-30T00:00:00Z",
            "issues": [{"identifier": "X-1", "estimate": 5.0}]
        });
        fs::write(&path, body.to_string()).unwrap();
        let out_jsonl = dir.path().join("cpp.jsonl");
        let out_summary = dir.path().join("cpp.json");
        let report = scan_cost_per_point(&tokens, &path, &out_jsonl, &out_summary).unwrap();
        assert_eq!(report.tickets_with_estimate, 1);
        assert!(report.buckets.contains_key(&5));
    }

    #[test]
    fn missing_inputs_produce_empty_outputs_not_errors() {
        let dir = TempDir::new().unwrap();
        let tokens = dir.path().join("does-not-exist.jsonl");
        let linear = dir.path().join("also-missing.json");
        let out_jsonl = dir.path().join("cpp.jsonl");
        let out_summary = dir.path().join("cpp.json");
        let report = scan_cost_per_point(&tokens, &linear, &out_jsonl, &out_summary).unwrap();
        assert_eq!(report.tickets_analyzed, 0);
        assert_eq!(report.tickets_with_estimate, 0);
        assert!(out_jsonl.exists());
        assert!(out_summary.exists());
    }
}
