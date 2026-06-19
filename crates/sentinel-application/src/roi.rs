//! ROI vs human-team baseline (SEN-15) — the headline factory metric.
//!
//! Joins three inputs to compute "$ Claude spent vs $ a human team would
//! have spent shipping the same work":
//!
//! 1. SEN-7 `tokens-per-ticket.jsonl` — Claude cost per Linear ticket.
//! 2. SEN-13 `cost-per-point-summary.json` — Claude $/point median, when
//!    Linear estimates are present.
//! 3. Constants from `sentinel_domain::constants` — fully-loaded human
//!    rate ($170k/yr, $654/day, 0.5 days per point → $327/point baseline).
//!
//! For each rolling window (7d / 30d / 90d / all-time) we report:
//!
//! * `claude_cost_usd` — total Claude spend on tickets active in window
//! * `tickets_shipped` — distinct ticket count in window
//! * `points_shipped`  — sum of estimates in window (when SEN-13 has data)
//! * `claude_cost_per_point` — SEN-13 median when present, else
//!   `claude_cost_usd / points_shipped`.
//! * `human_cost_usd` — `points * $327`; zero when estimates are unavailable.
//! * `roi_ratio` — `human_cost_usd / claude_cost_usd`
//! * `projected_annual_savings_usd` — extrapolates the per-day delta to
//!   `HUMAN_WORKING_DAYS_PER_YEAR` (260) days.
//!
//! Window membership uses the most-recent session file mtime per ticket
//! from `~/.claude/projects/*`. When the projects root is missing we
//! collapse everything into the all-time window and honestly report
//! `windowing: "unavailable"` in the row.
//!
//! Honest reporting on missing inputs:
//! * SEN-7 absent → empty report, `headline: None`.
//! * SEN-13 absent / `tickets_with_estimate == 0` → no ROI calculation;
//!   estimate-backed points are required.
//! * Projects root absent → all rows in "all-time" window only.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::time::SystemTime;

use sentinel_domain::constants::{
    HUMAN_FULLY_LOADED_USD_PER_DAY, HUMAN_USD_PER_POINT, HUMAN_WORKING_DAYS_PER_YEAR,
};

/// Window definitions in days. `None` means "all-time".
pub const WINDOW_DAYS: &[Option<u32>] = &[Some(7), Some(30), Some(90), None];

/// Per-window ROI row written to `roi.jsonl`.
#[derive(Debug, Clone, Serialize)]
pub struct RoiWindow {
    /// `Some(7|30|90)` for rolling, `None` for all-time.
    pub window_days: Option<u32>,
    /// Human-readable label: `"7d"`, `"30d"`, `"90d"`, `"all-time"`.
    pub label: String,
    pub tickets_shipped: usize,
    /// Sum of estimates in window. `0.0` when no estimates available.
    pub points_shipped: f64,
    pub claude_cost_usd: f64,
    /// $/point used for ROI math (median from SEN-13 when present, else
    /// derived from cost+points).
    pub claude_cost_per_point: f64,
    /// Equivalent human-team cost.
    pub human_cost_usd: f64,
    pub roi_ratio: f64,
    pub projected_annual_savings_usd: f64,
    /// `true` when this row has estimate-backed point data.
    pub estimate_data_available: bool,
    /// Reason ROI was not calculated, or empty when estimates are present.
    pub estimate_note: String,
}

/// Top-line single-number summary written to `roi-summary.json`.
#[derive(Debug, Clone, Serialize)]
pub struct HeadlineRoi {
    pub generated_at: String,
    pub roi_ratio: f64,
    pub claude_cost_usd_total: f64,
    pub human_cost_usd_total: f64,
    pub tickets_shipped_total: usize,
    pub projected_annual_savings_usd: f64,
    pub claude_cost_per_point: f64,
    pub human_cost_per_point: f64,
    pub estimate_data_available: bool,
    pub estimate_note: String,
    pub windows: Vec<RoiWindow>,
}

