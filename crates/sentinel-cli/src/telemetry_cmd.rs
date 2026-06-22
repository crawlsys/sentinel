//! `sentinel telemetry collect|ship` — the LEG-258 R2 telemetry pipeline.
//!
//! `collect` (LEG-259) tails the per-harness hook-invocation ledgers
//! (claude / codex / opencode) by `(dev, inode) + offset` checkpoint and
//! stages zstd NDJSON batches in `~/.claude/sentinel/telemetry/spool/`.
//! `ship` (LEG-260) drains the spool to R2 via signed S3-compatible PUTs
//! with idempotent content-hash keys, deleting spool files only after a
//! confirmed PUT. Both are one-shot and crash-safe. See
//! `sentinel-application::telemetry`.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::PathBuf;

use sentinel_application::telemetry::ledger::{
    collect_sources, default_ledger_sources, TelemetrySource,
};
use sentinel_application::telemetry::ship::{
    dry_run_report, ship_spool, ShipConfig, ShipTransport,
};
use sentinel_application::telemetry::snapshot::{
    default_kpi_sources, ldesk_db_path, SessionIssueSource, UsageRollupSource,
};
use sentinel_application::telemetry::spool::{self, SpoolConfig};
use sentinel_application::telemetry::{lake, report};

/// Telemetry state root: `<claude_dir>/sentinel/telemetry` (honors
/// `SENTINEL_CLAUDE_DIR` for isolated profiles/tests).
fn telemetry_dir() -> PathBuf {
    sentinel_application::paths::claude_dir()
        .join("sentinel")
        .join("telemetry")
}

pub fn run_collect() -> Result<()> {
    let dir = telemetry_dir();
    let checkpoint_path = dir.join("checkpoint.json");
    let spool_cfg = SpoolConfig::new(dir.join("spool"));
    let ledgers = default_ledger_sources();
    let kpis = default_kpi_sources();
    let db_path = ldesk_db_path();
    let usage_rollup = UsageRollupSource::new(db_path.clone());
    let session_issue = SessionIssueSource::new(db_path.clone());

    println!(
        "{}",
        "Sentinel Telemetry Collect (LEG-259 + LEG-261)".bold()
    );
    println!("Checkpoint:  {}", checkpoint_path.display());
    println!("Spool:       {}", spool_cfg.dir.display());
    for s in &ledgers {
        println!("Ledger [{}]: {}", s.name, s.live_path.display());
    }
    for s in &kpis {
        println!("KPI [{}]: {}", s.scan, s.path.display());
    }
    println!("ldesk db:    {} (read-only)", db_path.display());
    println!();

    // Ledgers and snapshots run in one pass against the shared checkpoint.
    let mut refs: Vec<&dyn TelemetrySource> =
        ledgers.iter().map(|s| s as &dyn TelemetrySource).collect();
    refs.extend(kpis.iter().map(|s| s as &dyn TelemetrySource));
    refs.push(&usage_rollup);
    refs.push(&session_issue);
    let report = collect_sources(&refs, &checkpoint_path, &spool_cfg)?;

    let mut total_rows = 0u64;
    let mut total_batches = 0u64;
    for (name, stats) in &report {
        total_rows += stats.rows;
        total_batches += stats.batches;
        println!(
            "  {:<10} {} rows → {} batches ({} bytes spooled, {} file(s) drained)",
            name.bold(),
            stats.rows.to_string().cyan(),
            stats.batches,
            stats.spooled_bytes,
            stats.files_drained,
        );
        for note in &stats.notes {
            println!("    {} {}", "note:".yellow(), note.yellow());
        }
    }

    let spool_depth = spool::list_manifests(&spool_cfg.dir)?.len();
    let spool_bytes = spool::dir_usage_bytes(&spool_cfg.dir);
    println!();
    println!(
        "Collected {} rows into {} batches. Spool now holds {} batch(es), {} bytes.",
        total_rows.to_string().green().bold(),
        total_batches,
        spool_depth,
        spool_bytes,
    );
    if spool_depth > 0 {
        println!(
            "Run {} to drain the spool to R2.",
            "sentinel telemetry ship".bold()
        );
    }
    Ok(())
}

