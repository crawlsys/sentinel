//! Cache hit rate + token efficiency tracker (SEN-14).
//!
//! Walks `~/.claude/projects/*/` for session JSONL files and computes,
//! for every session, the prompt-cache hit rate:
//!
//! ```text
//! cache_hit_rate = cache_read_input_tokens
//!     / (cache_read_input_tokens + input_tokens + cache_creation_input_tokens)
//! ```
//!
//! Aggregates p50/p90 globally and per-project, surfaces top-N "worst"
//! sessions by waste estimate `(1 - hit_rate) * total_tokens`, and writes
//! both a per-session JSONL and an aggregate summary JSON.
//!
//! Reuses the parsing/walking pattern from `tokens.rs` (SEN-7). The two
//! aggregators intentionally duplicate the walker for now — a follow-up
//! task may consolidate them into a shared session-walker helper.

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Average USD cost of a non-cached input token for the worst-session
/// "dollar waste" estimate. Conservative: we use Opus blended (input
/// $15/Mtok); cache-creation also costs ~$18.75/Mtok and cache-read
/// only $1.50/Mtok, so the saved-spend gap is at least this big.
const APPROX_INPUT_DOLLARS_PER_TOKEN: f64 = 15.0 / 1_000_000.0;

/// Per-session row written to the per-session JSONL.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRow {
    pub session_id: String,
    pub project: String,
    pub date: String,
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub total_input_tokens: u64,
    /// `None` when the session contains zero usage (avoids 0/0 NaN).
    pub cache_hit_rate: Option<f64>,
    /// Estimated wasted dollars vs. a 100% hit-rate session.
    pub waste_estimate_usd: f64,
}

/// Per-project aggregate written into the summary JSON.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectAggregate {
    pub project: String,
    pub sessions: u64,
    pub p50_hit_rate: f64,
    pub p90_hit_rate: f64,
    pub total_input_tokens: u64,
    pub total_cache_read: u64,
}

/// Worst-session entry, ranked by `(1 - hit_rate) * total_tokens`.
#[derive(Debug, Clone, Serialize)]
pub struct WorstSession {
    pub session_id: String,
    pub project: String,
    pub date: String,
    pub hit_rate: f64,
    pub total_input_tokens: u64,
    pub waste_estimate_usd: f64,
}

/// Per-day rolling aggregate.
#[derive(Debug, Clone, Serialize)]
pub struct DailyPoint {
    pub date: String,
    pub sessions: u64,
    pub hit_rate: f64,
}

/// Summary file shape.
#[derive(Debug, Clone, Serialize)]
pub struct CacheSummary {
    pub generated_at: String,
    pub sessions_scanned: u64,
    pub sessions_with_usage: u64,
    pub p50_hit_rate: f64,
    pub p90_hit_rate: f64,
    pub by_project: Vec<ProjectAggregate>,
    pub by_day: Vec<DailyPoint>,
    pub worst_sessions: Vec<WorstSession>,
}

/// Live report returned to the CLI for printing.
#[derive(Debug, Default, Clone)]
pub struct CacheReport {
    pub sessions_scanned: u64,
    pub sessions_with_usage: u64,
    pub p50_hit_rate: f64,
    pub p90_hit_rate: f64,
    pub worst_sessions: Vec<WorstSession>,
    pub daily_trend: Vec<DailyPoint>,
}