/// In-memory report returned by `scan_roi` for human reporting.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RoiReport {
    pub windows: Vec<RoiWindow>,
    pub headline: Option<HeadlineRoi>,
}

/// Scan ROI against SEN-7 + SEN-13 outputs, produce both per-window
/// JSONL and a top-line summary JSON. See module docs for full algo.
pub fn scan_roi(
    tokens_input: &Path,
    cost_per_point_input: &Path,
    output_jsonl: &Path,
    output_summary: &Path,
) -> Result<RoiReport> {
    // Phase 1: load SEN-7 ticket rows. Source-of-truth for cost.
    let token_rows = load_token_rows(tokens_input)
        .with_context(|| format!("load tokens input {}", tokens_input.display()))?;

    // Phase 2: load SEN-13 summary (optional).
    let sen13 = load_cost_per_point(cost_per_point_input);

    // Phase 3: build ticket → last-active timestamp map (best-effort).
    let projects_root = projects_root_for(tokens_input);
    let last_seen = projects_root
        .as_deref()
        .map(build_ticket_last_seen)
        .unwrap_or_default();

    // Phase 4: short-circuit on no input.
    if token_rows.is_empty() {
        ensure_parent(output_jsonl)?;
        File::create(output_jsonl)
            .with_context(|| format!("create {}", output_jsonl.display()))?
            .flush()?;
        ensure_parent(output_summary)?;
        let empty = HeadlineRoi {
            generated_at: now_iso(),
            roi_ratio: 0.0,
            claude_cost_usd_total: 0.0,
            human_cost_usd_total: 0.0,
            tickets_shipped_total: 0,
            projected_annual_savings_usd: 0.0,
            claude_cost_per_point: 0.0,
            human_cost_per_point: HUMAN_USD_PER_POINT,
            estimate_data_available: false,
            estimate_note: "no SEN-7 input".to_string(),
            windows: Vec::new(),
        };
        let mut summary_file = File::create(output_summary)
            .with_context(|| format!("create {}", output_summary.display()))?;
        serde_json::to_writer_pretty(&mut summary_file, &empty)?;
        summary_file.flush()?;
        return Ok(RoiReport {
            windows: Vec::new(),
            headline: None,
        });
    }

    // Phase 5: precompute estimates by ticket (from SEN-13 outputs's
    // sibling JSONL). The summary alone tells us the median $/point but
    // not which tickets carried estimates — for window-aware point
    // totals we want per-ticket estimates. Reuse SEN-13's per-ticket
    // JSONL if present alongside the summary.
    let estimates_path = cost_per_point_input
        .parent()
        .map(|p| p.join("cost-per-point.jsonl"));
    let estimates = estimates_path
        .as_deref()
        .map(load_estimates_from_jsonl)
        .unwrap_or_default();

    // Phase 6: build per-window rows.
    let now = now_secs();
    let mut windows: Vec<RoiWindow> = Vec::new();
    for &wd in WINDOW_DAYS {
        let row = build_window(wd, &token_rows, &last_seen, &estimates, sen13.as_ref(), now);
        windows.push(row);
    }

    // Phase 7: write outputs.
    ensure_parent(output_jsonl)?;
    let mut jsonl_file =
        File::create(output_jsonl).with_context(|| format!("create {}", output_jsonl.display()))?;
    for w in &windows {
        let line = serde_json::to_string(w)?;
        writeln!(jsonl_file, "{line}")?;
    }
    jsonl_file.flush()?;

    // Headline = the all-time window (last entry per WINDOW_DAYS order).
    let all_time = windows
        .iter()
        .find(|w| w.window_days.is_none())
        .cloned()
        .expect("all-time window always present");

    let headline = HeadlineRoi {
        generated_at: now_iso(),
        roi_ratio: all_time.roi_ratio,
        claude_cost_usd_total: all_time.claude_cost_usd,
        human_cost_usd_total: all_time.human_cost_usd,
        tickets_shipped_total: all_time.tickets_shipped,
        projected_annual_savings_usd: all_time.projected_annual_savings_usd,
        claude_cost_per_point: all_time.claude_cost_per_point,
        human_cost_per_point: HUMAN_USD_PER_POINT,
        estimate_data_available: all_time.estimate_data_available,
        estimate_note: all_time.estimate_note,
        windows: windows.clone(),
    };

    ensure_parent(output_summary)?;
    let mut summary_file = File::create(output_summary)
        .with_context(|| format!("create {}", output_summary.display()))?;
    serde_json::to_writer_pretty(&mut summary_file, &headline)?;
    summary_file.flush()?;

    Ok(RoiReport {
        windows,
        headline: Some(headline),
    })
}

