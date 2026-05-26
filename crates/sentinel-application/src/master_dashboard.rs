//! Master Enterprise Factory Dashboard — SEN-19.
//!
//! Single integration point that reads every shipped sentinel summary
//! file and combines them into one rendered structure
//! (`master-dashboard-summary.json`) that the Next.js dashboard's
//! master organism can pull in one fetch. The point is to amortise the
//! per-file IO cost across organisms and provide a frozen "as-of"
//! snapshot the UI can render deterministically.
//!
//! Reads (all optional — missing files surface as `None`):
//!
//! | Summary file | Producer |
//! |--------------|----------|
//! | `deploys-summary.json` | SEN-9 |
//! | `lead-times-summary.json` | SEN-10 |
//! | `change-failure-summary.json` | SEN-11 |
//! | `stage-thresholds-summary.json` | SEN-2 |
//! | `per-stage-cycle-time-summary.json` | SEN-17 |
//! | `throughput-summary.json` | SEN-5 |
//! | `first-time-pass-summary.json` | SEN-16 |
//! | `cost-per-point-summary.json` | SEN-13 |
//! | `roi-summary.json` | SEN-15 |
//! | `cache-efficiency-summary.json` | SEN-14 |
//! | `pr-review-summary.json` | SEN-18 |
//!
//! Writes: `master-dashboard-summary.json`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::Path;

use crate::change_failure::ChangeFailureSummary;
use crate::cycle_time_analytics::{PerStageBreakdownSummary, StageThresholdSummary};
use crate::deploy_freq::DeploySummary;
use crate::lead_time::LeadTimeSummary;
use crate::throughput::{FtpSummary, ThroughputSummary};

/// One section's load result. `Some` carries the parsed summary; `None`
/// means the source file was absent or unparseable — the dashboard
/// renders that section as "no data".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section<T> {
    pub source: String,
    pub loaded: bool,
    pub summary: Option<T>,
}

impl<T> Section<T> {
    fn empty(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            loaded: false,
            summary: None,
        }
    }
}

/// Pass-through wrapper for summaries that this module doesn't have a
/// strong type for (SEN-13, SEN-14, SEN-15, SEN-18). The dashboard's TS
/// types deserialize them directly from the embedded `Value`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawSection {
    pub source: String,
    pub loaded: bool,
    pub raw: Option<Value>,
}

impl RawSection {
    fn empty(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            loaded: false,
            raw: None,
        }
    }
}

/// Top-level rendered structure written to `master-dashboard-summary.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterDashboardSummary {
    pub generated_at: String,
    pub sections_loaded: u8,
    pub sections_total: u8,

    // DORA — strongly-typed sections we own the schema for.
    pub deploys: Section<DeploySummary>,
    pub lead_times: Section<LeadTimeSummary>,
    pub change_failure: Section<ChangeFailureSummary>,

    // Cycle-time analytics — also strongly typed.
    pub stage_thresholds: Section<StageThresholdSummary>,
    pub per_stage_breakdown: Section<PerStageBreakdownSummary>,
    pub throughput: Section<ThroughputSummary>,
    pub first_time_pass: Section<FtpSummary>,

    // Economics + review — passed through as JSON values since the
    // dashboard owns those schemas.
    pub cost_per_point: RawSection,
    pub roi: RawSection,
    pub cache_efficiency: RawSection,
    pub pr_review: RawSection,
}

/// Build the master summary by reading every per-metric file in
/// `metrics_dir`. `now` is injected for test determinism.
///
/// # Errors
/// Returns the write error for the output file. Individual section
/// load failures are absorbed (the section just reports `loaded=false`).
pub fn build_at(
    metrics_dir: &Path,
    summary_path: &Path,
    now: DateTime<Utc>,
) -> Result<MasterDashboardSummary> {
    let deploys = load_section::<DeploySummary>(metrics_dir, "deploys-summary.json");
    let lead_times = load_section::<LeadTimeSummary>(metrics_dir, "lead-times-summary.json");
    let change_failure =
        load_section::<ChangeFailureSummary>(metrics_dir, "change-failure-summary.json");
    let stage_thresholds =
        load_section::<StageThresholdSummary>(metrics_dir, "stage-thresholds-summary.json");
    let per_stage_breakdown =
        load_section::<PerStageBreakdownSummary>(metrics_dir, "per-stage-cycle-time-summary.json");
    let throughput = load_section::<ThroughputSummary>(metrics_dir, "throughput-summary.json");
    let first_time_pass = load_section::<FtpSummary>(metrics_dir, "first-time-pass-summary.json");

    let cost_per_point = load_raw(metrics_dir, "cost-per-point-summary.json");
    let roi = load_raw(metrics_dir, "roi-summary.json");
    let cache_efficiency = load_raw(metrics_dir, "cache-efficiency-summary.json");
    let pr_review = load_raw(metrics_dir, "pr-review-summary.json");

    let loaded_count = u8::from(deploys.loaded)
        + u8::from(lead_times.loaded)
        + u8::from(change_failure.loaded)
        + u8::from(stage_thresholds.loaded)
        + u8::from(per_stage_breakdown.loaded)
        + u8::from(throughput.loaded)
        + u8::from(first_time_pass.loaded)
        + u8::from(cost_per_point.loaded)
        + u8::from(roi.loaded)
        + u8::from(cache_efficiency.loaded)
        + u8::from(pr_review.loaded);

    let summary = MasterDashboardSummary {
        generated_at: now.to_rfc3339(),
        sections_loaded: loaded_count,
        sections_total: 11,
        deploys,
        lead_times,
        change_failure,
        stage_thresholds,
        per_stage_breakdown,
        throughput,
        first_time_pass,
        cost_per_point,
        roi,
        cache_efficiency,
        pr_review,
    };

    if let Some(parent) = summary_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&summary).context("serialize master summary")?;
    fs::write(summary_path, json).with_context(|| format!("write {}", summary_path.display()))?;
    Ok(summary)
}