/// Walk `projects_root`, compute per-session cache hit rates, write
/// `output_jsonl` and `output_summary`. Both outputs are full-overwrite.
pub fn scan_cache_efficiency(
    projects_root: &Path,
    output_jsonl: &Path,
    output_summary: &Path,
) -> Result<CacheReport> {
    let mut rows: Vec<SessionRow> = Vec::new();

    if projects_root.exists() {
        for entry in fs::read_dir(projects_root)
            .with_context(|| format!("read_dir {}", projects_root.display()))?
            .flatten()
        {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let project = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            for jsonl in list_jsonl_files(&path) {
                if let Some(row) = parse_session(&jsonl, &project) {
                    rows.push(row);
                }
            }
        }
    }

    // Per-session JSONL output (one row per session, including
    // sessions with zero usage so the output is exhaustive).
    ensure_parent(output_jsonl)?;
    let mut jsonl_file =
        File::create(output_jsonl).with_context(|| format!("create {}", output_jsonl.display()))?;
    for row in &rows {
        let line = serde_json::to_string(row)?;
        writeln!(jsonl_file, "{line}")?;
    }
    jsonl_file.flush()?;

    // Aggregates: only sessions with usage contribute to percentiles.
    let with_usage: Vec<&SessionRow> = rows.iter().filter(|r| r.cache_hit_rate.is_some()).collect();

    let hit_rates: Vec<f64> = with_usage
        .iter()
        .map(|r| r.cache_hit_rate.unwrap_or(0.0))
        .collect();
    let p50 = percentile(&hit_rates, 0.50);
    let p90 = percentile(&hit_rates, 0.90);

    let by_day = aggregate_by_day(&with_usage);
    let worst_sessions = top_worst_sessions(&with_usage, 10);

    let summary = CacheSummary {
        generated_at: Utc::now().to_rfc3339(),
        sessions_scanned: rows.len() as u64,
        sessions_with_usage: with_usage.len() as u64,
        p50_hit_rate: p50,
        p90_hit_rate: p90,
        by_project: aggregate_by_project(&with_usage),
        by_day: by_day.clone(),
        worst_sessions: worst_sessions.clone(),
    };
    // by_day / worst_sessions are reused below for the CacheReport
    // return value, so the clones above are intentional.

    ensure_parent(output_summary)?;
    let mut summary_file = File::create(output_summary)
        .with_context(|| format!("create {}", output_summary.display()))?;
    summary_file.write_all(serde_json::to_string_pretty(&summary)?.as_bytes())?;
    summary_file.flush()?;

    Ok(CacheReport {
        sessions_scanned: rows.len() as u64,
        sessions_with_usage: with_usage.len() as u64,
        p50_hit_rate: p50,
        p90_hit_rate: p90,
        worst_sessions,
        daily_trend: by_day,
    })
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

fn ensure_parent(p: &Path) -> Result<()> {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    Ok(())
}

fn parse_session(jsonl: &Path, project: &str) -> Option<SessionRow> {
    let session_id = jsonl
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let file = File::open(jsonl).ok()?;
    let reader = BufReader::new(file);

    let mut input_tokens: u64 = 0;
    let mut cache_read: u64 = 0;
    let mut cache_creation: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut latest_ts: Option<DateTime<Utc>> = None;

    for line in reader.lines().map_while(std::result::Result::ok) {
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Track the latest timestamp on any line type for "session date".
        if let Some(ts) = value.get("timestamp").and_then(|v| v.as_str()) {
            if let Ok(parsed) = DateTime::parse_from_rfc3339(ts) {
                let utc: DateTime<Utc> = parsed.with_timezone(&Utc);
                latest_ts = Some(latest_ts.map_or(utc, |cur| cur.max(utc)));
            }
        }

        if value.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        let Some(usage) = message.get("usage") else {
            continue;
        };

        input_tokens = input_tokens.saturating_add(usage_u64(usage, "input_tokens"));
        output_tokens = output_tokens.saturating_add(usage_u64(usage, "output_tokens"));
        cache_read = cache_read.saturating_add(usage_u64(usage, "cache_read_input_tokens"));
        cache_creation =
            cache_creation.saturating_add(usage_u64(usage, "cache_creation_input_tokens"));
    }

    let total_input_tokens = input_tokens
        .saturating_add(cache_read)
        .saturating_add(cache_creation);

    // Edge case: no usage at all — emit a row with `None` hit_rate
    // so consumers see the session without panicking on 0/0.
    let cache_hit_rate = if total_input_tokens == 0 {
        None
    } else {
        #[allow(clippy::cast_precision_loss)]
        Some((cache_read as f64) / (total_input_tokens as f64))
    };

    let waste_estimate_usd = cache_hit_rate.map_or(0.0, |hr| {
        #[allow(clippy::cast_precision_loss)]
        let total = total_input_tokens as f64;
        (1.0 - hr) * total * APPROX_INPUT_DOLLARS_PER_TOKEN
    });

    let date = latest_ts.map_or_else(|| "unknown".to_string(), |t| t.date_naive().to_string());

    Some(SessionRow {
        session_id,
        project: project.to_string(),
        date,
        input_tokens,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_creation,
        output_tokens,
        total_input_tokens,
        cache_hit_rate,
        waste_estimate_usd: round4(waste_estimate_usd),
    })
}

fn usage_u64(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0)
}

/// Linear-interpolated percentile on an unsorted slice.
/// Returns 0.0 on empty input.
#[must_use]
pub fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    if sorted.len() == 1 {
        return sorted[0];
    }

    #[allow(clippy::cast_precision_loss)]
    let last_idx = (sorted.len() - 1) as f64;
    let pos = q.clamp(0.0, 1.0) * last_idx;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo = pos.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    let frac = pos - pos.floor();
    (sorted[hi] - sorted[lo]).mul_add(frac, sorted[lo])
}