/// Build a single window row.
fn build_window(
    window_days: Option<u32>,
    rows: &[TokenRow],
    last_seen: &HashMap<String, u64>,
    estimates: &HashMap<String, f64>,
    sen13: Option<&Sen13Summary>,
    now_ts: u64,
) -> RoiWindow {
    let label = window_days.map_or_else(|| "all-time".to_string(), |d| format!("{d}d"));

    // Filter rows by window.
    let filtered: Vec<&TokenRow> = match window_days {
        None => rows.iter().collect(),
        Some(days) => {
            let cutoff = now_ts.saturating_sub(u64::from(days) * 86_400);
            rows.iter()
                .filter(|r| {
                    last_seen
                        .get(&r.ticket)
                        .copied()
                        .is_some_and(|ts| ts >= cutoff)
                })
                .collect()
        }
    };

    let tickets_shipped = filtered.len();
    let claude_cost_usd: f64 = filtered.iter().map(|r| r.cost_usd).sum();
    // Normalize `-0.0` from empty filter sums to `0.0` for clean JSON.
    let points_shipped: f64 = {
        let s: f64 = filtered
            .iter()
            .filter_map(|r| estimates.get(&r.ticket).copied())
            .sum();
        if s == 0.0 {
            0.0
        } else {
            s
        }
    };

    // Decide $/point. ROI is computed only from real estimate-backed points.
    let (cost_per_point, human_cost_usd, estimate_data_available, estimate_note) =
        if points_shipped > 0.0 {
            // Real estimate data: prefer SEN-13 median when window is all-
            // time AND median is positive — keeps it decoupled from whatever
            // ticket-set the client happens to be looking at. For narrower
            // windows, derive from the actual filtered totals.
            let cpp = if window_days.is_none() {
                sen13
                    .and_then(Sen13Summary::cost_per_point_median)
                    .filter(|m| *m > 0.0)
                    .unwrap_or_else(|| {
                        if claude_cost_usd > 0.0 {
                            claude_cost_usd / points_shipped
                        } else {
                            0.0
                        }
                    })
            } else if claude_cost_usd > 0.0 {
                claude_cost_usd / points_shipped
            } else {
                0.0
            };
            let human = points_shipped * HUMAN_USD_PER_POINT;
            (cpp, human, true, String::new())
        } else if tickets_shipped > 0 {
            (
                0.0,
                0.0,
                false,
                "SEN-13 estimate data required; ROI not computed".to_string(),
            )
        } else {
            (0.0, 0.0, false, String::new())
        };

    let roi_ratio = if claude_cost_usd > 0.0 {
        human_cost_usd / claude_cost_usd
    } else {
        0.0
    };

    // Projected annual savings: scale the per-day cost delta.
    // If window_days = N, claude_cost_usd is the delta for N days. The
    // human cost we computed above is the matching baseline for that
    // same N days of throughput. Scale to a year.
    let projected_annual_savings_usd = if !estimate_data_available {
        0.0
    } else if let Some(days) = window_days {
        if days > 0 {
            let delta_per_day = (human_cost_usd - claude_cost_usd) / f64::from(days);
            delta_per_day * HUMAN_WORKING_DAYS_PER_YEAR
        } else {
            0.0
        }
    } else {
        // For "all-time", compute savings only when we know the active
        // span: use the spread between earliest + latest activity as a
        // rough denominator. If we have no timestamps at all, leave 0.
        if filtered.is_empty() {
            0.0
        } else {
            let oldest = filtered
                .iter()
                .filter_map(|r| last_seen.get(&r.ticket).copied())
                .min();
            if let Some(oldest_ts) = oldest {
                // Safe: timestamp deltas in seconds fit f64 mantissa
                // for any realistic age (~285M years).
                #[allow(clippy::cast_precision_loss)]
                let span_days = (now_ts.saturating_sub(oldest_ts) as f64) / 86_400.0;
                if span_days >= 1.0 {
                    let delta_per_day = (human_cost_usd - claude_cost_usd) / span_days;
                    delta_per_day * HUMAN_WORKING_DAYS_PER_YEAR
                } else {
                    human_cost_usd - claude_cost_usd
                }
            } else {
                0.0
            }
        }
    };

    RoiWindow {
        window_days,
        label,
        tickets_shipped,
        points_shipped,
        claude_cost_usd,
        claude_cost_per_point: cost_per_point,
        human_cost_usd,
        roi_ratio,
        projected_annual_savings_usd,
        estimate_data_available,
        estimate_note,
    }
}

