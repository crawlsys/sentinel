//! Pure aggregation + rendering for the telemetry lake report (LEG-258).
//!
//! Deliberately **IO-free**: [`aggregate`] turns a slice of ledger rows into a
//! [`LakeReport`], and [`render_html`] / [`render_json`] / [`render_table`]
//! turn that into output. Reading the rows from R2 is the caller's job
//! ([`super::lake`]). Keeping this side pure means a future Cloudflare
//! read-side Worker (bound to the bucket) can reuse the exact same aggregation
//! shape without dragging in an HTTP/S3 client.
//!
//! Headline metrics the operator asked for: **last updated** (max row `ts` in
//! the window) and **unique clients reporting in** (distinct `client_id`).
//! Rows predating the `client_id` field carry [`UNKNOWN_CLIENT_ID`]; their
//! reporters are estimated separately as distinct sessions so the headline
//! number stays honest rather than silently conflating old + new data.

use chrono::{DateTime, Utc};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use crate::hook_metrics::{HookInvocation, UNKNOWN_CLIENT_ID};

/// A `(key, count)` row in a breakdown, sorted for stable rendering.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Count {
    pub key: String,
    pub count: usize,
}

/// Aggregated view of lake activity over a time window.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LakeReport {
    pub window_days: i64,
    /// When the report was generated (RFC3339).
    pub generated_at: String,
    /// Most recent row `ts` in the window (RFC3339), if any rows.
    pub last_updated: Option<String>,
    /// Human relative age of `last_updated` vs `generated_at` (e.g. `3m ago`).
    pub last_updated_age: Option<String>,
    pub total_rows: usize,
    /// Distinct non-`unknown` `client_id` — the headline "unique clients".
    pub unique_clients: usize,
    /// Distinct sessions among rows lacking a `client_id` (historical rows) —
    /// a labeled estimate, never folded into `unique_clients`.
    pub clients_estimated_from_sessions: usize,
    pub active_sessions: usize,
    pub catastrophic_intercepts: usize,
    pub by_harness: Vec<Count>,
    pub by_outcome: Vec<Count>,
    pub by_event: Vec<Count>,
    /// Distinct clients per harness (only counts identified clients).
    pub clients_per_harness: Vec<Count>,
    /// Row counts per UTC day, ascending by date.
    pub rows_per_day: Vec<Count>,
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Aggregate `rows` over the last `window_days` relative to `now`. Rows whose
/// `ts` parses and is older than the cutoff are dropped; rows with an
/// unparseable `ts` are kept (fail-open). Pure — no IO, no clock read.
#[must_use]
pub fn aggregate(rows: &[HookInvocation], window_days: i64, now: DateTime<Utc>) -> LakeReport {
    let cutoff = now - chrono::Duration::days(window_days);

    let mut clients: BTreeSet<&str> = BTreeSet::new();
    let mut sessions: BTreeSet<&str> = BTreeSet::new();
    let mut unknown_sessions: BTreeSet<&str> = BTreeSet::new();
    let mut harness_clients: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    let mut by_harness: HashMap<&str, usize> = HashMap::new();
    let mut by_outcome: HashMap<&str, usize> = HashMap::new();
    let mut by_event: HashMap<&str, usize> = HashMap::new();
    let mut per_day: HashMap<String, usize> = HashMap::new();
    let mut last: Option<DateTime<Utc>> = None;
    let mut total_rows = 0usize;
    let mut catastrophic = 0usize;

    for r in rows {
        let ts = parse_ts(&r.ts);
        if ts.is_some_and(|t| t < cutoff) {
            continue;
        }
        total_rows += 1;

        let identified = !r.client_id.is_empty() && r.client_id != UNKNOWN_CLIENT_ID;
        if identified {
            clients.insert(&r.client_id);
            harness_clients
                .entry(&r.source_harness)
                .or_default()
                .insert(&r.client_id);
        }
        if let Some(s) = r.session_id.as_deref() {
            sessions.insert(s);
            if !identified {
                unknown_sessions.insert(s);
            }
        }
        if r.outcome == "deny" && r.hook == "catastrophic_escalation" {
            catastrophic += 1;
        }
        *by_harness.entry(&r.source_harness).or_insert(0) += 1;
        *by_outcome.entry(&r.outcome).or_insert(0) += 1;
        *by_event.entry(&r.event).or_insert(0) += 1;
        if let Some(t) = ts {
            *per_day.entry(t.date_naive().to_string()).or_insert(0) += 1;
            last = Some(last.map_or(t, |l| l.max(t)));
        }
    }

    let clients_per_harness = harness_clients
        .into_iter()
        .map(|(k, set)| Count {
            key: k.to_string(),
            count: set.len(),
        })
        .collect();

    LakeReport {
        window_days,
        generated_at: now.to_rfc3339(),
        last_updated: last.map(|t| t.to_rfc3339()),
        last_updated_age: last.map(|t| humanize_age(now - t)),
        total_rows,
        unique_clients: clients.len(),
        clients_estimated_from_sessions: unknown_sessions.len(),
        active_sessions: sessions.len(),
        catastrophic_intercepts: catastrophic,
        by_harness: top(by_harness),
        by_outcome: top(by_outcome),
        by_event: top(by_event),
        clients_per_harness: sort_counts(clients_per_harness),
        rows_per_day: per_day_sorted(per_day),
    }
}