fn aggregate_by_project(rows: &[&SessionRow]) -> Vec<ProjectAggregate> {
    let mut buckets: HashMap<String, Vec<&SessionRow>> = HashMap::new();
    for r in rows {
        buckets.entry(r.project.clone()).or_default().push(r);
    }
    let mut out: Vec<ProjectAggregate> = buckets
        .into_iter()
        .map(|(project, rs)| {
            let hit_rates: Vec<f64> = rs.iter().map(|r| r.cache_hit_rate.unwrap_or(0.0)).collect();
            let total_input = rs.iter().map(|r| r.total_input_tokens).sum::<u64>();
            let total_read = rs.iter().map(|r| r.cache_read_input_tokens).sum::<u64>();
            ProjectAggregate {
                project,
                sessions: rs.len() as u64,
                p50_hit_rate: percentile(&hit_rates, 0.50),
                p90_hit_rate: percentile(&hit_rates, 0.90),
                total_input_tokens: total_input,
                total_cache_read: total_read,
            }
        })
        .collect();
    out.sort_by_key(|b| std::cmp::Reverse(b.total_input_tokens));
    out
}

fn aggregate_by_day(rows: &[&SessionRow]) -> Vec<DailyPoint> {
    let mut buckets: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for r in rows {
        if r.date == "unknown" {
            continue;
        }
        // Validate date parses as YYYY-MM-DD.
        if NaiveDate::parse_from_str(&r.date, "%Y-%m-%d").is_err() {
            continue;
        }
        if let Some(rate) = r.cache_hit_rate {
            buckets.entry(r.date.clone()).or_default().push(rate);
            *counts.entry(r.date.clone()).or_insert(0) += 1;
        }
    }
    buckets
        .into_iter()
        .map(|(date, rates)| {
            #[allow(clippy::cast_precision_loss)]
            let mean = if rates.is_empty() {
                0.0
            } else {
                rates.iter().sum::<f64>() / (rates.len() as f64)
            };
            DailyPoint {
                sessions: counts.get(&date).copied().unwrap_or(0),
                date,
                hit_rate: round4(mean),
            }
        })
        .collect()
}

fn top_worst_sessions(rows: &[&SessionRow], n: usize) -> Vec<WorstSession> {
    // Filter to "long enough to matter" — anything with at least 50K
    // total input tokens. Otherwise tiny sessions with 0% hit rate
    // dominate the list with no real spend impact.
    let mut candidates: Vec<&&SessionRow> = rows
        .iter()
        .filter(|r| r.total_input_tokens >= 50_000)
        .collect();

    candidates.sort_by(|a, b| {
        let aw = waste_score(a);
        let bw = waste_score(b);
        bw.partial_cmp(&aw).unwrap_or(std::cmp::Ordering::Equal)
    });

    candidates
        .into_iter()
        .take(n)
        .map(|r| WorstSession {
            session_id: r.session_id.clone(),
            project: r.project.clone(),
            date: r.date.clone(),
            hit_rate: r.cache_hit_rate.unwrap_or(0.0),
            total_input_tokens: r.total_input_tokens,
            waste_estimate_usd: r.waste_estimate_usd,
        })
        .collect()
}