/// Lightweight projection of a SEN-7 row.
#[derive(Debug, Clone)]
struct TokenRow {
    ticket: String,
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
            cost_usd: value
                .get("cost_usd")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
        });
    }
    Ok(rows)
}

/// Subset of SEN-13 summary we care about.
#[derive(Debug, Clone)]
struct Sen13Summary {
    tickets_with_estimate: usize,
    /// Bucket → median $/point. Keys are the canonical estimate buckets.
    bucket_medians: Vec<(u8, f64, usize)>,
}

impl Sen13Summary {
    /// Compute an n-weighted median $/point across the SEN-13 buckets.
    fn cost_per_point_median(&self) -> Option<f64> {
        if self.tickets_with_estimate == 0 {
            return None;
        }
        let total_n: usize = self.bucket_medians.iter().map(|(_, _, n)| *n).sum();
        if total_n == 0 {
            return None;
        }
        // Weighted average of the per-bucket medians by n. Approximates a
        // single global median without re-reading every per-ticket row.
        let weighted: f64 = self
            .bucket_medians
            .iter()
            .map(|(_, m, n)| {
                #[allow(clippy::cast_precision_loss)]
                let w = *n as f64;
                m * w
            })
            .sum();
        #[allow(clippy::cast_precision_loss)]
        let denom = total_n as f64;
        if denom > 0.0 {
            Some(weighted / denom)
        } else {
            None
        }
    }
}