/// Counts sorted by count desc, then key asc — stable for rendering + tests.
fn top(map: HashMap<&str, usize>) -> Vec<Count> {
    sort_counts(
        map.into_iter()
            .map(|(key, count)| Count {
                key: key.to_string(),
                count,
            })
            .collect(),
    )
}

fn sort_counts(mut v: Vec<Count>) -> Vec<Count> {
    v.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
    v
}

fn per_day_sorted(map: HashMap<String, usize>) -> Vec<Count> {
    let mut v: Vec<Count> = map
        .into_iter()
        .map(|(key, count)| Count { key, count })
        .collect();
    v.sort_by(|a, b| a.key.cmp(&b.key));
    v
}

fn humanize_age(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

// --- rendering --------------------------------------------------------------

/// Pretty JSON of the full report.
#[must_use]
pub fn render_json(report: &LakeReport) -> String {
    serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_string())
}

/// Compact plain-text summary for a terminal.
#[must_use]
pub fn render_table(report: &LakeReport) -> String {
    let mut out = String::new();
    let age = report.last_updated_age.as_deref().unwrap_or("");
    let last = report
        .last_updated
        .as_deref()
        .map_or_else(|| "never".to_string(), |t| format!("{t} ({age})"));
    let _ = writeln!(out, "Telemetry lake — last {} day(s)", report.window_days);
    let _ = writeln!(out, "  Last updated:    {last}");
    let _ = writeln!(out, "  Unique clients:  {}", report.unique_clients);
    if report.clients_estimated_from_sessions > 0 {
        let _ = writeln!(
            out,
            "    (+~{} estimated from pre-client_id rows, by session)",
            report.clients_estimated_from_sessions
        );
    }
    let _ = writeln!(out, "  Active sessions: {}", report.active_sessions);
    let _ = writeln!(out, "  Total rows:      {}", report.total_rows);
    let _ = writeln!(
        out,
        "  Catastrophic intercepts: {}",
        report.catastrophic_intercepts
    );
    let mut section = |title: &str, rows: &[Count]| {
        let _ = writeln!(out, "  {title}:");
        for c in rows {
            let _ = writeln!(out, "    {:<28} {}", c.key, c.count);
        }
    };
    section("By harness", &report.by_harness);
    section("Clients per harness", &report.clients_per_harness);
    section("By outcome", &report.by_outcome);
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn html_rows(rows: &[Count]) -> String {
    if rows.is_empty() {
        return "<tr><td colspan=2 class=dim>—</td></tr>".to_string();
    }
    rows.iter().fold(String::new(), |mut acc, c| {
        let _ = write!(
            acc,
            "<tr><td>{}</td><td class=num>{}</td></tr>",
            html_escape(&c.key),
            c.count
        );
        acc
    })
}

/// Self-contained one-shot HTML report (no external assets). "Last updated"
/// and "Unique clients" are the hero numbers; everything else is a breakdown.
/// Styling mirrors the lightweight dark look of `api/ccam_dashboard.html`.
#[must_use]
pub fn render_html(report: &LakeReport) -> String {
    let last = report
        .last_updated
        .as_deref()
        .map_or_else(|| "never".to_string(), html_escape);
    let age = report
        .last_updated_age
        .as_deref()
        .map_or(String::new(), |a| format!(" · {}", html_escape(a)));
    let estimate_note = if report.clients_estimated_from_sessions > 0 {
        format!(
            "<div class=note>+~{} more estimated from rows predating <code>client_id</code> (counted by distinct session)</div>",
            report.clients_estimated_from_sessions
        )
    } else {
        String::new()
    };

    format!(
        r#"<!doctype html><html><head><meta charset=utf-8>
<title>Sentinel Telemetry Lake</title>
<style>
:root{{color-scheme:dark}}
body{{font:14px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;background:#0d1117;color:#c9d1d9;margin:0;padding:32px;max-width:980px;margin:auto}}
h1{{font-size:18px;margin:0 0 4px}}
.sub{{color:#8b949e;margin-bottom:24px}}
.hero{{display:flex;gap:16px;flex-wrap:wrap;margin-bottom:24px}}
.card{{background:#161b22;border:1px solid #30363d;border-radius:10px;padding:16px 20px;flex:1;min-width:200px}}
.card .label{{color:#8b949e;font-size:12px;text-transform:uppercase;letter-spacing:.05em}}
.card .big{{font-size:30px;font-weight:600;color:#58a6ff;margin-top:6px}}
.card .big.green{{color:#3fb950}}
.card .big.amber{{color:#d29922}}
.note{{color:#8b949e;font-size:12px;margin-top:6px}}
.grid{{display:grid;grid-template-columns:1fr 1fr;gap:16px}}
table{{width:100%;border-collapse:collapse;background:#161b22;border:1px solid #30363d;border-radius:10px;overflow:hidden}}
caption{{text-align:left;color:#8b949e;font-size:12px;text-transform:uppercase;letter-spacing:.05em;padding:10px 12px 4px}}
td{{padding:6px 12px;border-top:1px solid #21262d}}
td.num{{text-align:right;color:#79c0ff}}
td.dim{{color:#8b949e}}
code{{background:#21262d;padding:1px 5px;border-radius:4px}}
</style></head><body>
<h1>Sentinel Telemetry Lake</h1>
<div class=sub>Window: last {window} day(s) · generated {generated}</div>
<div class=hero>
  <div class=card><div class=label>Last updated</div><div class="big green">{last}</div><div class=note>{age}</div></div>
  <div class=card><div class=label>Unique clients (window)</div><div class=big>{clients}</div>{estimate_note}</div>
  <div class=card><div class=label>Active sessions</div><div class=big>{sessions}</div></div>
  <div class=card><div class=label>Total rows</div><div class=big>{rows}</div></div>
  <div class=card><div class=label>Catastrophic intercepts</div><div class="big amber">{cata}</div></div>
</div>
<div class=grid>
  <table><caption>Clients per harness</caption>{clients_h}</table>
  <table><caption>Rows by harness</caption>{by_h}</table>
  <table><caption>Outcomes</caption>{by_o}</table>
  <table><caption>Events</caption>{by_e}</table>
  <table><caption>Rows per day</caption>{per_day}</table>
</div>
</body></html>"#,
        window = report.window_days,
        generated = html_escape(&report.generated_at),
        last = last,
        age = age.trim_start_matches(" · "),
        clients = report.unique_clients,
        estimate_note = estimate_note,
        sessions = report.active_sessions,
        rows = report.total_rows,
        cata = report.catastrophic_intercepts,
        clients_h = html_rows(&report.clients_per_harness),
        by_h = html_rows(&report.by_harness),
        by_o = html_rows(&report.by_outcome),
        by_e = html_rows(&report.by_event),
        per_day = html_rows(&report.rows_per_day),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        ts: &str,
        harness: &str,
        client: &str,
        session: &str,
        hook: &str,
        outcome: &str,
    ) -> HookInvocation {
        HookInvocation {
            ts: ts.to_string(),
            event: "PreToolUse".to_string(),
            hook: hook.to_string(),
            tool: None,
            session_id: Some(session.to_string()),
            repo_root: None,
            duration_us: 1,
            outcome: outcome.to_string(),
            reason: None,
            source_harness: harness.to_string(),
            client_id: client.to_string(),
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-22T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn counts_distinct_clients_not_sessions() {
        let rows = vec![
            row(
                "2026-06-22T10:00:00Z",
                "claude",
                "m-a",
                "s1",
                "phase_gate",
                "allow",
            ),
            row(
                "2026-06-22T10:01:00Z",
                "claude",
                "m-a",
                "s2",
                "phase_gate",
                "allow",
            ), // same client, new session
            row(
                "2026-06-22T10:02:00Z",
                "codex",
                "m-b",
                "s3",
                "phase_gate",
                "allow",
            ),
        ];
        let r = aggregate(&rows, 7, now());
        assert_eq!(r.unique_clients, 2); // m-a, m-b — NOT 3 sessions
        assert_eq!(r.active_sessions, 3);
        assert_eq!(r.clients_estimated_from_sessions, 0);
    }

    #[test]
    fn historical_rows_fall_back_to_session_estimate() {
        let rows = vec![
            row(
                "2026-06-22T10:00:00Z",
                "claude",
                UNKNOWN_CLIENT_ID,
                "s1",
                "phase_gate",
                "allow",
            ),
            row(
                "2026-06-22T10:01:00Z",
                "claude",
                UNKNOWN_CLIENT_ID,
                "s2",
                "phase_gate",
                "allow",
            ),
            row(
                "2026-06-22T10:02:00Z",
                "claude",
                "m-new",
                "s3",
                "phase_gate",
                "allow",
            ),
        ];
        let r = aggregate(&rows, 7, now());
        assert_eq!(r.unique_clients, 1); // only m-new is identified
        assert_eq!(r.clients_estimated_from_sessions, 2); // s1, s2 (unattributed)
    }

    #[test]
    fn last_updated_is_max_ts_in_window() {
        let rows = vec![
            row(
                "2026-06-22T09:00:00Z",
                "claude",
                "m-a",
                "s1",
                "phase_gate",
                "allow",
            ),
            row(
                "2026-06-22T11:30:00Z",
                "claude",
                "m-a",
                "s1",
                "phase_gate",
                "allow",
            ),
        ];
        let r = aggregate(&rows, 7, now());
        assert_eq!(r.last_updated.as_deref(), Some("2026-06-22T11:30:00+00:00"));
        assert_eq!(r.last_updated_age.as_deref(), Some("30m ago"));
    }

    #[test]
    fn drops_rows_outside_window() {
        let rows = vec![
            row(
                "2026-06-01T10:00:00Z",
                "claude",
                "m-old",
                "s0",
                "phase_gate",
                "allow",
            ), // 21d ago
            row(
                "2026-06-22T10:00:00Z",
                "claude",
                "m-a",
                "s1",
                "phase_gate",
                "allow",
            ),
        ];
        let r = aggregate(&rows, 7, now());
        assert_eq!(r.total_rows, 1);
        assert_eq!(r.unique_clients, 1);
    }

    #[test]
    fn counts_catastrophic_intercepts() {
        let rows = vec![
            row(
                "2026-06-22T10:00:00Z",
                "claude",
                "m-a",
                "s1",
                "catastrophic_escalation",
                "deny",
            ),
            row(
                "2026-06-22T10:01:00Z",
                "claude",
                "m-a",
                "s1",
                "catastrophic_escalation",
                "allow",
            ), // not a deny
            row(
                "2026-06-22T10:02:00Z",
                "claude",
                "m-a",
                "s1",
                "git_hygiene",
                "deny",
            ), // wrong hook
        ];
        let r = aggregate(&rows, 7, now());
        assert_eq!(r.catastrophic_intercepts, 1);
    }

    #[test]
    fn empty_lake_renders_without_panic() {
        let r = aggregate(&[], 7, now());
        assert_eq!(r.unique_clients, 0);
        assert_eq!(r.last_updated, None);
        let html = render_html(&r);
        assert!(html.contains("Unique clients"));
        assert!(html.contains("never"));
        let _ = render_json(&r);
        let _ = render_table(&r);
    }

    #[test]
    fn html_contains_hero_numbers() {
        let rows = vec![row(
            "2026-06-22T10:00:00Z",
            "claude",
            "m-a",
            "s1",
            "phase_gate",
            "allow",
        )];
        let r = aggregate(&rows, 7, now());
        let html = render_html(&r);
        assert!(html.contains(">1<")); // unique clients = 1 rendered
        assert!(html.contains("claude"));
    }
}