fn waste_score(r: &SessionRow) -> f64 {
    let hr = r.cache_hit_rate.unwrap_or(0.0);
    #[allow(clippy::cast_precision_loss)]
    let total = r.total_input_tokens as f64;
    (1.0 - hr) * total
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_session(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(format!("{name}.jsonl"));
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn hit_rate_computed_from_known_token_counts() {
        let dir = TempDir::new().unwrap();
        // 700 cache_read + 200 input + 100 cache_creation => 700/1000 = 0.70
        let body = r#"{"type":"user","message":{"role":"user","content":"hi"}}
{"type":"assistant","message":{"model":"claude-opus-4-7","usage":{"input_tokens":200,"output_tokens":50,"cache_read_input_tokens":700,"cache_creation_input_tokens":100}}}"#;
        let p = write_session(dir.path(), "s1", body);
        let row = parse_session(&p, "proj").unwrap();
        let hr = row.cache_hit_rate.unwrap();
        assert!((hr - 0.70).abs() < 1e-6, "hit rate was {hr}");
        assert_eq!(row.total_input_tokens, 1000);
        assert_eq!(row.cache_read_input_tokens, 700);
    }

    #[test]
    fn zero_usage_session_returns_none_hit_rate() {
        let dir = TempDir::new().unwrap();
        // A pr-link-only session: no assistant messages, no usage.
        let body = r#"{"type":"system","content":"pr-link"}"#;
        let p = write_session(dir.path(), "s2", body);
        let row = parse_session(&p, "proj").unwrap();
        assert!(
            row.cache_hit_rate.is_none(),
            "expected None, got {:?}",
            row.cache_hit_rate
        );
        assert_eq!(row.total_input_tokens, 0);
        assert!(row.waste_estimate_usd.abs() < f64::EPSILON);
    }

    #[test]
    fn percentile_p50_and_p90_match_expected() {
        // 11 values: 0.0, 0.1, 0.2, ..., 1.0
        let v: Vec<f64> = (0..=10).map(|i| f64::from(i) / 10.0).collect();
        let p50 = percentile(&v, 0.50);
        let p90 = percentile(&v, 0.90);
        assert!((p50 - 0.5).abs() < 1e-6, "p50 was {p50}");
        assert!((p90 - 0.9).abs() < 1e-6, "p90 was {p90}");
    }

    #[test]
    fn percentile_handles_empty_and_single() {
        assert!((percentile(&[], 0.5) - 0.0).abs() < 1e-9);
        assert!((percentile(&[0.42], 0.9) - 0.42).abs() < 1e-9);
    }

    #[test]
    fn worst_sessions_ranked_by_waste_score() {
        let mk = |id: &str, hr: f64, total: u64| -> SessionRow {
            SessionRow {
                session_id: id.into(),
                project: "p".into(),
                date: "2026-04-30".into(),
                input_tokens: total,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                output_tokens: 0,
                total_input_tokens: total,
                cache_hit_rate: Some(hr),
                waste_estimate_usd: 0.0,
            }
        };
        // (1-hr)*total: a=0.5*200_000=100_000; b=0.9*100_000=90_000; c=0.1*1_000_000=100_000
        let a = mk("a", 0.5, 200_000);
        let b = mk("b", 0.1, 100_000);
        let c = mk("c", 0.9, 1_000_000);
        let rows = vec![&a, &b, &c];
        let worst = top_worst_sessions(&rows, 3);
        assert_eq!(worst.len(), 3);
        // a and c tie at 100K waste tokens; b sits at 90K → b last.
        let last = worst.last().unwrap();
        assert_eq!(last.session_id, "b");
    }

    #[test]
    fn worst_sessions_filters_tiny_sessions() {
        let tiny = SessionRow {
            session_id: "tiny".into(),
            project: "p".into(),
            date: "2026-04-30".into(),
            input_tokens: 100,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 0,
            total_input_tokens: 100,
            cache_hit_rate: Some(0.0),
            waste_estimate_usd: 0.0,
        };
        let rows = vec![&tiny];
        let worst = top_worst_sessions(&rows, 10);
        assert!(worst.is_empty(), "tiny session should be filtered out");
    }

    #[test]
    fn end_to_end_scan_writes_jsonl_and_summary() {
        let dir = TempDir::new().unwrap();
        let projects = dir.path().join("projects");
        let proj_a = projects.join("proj-a");
        fs::create_dir_all(&proj_a).unwrap();

        // Session with 80% hit rate.
        let s1 = r#"{"type":"user","timestamp":"2026-04-29T10:00:00Z","message":{"role":"user","content":"hi"}}
{"type":"assistant","timestamp":"2026-04-29T10:00:01Z","message":{"model":"claude-opus-4-7","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":800,"cache_creation_input_tokens":100}}}"#;
        fs::write(proj_a.join("aaaa.jsonl"), s1).unwrap();

        // Session with 20% hit rate.
        let s2 = r#"{"type":"assistant","timestamp":"2026-04-30T10:00:01Z","message":{"model":"claude-opus-4-7","usage":{"input_tokens":700,"output_tokens":50,"cache_read_input_tokens":200,"cache_creation_input_tokens":100}}}"#;
        fs::write(proj_a.join("bbbb.jsonl"), s2).unwrap();

        let out_jsonl = dir.path().join("out").join("cache.jsonl");
        let out_summary = dir.path().join("out").join("summary.json");
        let report = scan_cache_efficiency(&projects, &out_jsonl, &out_summary).unwrap();

        assert_eq!(report.sessions_scanned, 2);
        assert_eq!(report.sessions_with_usage, 2);
        // p50 of {0.2, 0.8} = 0.5
        assert!(
            (report.p50_hit_rate - 0.5).abs() < 1e-6,
            "p50={}",
            report.p50_hit_rate
        );

        let jsonl_content = fs::read_to_string(&out_jsonl).unwrap();
        assert_eq!(jsonl_content.lines().count(), 2);

        let summary_content = fs::read_to_string(&out_summary).unwrap();
        let summary: serde_json::Value = serde_json::from_str(&summary_content).unwrap();
        assert_eq!(summary["sessions_scanned"], 2);
        assert_eq!(summary["sessions_with_usage"], 2);
        assert_eq!(summary["by_project"][0]["project"], "proj-a");
        // by_day has 2 entries (one per date).
        assert_eq!(summary["by_day"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn scan_against_missing_root_writes_empty_outputs() {
        let dir = TempDir::new().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let out_jsonl = dir.path().join("cache.jsonl");
        let out_summary = dir.path().join("summary.json");
        let report = scan_cache_efficiency(&nonexistent, &out_jsonl, &out_summary).unwrap();
        assert_eq!(report.sessions_scanned, 0);
        assert!(out_jsonl.exists());
        assert!(out_summary.exists());
        assert_eq!(fs::read_to_string(&out_jsonl).unwrap(), "");
    }
}