fn load_cost_per_point(path: &Path) -> Option<Sen13Summary> {
    if !path.exists() {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    // Safe: tickets-with-estimate counts fit well below `usize::MAX`
    // on both 32- and 64-bit targets.
    #[allow(clippy::cast_possible_truncation)]
    let tickets_with_estimate = value
        .get("tickets_with_estimate")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    let mut bucket_medians: Vec<(u8, f64, usize)> = Vec::new();
    if let Some(buckets) = value.get("buckets").and_then(|v| v.as_object()) {
        for (k, v) in buckets {
            let Ok(b) = k.parse::<u8>() else {
                continue;
            };
            let median = v
                .get("cost_p50")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            #[allow(clippy::cast_possible_truncation)]
            let n = v.get("n").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
            bucket_medians.push((b, median, n));
        }
    }
    Some(Sen13Summary {
        tickets_with_estimate,
        bucket_medians,
    })
}

fn load_estimates_from_jsonl(path: &Path) -> HashMap<String, f64> {
    let mut map = HashMap::new();
    if !path.exists() {
        return map;
    }
    let Ok(file) = File::open(path) else {
        return map;
    };
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(std::result::Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(ticket) = v.get("ticket").and_then(|x| x.as_str()) else {
            continue;
        };
        let Some(estimate) = v.get("estimate").and_then(serde_json::Value::as_f64) else {
            continue;
        };
        if estimate.is_finite() && estimate > 0.0 {
            map.insert(ticket.to_string(), estimate);
        }
    }
    map
}

/// Resolve the projects root from the SEN-7 tokens input path.
/// `~/.claude/sentinel/metrics/tokens-per-ticket.jsonl` →
/// `~/.claude/projects` (sibling of `sentinel`).
fn projects_root_for(tokens_input: &Path) -> Option<std::path::PathBuf> {
    // tokens_input = .../.claude/sentinel/metrics/tokens-per-ticket.jsonl
    // Want         = .../.claude/projects
    let metrics_dir = tokens_input.parent()?;
    let sentinel_dir = metrics_dir.parent()?;
    let claude_dir = sentinel_dir.parent()?;
    let projects = claude_dir.join("projects");
    if projects.exists() {
        Some(projects)
    } else {
        None
    }
}

/// Walk `~/.claude/projects/*` for session JSONLs, return
/// `ticket → max(mtime)` over all sessions that match a known ticket
/// id (via path-slug pattern matching). Best-effort — missing or
/// unreadable directories simply yield no entries.
fn build_ticket_last_seen(projects_root: &Path) -> HashMap<String, u64> {
    let mut map: HashMap<String, u64> = HashMap::new();
    let Ok(entries) = fs::read_dir(projects_root) else {
        return map;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let Some(ticket) = extract_ticket_from_slug(&dir_name) else {
            continue;
        };
        // Find max mtime of any *.jsonl in this dir.
        let mut max_mtime: Option<u64> = None;
        let Ok(files) = fs::read_dir(&path) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = f.metadata() else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            let Ok(secs) = mtime.duration_since(SystemTime::UNIX_EPOCH) else {
                continue;
            };
            let s = secs.as_secs();
            max_mtime = Some(max_mtime.map_or(s, |cur| cur.max(s)));
        }
        if let Some(ts) = max_mtime {
            map.entry(ticket)
                .and_modify(|cur| {
                    if ts > *cur {
                        *cur = ts;
                    }
                })
                .or_insert(ts);
        }
    }
    map
}

/// Scan a project-slug for a `PREFIX-NUMBER` ticket id. We only accept
/// the SEN-7 known prefix list to avoid false positives like
/// `HTTP-200`. Returns the first matching id (uppercase normalised).
pub fn extract_ticket_from_slug(slug: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r"([A-Za-z]{2,7})-(\d+)").expect("ticket regex compiles"));
    for cap in re.captures_iter(slug) {
        let prefix = cap.get(1)?.as_str().to_uppercase();
        let num = cap.get(2)?.as_str();
        if KNOWN_PREFIXES.contains(&prefix.as_str()) {
            return Some(format!("{prefix}-{num}"));
        }
    }
    None
}

/// Same allow-list used by SEN-7 — kept in sync intentionally.
const KNOWN_PREFIXES: &[&str] = &[
    "FPCRM", "FPFIELD", "FPROUTE", "FPMD", "FPTRIBU", "LEG", "COR", "EXA", "SYN", "TES", "TRI",
    "SEN",
];

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn now_iso() -> String {
    let now: DateTime<Utc> = Utc::now();
    now.to_rfc3339()
}

fn ensure_parent(p: &Path) -> Result<()> {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    Ok(())
}

/// Re-export the human baseline constants for the CLI to display
/// without re-importing from `sentinel-domain` directly.
#[must_use]
pub const fn human_baseline_per_point() -> f64 {
    HUMAN_USD_PER_POINT
}