pub async fn run_ship(dry_run: bool) -> Result<()> {
    let spool_dir = telemetry_dir().join("spool");
    println!("{}", "Sentinel Telemetry Ship (LEG-260)".bold());
    println!("Spool:       {}", spool_dir.display());

    if dry_run {
        let entries = dry_run_report(&spool_dir)?;
        println!(
            "Mode:        {} (no network, no deletions)\n",
            "dry-run".yellow()
        );
        if entries.is_empty() {
            println!("{}", "Spool is empty — nothing to ship.".green());
            return Ok(());
        }
        let mut bytes = 0u64;
        for e in &entries {
            bytes += e.compressed_bytes;
            println!(
                "  {} ({} rows, {} bytes)",
                e.object_key.cyan(),
                e.rows,
                e.compressed_bytes
            );
        }
        println!(
            "\nWould ship {} batch(es), {} bytes.",
            entries.len().to_string().bold(),
            bytes
        );
        return Ok(());
    }

    let transport = ShipTransport::from_env()?;
    match &transport {
        ShipTransport::Bearer(c) => {
            println!("Transport:   {}", "bearer worker".bold());
            println!("Ingest URL:  {}", c.ingest_url);
        }
        ShipTransport::SigV4(c) => {
            println!("Transport:   {}", "sigv4 (direct)".bold());
            println!("Endpoint:    {}", c.endpoint);
            println!("Bucket:      {}", c.bucket);
        }
    }
    println!();

    let report = ship_spool(&transport, &spool_dir).await?;
    println!(
        "Shipped {} batch(es) ({} bytes), {} already present (idempotent skip), {} failed.",
        report.shipped.to_string().green().bold(),
        report.bytes_shipped,
        report.skipped_existing,
        report.failed,
    );
    for err in &report.errors {
        println!("  {} {}", "error:".red(), err);
    }
    if report.failed > 0 {
        anyhow::bail!(
            "{} batch(es) failed to ship and remain spooled for the next run",
            report.failed
        );
    }
    Ok(())
}

/// Output format for `sentinel telemetry report`.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum ReportFormat {
    /// Self-contained HTML report written to a file (default).
    Html,
    /// Pretty JSON to stdout.
    Json,
    /// Plain-text summary to stdout.
    Table,
}

/// Parse `--window` into a day count: `24h` → 1, `7d` → 7, `30d` → 30, a bare
/// integer → days. Sub-day `h` values round up to a 1-day window.
fn parse_window_days(s: &str) -> Result<i64> {
    let s = s.trim();
    let value = if let Some(d) = s.strip_suffix('d') {
        d.parse::<i64>().ok()
    } else if let Some(h) = s.strip_suffix('h') {
        // ceil(hours / 24) for a whole-day window (signed div_ceil is unstable).
        h.parse::<i64>().ok().map(|n| (n + 23) / 24)
    } else {
        s.parse::<i64>().ok()
    };
    value
        .map(|n| n.max(1))
        .ok_or_else(|| anyhow::anyhow!("invalid --window '{s}'; use e.g. 24h, 7d, 30d"))
}

/// Read the R2 lake and emit a fleet-activity report (LEG-258). Reads via the
/// `R2_*` `SigV4` creds — the ingest Worker is write-only, so reporting needs
/// the `S3` read path (run under `doppler run -p legatus -c dev --`).
pub async fn run_report(
    window: &str,
    harness: Option<&str>,
    format: ReportFormat,
    out: Option<&str>,
) -> Result<()> {
    let window_days = parse_window_days(window)?;
    let cfg = ShipConfig::from_env().context(
        "the lake report reads R2 via R2_* creds (the ingest Worker is write-only) — \
         run under `doppler run --project legatus --config dev --`",
    )?;
    let now = chrono::Utc::now();

    eprintln!("{}", "Sentinel Telemetry Report (LEG-258)".bold());
    eprintln!("Endpoint:    {}", cfg.endpoint);
    eprintln!("Bucket:      {}", cfg.bucket);
    eprintln!(
        "Window:      last {window_days} day(s){}",
        harness.map_or_else(String::new, |h| format!(", harness={h}"))
    );

    let rows = lake::fetch_rows(&cfg, window_days, harness, now).await?;
    let rep = report::aggregate(&rows, window_days, now);

    match format {
        ReportFormat::Json => println!("{}", report::render_json(&rep)),
        ReportFormat::Table => println!("{}", report::render_table(&rep)),
        ReportFormat::Html => {
            let html = report::render_html(&rep);
            let path = out.map_or_else(|| telemetry_dir().join("lake-report.html"), PathBuf::from);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&path, html).with_context(|| format!("write {}", path.display()))?;
            println!("{} {}", "Wrote".green().bold(), path.display());
        }
    }
    eprintln!(
        "  Last updated:   {}",
        rep.last_updated.as_deref().unwrap_or("never")
    );
    eprintln!(
        "  Unique clients: {}",
        rep.unique_clients.to_string().cyan()
    );
    if rep.clients_estimated_from_sessions > 0 {
        eprintln!(
            "    (+~{} estimated from pre-client_id rows)",
            rep.clients_estimated_from_sessions
        );
    }
    Ok(())
}