/// `Utc::now()` wrapper around [`build_at`].
///
/// # Errors
/// Same as [`build_at`].
pub fn build(metrics_dir: &Path, summary_path: &Path) -> Result<MasterDashboardSummary> {
    build_at(metrics_dir, summary_path, Utc::now())
}

/// Strongly-typed section loader. Missing file / parse error → empty.
fn load_section<T: serde::de::DeserializeOwned>(dir: &Path, name: &str) -> Section<T> {
    let path = dir.join(name);
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Section::empty(name),
    };
    match serde_json::from_str::<T>(&raw) {
        Ok(s) => Section {
            source: name.to_string(),
            loaded: true,
            summary: Some(s),
        },
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, "master_dashboard: parse failure; skipping");
            Section::empty(name)
        }
    }
}

/// JSON-pass-through loader. Missing file / parse error → empty.
fn load_raw(dir: &Path, name: &str) -> RawSection {
    let path = dir.join(name);
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return RawSection::empty(name),
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => RawSection {
            source: name.to_string(),
            loaded: true,
            raw: Some(v),
        },
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, "master_dashboard: raw parse failure; skipping");
            RawSection::empty(name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::throughput::TeamThroughput;
    use chrono::TimeZone;

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap()
    }

    #[test]
    fn build_empty_metrics_dir_produces_all_unloaded_sections() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("master.json");
        let s = build_at(tmp.path(), &out, fixed_now()).unwrap();
        assert_eq!(s.sections_loaded, 0);
        assert_eq!(s.sections_total, 11);
        assert!(!s.deploys.loaded);
        assert!(!s.lead_times.loaded);
        assert!(!s.change_failure.loaded);
        assert!(!s.throughput.loaded);
        assert!(!s.first_time_pass.loaded);
        assert!(!s.cost_per_point.loaded);
        // Output file written.
        assert!(out.exists());
    }

    #[test]
    fn build_loads_strongly_typed_section_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let throughput = ThroughputSummary {
            generated_at: fixed_now().to_rfc3339(),
            window_days: 30,
            events_scanned: 10,
            completions_scanned: 5,
            per_team: vec![TeamThroughput {
                team: Some("FPCRM".to_string()),
                completed_30d: 5,
                completed_7d: 1,
                completed_per_week: 1.166,
                completed_per_month: 5.0,
            }],
            daily_points: vec![],
        };
        let throughput_path = tmp.path().join("throughput-summary.json");
        fs::write(
            &throughput_path,
            serde_json::to_string(&throughput).unwrap(),
        )
        .unwrap();
        let out = tmp.path().join("master.json");
        let s = build_at(tmp.path(), &out, fixed_now()).unwrap();
        assert_eq!(s.sections_loaded, 1);
        assert!(s.throughput.loaded);
        let loaded = s.throughput.summary.unwrap();
        assert_eq!(loaded.completions_scanned, 5);
    }

    #[test]
    fn build_loads_raw_section_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let roi_path = tmp.path().join("roi-summary.json");
        fs::write(&roi_path, r#"{"window_days":30,"roi_ratio":3.5}"#).unwrap();
        let out = tmp.path().join("master.json");
        let s = build_at(tmp.path(), &out, fixed_now()).unwrap();
        assert!(s.roi.loaded);
        assert_eq!(s.roi.raw.unwrap()["roi_ratio"].as_f64().unwrap(), 3.5);
    }

    #[test]
    fn build_handles_malformed_json_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let bad_path = tmp.path().join("roi-summary.json");
        fs::write(&bad_path, "{not-json").unwrap();
        let out = tmp.path().join("master.json");
        let s = build_at(tmp.path(), &out, fixed_now()).unwrap();
        // Malformed file → section not loaded; build still succeeds.
        assert!(!s.roi.loaded);
        assert_eq!(s.sections_loaded, 0);
    }

    #[test]
    fn build_counts_loaded_sections_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        // Three raw sections present.
        fs::write(tmp.path().join("roi-summary.json"), "{}").unwrap();
        fs::write(tmp.path().join("cost-per-point-summary.json"), "{}").unwrap();
        fs::write(tmp.path().join("pr-review-summary.json"), "{}").unwrap();
        let out = tmp.path().join("master.json");
        let s = build_at(tmp.path(), &out, fixed_now()).unwrap();
        assert_eq!(s.sections_loaded, 3);
    }

    #[test]
    fn build_writes_summary_with_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("master.json");
        let now = fixed_now();
        let s = build_at(tmp.path(), &out, now).unwrap();
        assert_eq!(s.generated_at, now.to_rfc3339());
        // And the file round-trips through deserialization.
        let raw = fs::read_to_string(&out).unwrap();
        let reread: MasterDashboardSummary = serde_json::from_str(&raw).unwrap();
        assert_eq!(reread.generated_at, now.to_rfc3339());
        assert_eq!(reread.sections_total, 11);
    }
}