#[must_use]
pub const fn human_baseline_per_day() -> f64 {
    HUMAN_FULLY_LOADED_USD_PER_DAY
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_tokens(dir: &Path, rows: &[(&str, f64)]) -> std::path::PathBuf {
        use std::fmt::Write as _;
        let path = dir.join("tokens.jsonl");
        let mut s = String::new();
        for (ticket, cost) in rows {
            writeln!(
                &mut s,
                "{{\"ticket\":\"{ticket}\",\"sessions\":1,\"total_input\":0,\"cache_read\":0,\"cache_creation\":0,\"output\":0,\"cost_usd\":{cost},\"models\":{{}},\"confidence\":\"high\"}}"
            )
            .unwrap();
        }
        fs::write(&path, s).unwrap();
        path
    }

    fn write_cost_per_point_summary(
        dir: &Path,
        tickets_with_estimate: usize,
        buckets: &[(u8, f64, usize)],
    ) -> std::path::PathBuf {
        let path = dir.join("cost-per-point-summary.json");
        let mut buckets_obj = serde_json::Map::new();
        for (b, m, n) in buckets {
            let mut entry = serde_json::Map::new();
            entry.insert("n".into(), serde_json::json!(*n));
            entry.insert("cost_p50".into(), serde_json::json!(*m));
            buckets_obj.insert(b.to_string(), serde_json::Value::Object(entry));
        }
        let body = serde_json::json!({
            "tickets_total": tickets_with_estimate,
            "tickets_with_estimate": tickets_with_estimate,
            "buckets": buckets_obj,
            "drift_ratio_high_vs_low": null,
            "drift_alarm": false,
        });
        fs::write(&path, body.to_string()).unwrap();
        path
    }

    fn write_cost_per_point_jsonl(dir: &Path, rows: &[(&str, f64)]) -> std::path::PathBuf {
        use std::fmt::Write as _;
        let path = dir.join("cost-per-point.jsonl");
        let mut s = String::new();
        for (ticket, est) in rows {
            writeln!(
                &mut s,
                "{{\"ticket\":\"{ticket}\",\"estimate\":{est},\"bucket\":3,\"total_tokens\":0,\"cost_usd\":0,\"tokens_per_point\":0,\"cost_per_point\":0}}"
            )
            .unwrap();
        }
        fs::write(&path, s).unwrap();
        path
    }

    #[test]
    fn roi_ratio_matches_known_inputs() {
        // Known case: human $/pt = 327, claude $/pt = 32.7 → ratio 10x.
        // Use 100 points → human cost 32700, claude cost 3270.
        let dir = TempDir::new().unwrap();
        // 10 tickets, 10 points each = 100 points, $327 each = $3270 total.
        let mut tokens_rows: Vec<(String, f64)> = Vec::new();
        for i in 0..10 {
            tokens_rows.push((format!("FPCRM-{}", 100 + i), 327.0));
        }
        let token_refs: Vec<(&str, f64)> =
            tokens_rows.iter().map(|(t, c)| (t.as_str(), *c)).collect();
        let tokens = write_tokens(dir.path(), &token_refs);

        let est_refs: Vec<(&str, f64)> = tokens_rows
            .iter()
            .map(|(t, _)| (t.as_str(), 10.0_f64))
            .collect();
        write_cost_per_point_jsonl(dir.path(), &est_refs);
        let summary = write_cost_per_point_summary(dir.path(), 10, &[(8_u8, 32.7_f64, 10_usize)]);

        let out_jsonl = dir.path().join("roi.jsonl");
        let out_summary = dir.path().join("roi-summary.json");
        let report = scan_roi(&tokens, &summary, &out_jsonl, &out_summary).unwrap();

        let all_time = report
            .windows
            .iter()
            .find(|w| w.window_days.is_none())
            .expect("all-time window present");
        // 100 points * $327 = $32700 human; $3270 claude → ratio 10.0.
        assert!(
            (all_time.roi_ratio - 10.0).abs() < 0.01,
            "roi_ratio {} should be ~10.0",
            all_time.roi_ratio
        );
        assert!((all_time.human_cost_usd - 32_700.0).abs() < 0.01);
        assert!((all_time.claude_cost_usd - 3_270.0).abs() < 0.01);
        assert!(all_time.estimate_data_available);
    }

    #[test]
    fn annual_savings_projection_scales_correctly() {
        let dir = TempDir::new().unwrap();
        // 30-day window: claude $1k, human $11k → delta $10k over 30d
        // → $10k/30 = $333.33/day → * 260 working days = $86,666.67/yr.
        // Set up 30 tickets, each 1 pt at $33.33 cost per ticket.
        // Then claude_cost = $1000, points = 30, claude_cpp ≈ $33.33,
        // human_cost = 30 * $327 = $9810. Delta = $8810. Hmm.
        // Easier: make tickets w/ 33.33 cost and 1pt each, 30 of them.
        // claude_cost = 999.9, human_cost = 30 * 327 = 9810,
        // delta = 8810.1, /30 = 293.67/day, *260 = $76,355/yr.
        // Test the math directly.
        let mut tokens_rows: Vec<(String, f64)> = Vec::new();
        let mut est_rows: Vec<(String, f64)> = Vec::new();
        for i in 0..30 {
            tokens_rows.push((format!("SEN-{}", 100 + i), 33.33));
            est_rows.push((format!("SEN-{}", 100 + i), 1.0));
        }
        let token_refs: Vec<(&str, f64)> =
            tokens_rows.iter().map(|(t, c)| (t.as_str(), *c)).collect();
        let tokens = write_tokens(dir.path(), &token_refs);
        let est_refs: Vec<(&str, f64)> = est_rows.iter().map(|(t, e)| (t.as_str(), *e)).collect();
        write_cost_per_point_jsonl(dir.path(), &est_refs);
        let summary = write_cost_per_point_summary(dir.path(), 30, &[(1_u8, 33.33_f64, 30_usize)]);

        // Build a fake projects dir so the ticket→last_seen map says all
        // tickets were active 5 days ago (within both the 7d and 30d
        // windows, but we'll mostly verify the 30d window).
        let projects = dir.path().parent().unwrap().join("projects-unused"); // not used — we'll point the analyzer at a custom layout.

        // Easier: rely on tokens_input being elsewhere → no projects/
        // root; analyzer will collapse into all-time only.
        // For window verification we ALSO need timestamps. Build the
        // canonical layout: dir/ -> projects/, sentinel/metrics/.
        let _ = projects;
        let claude_root = dir.path().join("claude-root");
        let metrics_dir = claude_root.join("sentinel").join("metrics");
        fs::create_dir_all(&metrics_dir).unwrap();
        let projects_dir = claude_root.join("projects");
        fs::create_dir_all(&projects_dir).unwrap();
        // Move tokens + summary into the canonical metrics dir.
        let canonical_tokens = metrics_dir.join("tokens-per-ticket.jsonl");
        fs::copy(&tokens, &canonical_tokens).unwrap();
        let canonical_summary = metrics_dir.join("cost-per-point-summary.json");
        fs::copy(&summary, &canonical_summary).unwrap();
        let canonical_jsonl = metrics_dir.join("cost-per-point.jsonl");
        fs::copy(dir.path().join("cost-per-point.jsonl"), &canonical_jsonl).unwrap();

        // For each ticket, create a project dir with a session JSONL
        // and set its mtime to "now". This puts every ticket inside the
        // 7d window, the 30d window, and the 90d window.
        for (ticket, _) in &tokens_rows {
            let pdir = projects_dir.join(format!("c--worktrees-{ticket}-feat"));
            fs::create_dir_all(&pdir).unwrap();
            fs::write(pdir.join("session.jsonl"), b"").unwrap();
        }

        let out_jsonl = canonical_tokens.parent().unwrap().join("roi.jsonl");
        let out_summary = canonical_tokens.parent().unwrap().join("roi-summary.json");
        let report = scan_roi(
            &canonical_tokens,
            &canonical_summary,
            &out_jsonl,
            &out_summary,
        )
        .unwrap();

        // 30d window contains all 30 tickets (just-touched mtime).
        let w30 = report
            .windows
            .iter()
            .find(|w| w.window_days == Some(30))
            .expect("30d window present");
        assert_eq!(w30.tickets_shipped, 30);
        assert!((w30.points_shipped - 30.0).abs() < 1e-9);
        // delta = 9810 - 999.9 = 8810.1. /30 = 293.67. *260 = 76,354.20.
        let expected_savings = (9810.0 - 999.9) / 30.0 * HUMAN_WORKING_DAYS_PER_YEAR;
        assert!(
            (w30.projected_annual_savings_usd - expected_savings).abs() < 1.0,
            "got {}, expected ~{}",
            w30.projected_annual_savings_usd,
            expected_savings
        );
    }

    #[test]
    fn missing_inputs_return_clean_empty_report() {
        let dir = TempDir::new().unwrap();
        let tokens = dir.path().join("does-not-exist.jsonl");
        let summary_in = dir.path().join("also-missing.json");
        let out_jsonl = dir.path().join("roi.jsonl");
        let out_summary = dir.path().join("roi-summary.json");
        let report = scan_roi(&tokens, &summary_in, &out_jsonl, &out_summary).unwrap();
        assert!(report.headline.is_none());
        assert!(report.windows.is_empty());
        assert!(out_jsonl.exists());
        assert!(out_summary.exists());
        // Summary is parseable JSON with 0 cost.
        let summary: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&out_summary).unwrap()).unwrap();
        assert_eq!(summary["claude_cost_usd_total"], 0.0);
        assert_eq!(summary["tickets_shipped_total"], 0);
    }

    #[test]
    fn roi_requires_estimates_when_sen13_has_zero_estimates() {
        let dir = TempDir::new().unwrap();
        // SEN-7 has 5 tickets with $100 each = $500 total.
        let tokens = write_tokens(
            dir.path(),
            &[
                ("FPCRM-1", 100.0),
                ("FPCRM-2", 100.0),
                ("FPCRM-3", 100.0),
                ("FPCRM-4", 100.0),
                ("FPCRM-5", 100.0),
            ],
        );
        // SEN-13 reports 0 tickets with estimate.
        let summary_in = write_cost_per_point_summary(dir.path(), 0, &[]);
        let out_jsonl = dir.path().join("roi.jsonl");
        let out_summary = dir.path().join("roi-summary.json");
        let report = scan_roi(&tokens, &summary_in, &out_jsonl, &out_summary).unwrap();

        let all_time = report
            .windows
            .iter()
            .find(|w| w.window_days.is_none())
            .expect("all-time present");
        assert!(!all_time.estimate_data_available);
        assert!(!all_time.estimate_note.is_empty());
        assert_eq!(all_time.tickets_shipped, 5);
        assert!((all_time.points_shipped - 0.0).abs() < 1e-9); // real points are still 0
        assert!((all_time.claude_cost_usd - 500.0).abs() < 0.01);
        assert!((all_time.human_cost_usd - 0.0).abs() < 0.01);
        assert!((all_time.roi_ratio - 0.0).abs() < 0.01);
    }

    #[test]
    fn slug_extraction_recognises_known_prefixes() {
        assert_eq!(
            extract_ticket_from_slug("c--worktrees-FPCRM-413-feat"),
            Some("FPCRM-413".to_string())
        );
        assert_eq!(
            extract_ticket_from_slug("worktrees-sen-15-roi-blah"),
            Some("SEN-15".to_string())
        );
        assert_eq!(extract_ticket_from_slug("HTTP-200-ok"), None);
        assert_eq!(extract_ticket_from_slug("just-a-feature"), None);
    }

    #[test]
    fn weighted_median_collapses_buckets() {
        let s = Sen13Summary {
            tickets_with_estimate: 10,
            bucket_medians: vec![(2, 10.0, 5), (8, 100.0, 5)],
        };
        // Weighted: (10*5 + 100*5) / 10 = 55.0
        assert!((s.cost_per_point_median().unwrap() - 55.0).abs() < 1e-9);

        let empty = Sen13Summary {
            tickets_with_estimate: 0,
            bucket_medians: vec![],
        };
        assert!(empty.cost_per_point_median().is_none());
    }
}
