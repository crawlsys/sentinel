//! Linear → native-task INBOUND sync hook (the inbound half of the
//! Linear↔native-task two-way sync).
//!
//! Runs on `UserPromptSubmit`. Best-effort and **MUST fail open** — it never
//! blocks the user's prompt under any circumstance.
//!
//! ## What it does
//!
//! Rather than polling the Linear GraphQL API per-issue, this hook polls the
//! **Hookdeck Events API** for Linear webhook deliveries that Hookdeck has
//! *already captured* from Linear. Hookdeck is the inbound webhook gateway, so
//! every Linear `Issue.update` (state change, etc.) lands there as an event.
//! We read the newest events for the Linear source and reconcile in-progress
//! native tasks against the state transitions Linear pushed.
//!
//! 1. Reads the active session's native tasks (same on-disk format as
//!    `task_persist`) and finds those tagged `@linear:{ID}` that are currently
//!    *in progress* (native status `in_progress` and/or the `🔄` emoji prefix).
//! 2. Polls the Hookdeck Events API for the Linear source
//!    ([`LINEAR_SOURCE_ID`]), newest-first, with the raw delivery body included.
//!    Only events *newer* than the persisted cursor are processed.
//! 3. Parses each event's raw Linear webhook body (reusing the field shape from
//!    [`crate::hooks::hookdeck_decoders::linear`]) to extract the issue
//!    identifier (e.g. `FIR-123`) and its new workflow-state name. The state
//!    name is mapped to a desired native-task status emoji:
//!    - In Progress / Started / In Review / Code Review → `🔄` (no drift)
//!    - Done / Completed / Merged / any QA state → suggest `✅`
//!    - Canceled / Duplicate → suggest `❌`
//! 4. Cross-references against the in-progress `@linear`-tagged tasks. When (and
//!    only when) there is actual drift (the Hookdeck event state differs from
//!    the task's current in-progress emoji), injects a concise context block
//!    telling the agent which tasks to `TaskUpdate`. The hook itself cannot call
//!    `TaskUpdate` — it injects an instruction, mirroring `task_completed.rs`.
//! 5. Advances the cursor past the newest processed event.
//!
//! ## Throttle
//!
//! Polls the Hookdeck API at most once per [`POLL_INTERVAL`] per session. A
//! timestamp marker is written to `~/.claude/sentinel/state/` **before** the
//! network call so a failed request still counts against the window. Within the
//! window the hook short-circuits to `allow()` without any network call.
//!
//! ## Key resolution
//!
//! `HOOKDECK_API_KEY` is resolved from the process env first, then — if absent —
//! by reading `~/.claude/sentinel/secrets.toml` directly and scanning each
//! section (`[account]` + `[global]`) for a `HOOKDECK_API_KEY`. The key lives in
//! `[global]`. No MCP server is involved.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::{EnvPort, FileSystemPort, HookContext};

/// Minimum interval between Hookdeck API polls, per session.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Hookdeck Events API endpoint (pinned API version `2024-09-01`).
const HOOKDECK_EVENTS_URL: &str = "https://api.hookdeck.com/2024-09-01/events";

/// The Hookdeck source id for Linear webhook deliveries.
const LINEAR_SOURCE_ID: &str = "src_ebtn42uit6926m";

/// Page size for the events poll.
const EVENTS_LIMIT: u32 = 50;

/// A task read from Claude Code's on-disk format. Mirrors the subset of fields
/// `task_persist::Task` uses that this hook needs.
#[derive(Debug, Clone, serde::Deserialize)]
struct Task {
    #[serde(default)]
    id: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    status: String,
}

/// A native task that carries a `@linear:{ID}` tag and is currently in
/// progress (status `in_progress` and/or a `🔄` emoji prefix on the subject).
#[derive(Debug, Clone)]
struct LinkedTask {
    task_id: String,
    subject: String,
    linear_id: String,
}

/// The desired native-task status implied by a Linear workflow state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesiredStatus {
    /// Linear says still in progress — `🔄`, no drift from an in-progress task.
    InProgress,
    /// Linear says done/completed (incl. QA-flavoured completed states) — `✅`.
    Completed,
    /// Linear says canceled — `❌`.
    Canceled,
}

impl DesiredStatus {
    fn emoji(self) -> &'static str {
        match self {
            Self::InProgress => "🔄",
            Self::Completed => "✅",
            Self::Canceled => "❌",
        }
    }

    /// Native task status string the agent should set via `TaskUpdate`.
    fn task_status(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Canceled => "failed",
        }
    }
}

/// Map a Linear workflow-state **name** (as it appears in the webhook body's
/// `data.state.name`) to the desired native-task status.
///
/// Unlike the previous GraphQL implementation we only reliably have the
/// human-readable state *name* in the webhook payload, so we match
/// case-insensitively on the well-known state-name families used across the
/// Firefly Pro / Linear pipelines:
///
/// - `In Progress` / `Started` / `In Review` / `Code Review` → still in
///   progress (`🔄`)
/// - `Done` / `Completed` / `Merged` / any `QA` state (`QA Testing`,
///   `QA Failed`, `QA`) → suggest `✅` (the inbound sync treats handed-off
///   QA-land states as "done" for an in-progress dev task)
/// - `Canceled` / `Cancelled` / `Duplicate` → suggest `❌`
///
/// Anything else (Backlog, Todo, Triage, or an unknown name) is treated as
/// "no actionable drift" → `None`.
fn map_linear_state(state_name: &str) -> Option<DesiredStatus> {
    let n = state_name.trim().to_ascii_lowercase();
    if n.is_empty() {
        return None;
    }
    // Canceled family first (so "cancelled"/"duplicate" never fall through).
    if n.contains("cancel") || n == "duplicate" {
        return Some(DesiredStatus::Canceled);
    }
    // Completed / done family, including QA states.
    if n.contains("done") || n.contains("complete") || n.contains("merged") || n.contains("qa") {
        return Some(DesiredStatus::Completed);
    }
    // In-progress family.
    if n.contains("in progress")
        || n.contains("started")
        || n.contains("in review")
        || n.contains("code review")
    {
        return Some(DesiredStatus::InProgress);
    }
    None
}

/// Extract a Linear issue ID from a task subject containing `@linear:{ID}`.
///
/// Returns `Some("PREFIX-123")` if found, `None` otherwise. Mirrors the logic
/// in `task_completed::extract_linear_id` so the two halves of the sync agree
/// on what a valid tag looks like.
fn extract_linear_id(subject: &str) -> Option<&str> {
    let marker = "@linear:";
    let start = subject.find(marker)?;
    let after = &subject[start + marker.len()..];
    let end = after
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after.len());
    let id = &after[..end];
    if let Some(hyphen) = id.find('-') {
        let prefix = &id[..hyphen];
        let number = &id[hyphen + 1..];
        if !prefix.is_empty()
            && prefix.chars().all(|c| c.is_ascii_alphanumeric())
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit())
        {
            return Some(id);
        }
    }
    None
}

/// Whether a task is "in progress" for sync purposes: either the native status
/// is `in_progress`, or the subject carries the `🔄` two-emoji status prefix.
fn is_in_progress(task: &Task) -> bool {
    task.status == "in_progress" || task.subject.contains('🔄')
}

/// Find the active session's task directory: `~/.claude/tasks/{session_id}/`.
/// Returns `None` when the directory is missing or has no `.json` task files.
/// Mirrors `task_persist::find_active_task_dir` (no cross-project fallback).
fn find_active_task_dir(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let session_dir = home.join(".claude").join("tasks").join(session_id);
    if fs.is_dir(&session_dir) {
        Some(session_dir)
    } else {
        None
    }
}

/// Read all tasks from a task directory (each `*.json` file is one task).
fn read_tasks(fs: &dyn FileSystemPort, dir: &Path) -> Vec<Task> {
    let mut tasks = Vec::new();
    if let Ok(entries) = fs.read_dir(dir) {
        for path in entries {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !Path::new(&name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("json"))
                || name.starts_with('.')
            {
                continue;
            }
            if let Ok(content) = fs.read_to_string(&path) {
                if let Ok(task) = serde_json::from_str::<Task>(&content) {
                    tasks.push(task);
                }
            }
        }
    }
    tasks
}

/// Collect in-progress native tasks that carry a `@linear:{ID}` tag.
fn collect_linked_in_progress(tasks: &[Task]) -> Vec<LinkedTask> {
    tasks
        .iter()
        .filter(|t| is_in_progress(t))
        .filter_map(|t| {
            extract_linear_id(&t.subject).map(|id| LinkedTask {
                task_id: t.id.clone(),
                subject: t.subject.clone(),
                linear_id: id.to_string(),
            })
        })
        .collect()
}

/// Resolve the Hookdeck API key: env first, then `~/.claude/sentinel/secrets.toml`.
///
/// The secrets.toml is account-keyed TOML; each top-level section is an account
/// (or `[global]`) and a section may carry a `HOOKDECK_API_KEY`. We scan all
/// sections and return the first key found. The Hookdeck key lives in `[global]`.
fn resolve_hookdeck_api_key(env: &dyn EnvPort, fs: &dyn FileSystemPort) -> Option<String> {
    if let Some(key) = env.var("HOOKDECK_API_KEY").filter(|s| !s.is_empty()) {
        return Some(key);
    }
    let home = fs.home_dir()?;
    let path = home.join(".claude").join("sentinel").join("secrets.toml");
    let content = fs.read_to_string(&path).ok()?;
    parse_hookdeck_key_from_secrets(&content)
}

/// Parse the first `HOOKDECK_API_KEY` out of an account-keyed secrets.toml body.
fn parse_hookdeck_key_from_secrets(content: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(content).ok()?;
    let table = value.as_table()?;
    for (_account, section) in table {
        if let Some(section_table) = section.as_table() {
            if let Some(key) = section_table
                .get("HOOKDECK_API_KEY")
                .and_then(|v| v.as_str())
            {
                if !key.is_empty() {
                    return Some(key.to_string());
                }
            }
        }
    }
    None
}

/// Path to the per-session throttle marker.
fn throttle_marker(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    Some(
        home.join(".claude")
            .join("sentinel")
            .join("state")
            .join(format!("linear-inbound-sync-{session_id}.ts")),
    )
}

/// Path to the per-session Hookdeck cursor (stores the last-seen event id).
fn cursor_path(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    Some(
        home.join(".claude")
            .join("sentinel")
            .join("state")
            .join(format!("linear-hookdeck-cursor-{session_id}")),
    )
}

/// Read the persisted cursor (last-seen event id). Missing/empty → `None`.
fn read_cursor(fs: &dyn FileSystemPort, path: &Path) -> Option<String> {
    let content = fs.read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Persist the cursor (newest processed event id). Best-effort.
fn write_cursor(fs: &dyn FileSystemPort, path: &Path, event_id: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs.create_dir_all(parent);
    }
    let _ = fs.write(path, event_id.as_bytes());
}

/// Return `true` if a poll is allowed right now (no recent marker), `false` if
/// the last poll was within [`POLL_INTERVAL`]. Missing/garbled marker → allowed.
fn poll_allowed(fs: &dyn FileSystemPort, marker: &Path) -> bool {
    let Ok(content) = fs.read_to_string(marker) else {
        return true;
    };
    let Ok(prev) = content.trim().parse::<u64>() else {
        return true;
    };
    let now = now_unix();
    now.saturating_sub(prev) >= POLL_INTERVAL.as_secs()
}

/// Persist the current timestamp into the throttle marker (best-effort).
fn record_poll(fs: &dyn FileSystemPort, marker: &Path) {
    if let Some(parent) = marker.parent() {
        let _ = fs.create_dir_all(parent);
    }
    let _ = fs.write(marker, now_unix().to_string().as_bytes());
}

/// Current unix time in seconds.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// One Linear state transition parsed out of a Hookdeck event delivery.
#[derive(Debug, Clone)]
struct EventTransition {
    /// The Hookdeck event id (used to advance the cursor).
    event_id: String,
    /// The Linear issue identifier, e.g. `FIR-123`.
    identifier: String,
    /// The Linear workflow-state name from `data.state.name`.
    state_name: String,
}

/// Parse the Hookdeck events-list response into newest-first transitions,
/// keeping only those carrying a parseable Linear `Issue` payload with a state.
///
/// The Hookdeck event model nests the raw webhook delivery under `data`
/// (requested via `include=data`); the original webhook JSON body is at
/// `data.body`. Each event also has a top-level `id` (the cursor key).
///
/// We extract the Linear issue identifier + new state name from `data.body`
/// using the same field shape `hookdeck_decoders::linear` decodes: a Linear
/// webhook body has `type: "Issue"`, `data.identifier`, and `data.state.name`.
fn parse_events(body: &serde_json::Value) -> Vec<EventTransition> {
    let mut out = Vec::new();
    let Some(models) = body.get("models").and_then(|m| m.as_array()) else {
        return out;
    };
    for event in models {
        let Some(event_id) = event.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        // The raw Linear webhook body lives at data.body when include=data.
        let Some(webhook) = event.pointer("/data/body") else {
            continue;
        };
        if let Some((identifier, state_name)) = parse_linear_webhook(webhook) {
            out.push(EventTransition {
                event_id: event_id.to_string(),
                identifier,
                state_name,
            });
        }
    }
    out
}

/// Extract `(identifier, state_name)` from a raw Linear webhook body.
///
/// Only `Issue` payloads that carry a `data.state.name` are considered (these
/// are the state-bearing snapshots — `create`/`update` both include the issue's
/// current state). Mirrors the field shape decoded by
/// `hookdeck_decoders::linear` (`type == "Issue"`, `data.identifier`,
/// `data.state.name`).
fn parse_linear_webhook(webhook: &serde_json::Value) -> Option<(String, String)> {
    let entity = webhook.get("type").and_then(|v| v.as_str())?;
    if entity != "Issue" {
        return None;
    }
    let identifier = webhook
        .pointer("/data/identifier")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let state_name = webhook
        .pointer("/data/state/name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    Some((identifier, state_name))
}

/// Poll the Hookdeck Events API for the Linear source, newest-first, with the
/// raw delivery body included. Returns parsed transitions on success or an empty
/// vec on any error (fail-open). Runs inside the shared bounded async runtime so
/// it can never block the hook beyond the budget.
fn poll_hookdeck_events(api_key: &str) -> Vec<EventTransition> {
    let api_key = api_key.to_string();
    super::run_async(async move {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()
        {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let resp = client
            .get(HOOKDECK_EVENTS_URL)
            .bearer_auth(&api_key)
            .query(&[
                ("source_id", LINEAR_SOURCE_ID),
                ("limit", &EVENTS_LIMIT.to_string()),
                ("order_by", "created_at"),
                ("dir", "desc"),
                // include=data is REQUIRED — without it the event model omits
                // the `data` object (and thus the raw webhook at `data.body`).
                ("include", "data"),
            ])
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        match resp.json::<serde_json::Value>().await {
            Ok(body) => parse_events(&body),
            Err(_) => Vec::new(),
        }
    })
}

/// Split the newest-first event list at the cursor: return only events newer
/// than (i.e. listed *before*) the event whose id equals the cursor. If the
/// cursor is absent or not found in the page, every event is "new".
fn events_after_cursor<'a>(
    events: &'a [EventTransition],
    cursor: Option<&str>,
) -> &'a [EventTransition] {
    let Some(cursor) = cursor else {
        return events;
    };
    match events.iter().position(|e| e.event_id == cursor) {
        // Events before the cursor position are strictly newer (desc order).
        Some(idx) => &events[..idx],
        // Cursor not on this page → treat all as new (e.g. cursor aged out).
        None => events,
    }
}

/// A detected drift: an in-progress native task whose Linear issue (per the
/// Hookdeck event stream) has moved to a state that disagrees with `🔄`.
struct Drift {
    task_id: String,
    subject: String,
    linear_id: String,
    linear_state_name: String,
    desired: DesiredStatus,
}

/// Compute drifts by cross-referencing new Hookdeck events against in-progress
/// linked tasks.
///
/// For each linked task we take the **most recent** (newest-first wins) event
/// for its Linear id, map the event's state name to a desired status, and emit
/// a drift only when the desired status disagrees with the task's current
/// in-progress `🔄` (i.e. the event says `✅`/`❌`). Events whose state maps to
/// `InProgress` (or doesn't map at all) produce no drift.
fn compute_drifts(linked: &[LinkedTask], events: &[EventTransition]) -> Vec<Drift> {
    let mut drifts = Vec::new();
    for task in linked {
        // Newest-first list → first match is the latest known state.
        let Some(event) = events.iter().find(|e| e.identifier == task.linear_id) else {
            continue;
        };
        let Some(desired) = map_linear_state(&event.state_name) else {
            continue;
        };
        // An in-progress task whose Linear issue is still "in progress" is in
        // sync — no drift (the task already shows 🔄).
        if desired == DesiredStatus::InProgress {
            continue;
        }
        drifts.push(Drift {
            task_id: task.task_id.clone(),
            subject: task.subject.clone(),
            linear_id: task.linear_id.clone(),
            linear_state_name: event.state_name.clone(),
            desired,
        });
    }
    drifts
}

/// Render the context block injected when drift is detected.
fn render_drift_context(drifts: &[Drift]) -> String {
    let mut ctx = String::from(
        "[Linear Inbound Sync] One or more in-progress tasks have moved in Linear \
         (detected via the Hookdeck event stream). Reconcile each via TaskUpdate \
         (update status + the two-emoji subject prefix):\n",
    );
    for d in drifts {
        let _ = write!(
            ctx,
            "\n  - Task #{id} '{subject}' — Linear {linear} is now \"{state}\" → set status `{ts}` (prefix {emoji})",
            id = d.task_id,
            subject = d.subject,
            linear = d.linear_id,
            state = d.linear_state_name,
            ts = d.desired.task_status(),
            emoji = d.desired.emoji(),
        );
    }
    ctx.push_str(
        "\n\nOnly the listed tasks drifted; leave all others unchanged. \
         If a task is already at the suggested status, no action is needed.",
    );
    ctx
}

/// Process the inbound-sync hook on `UserPromptSubmit`.
///
/// Fail-open contract: every early return and every error path yields
/// `HookOutput::allow()`. The only non-allow output is a context injection,
/// which never blocks.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    // Need a session id to scope tasks + the throttle marker + the cursor.
    let session_id = match input
        .session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| ctx.session_id())
    {
        Some(s) if !s.is_empty() => s,
        _ => return HookOutput::allow(),
    };

    // 1. Read native tasks for this session.
    let Some(task_dir) = find_active_task_dir(ctx.fs, &session_id) else {
        return HookOutput::allow();
    };
    let tasks = read_tasks(ctx.fs, &task_dir);
    let linked = collect_linked_in_progress(&tasks);
    if linked.is_empty() {
        // No @linear in-progress tasks → nothing to sync.
        return HookOutput::allow();
    }

    // 2. Resolve the Hookdeck API key (env → secrets.toml). No key → silent allow.
    let Some(api_key) = resolve_hookdeck_api_key(ctx.env, ctx.fs) else {
        return HookOutput::allow();
    };

    // 3. Throttle: at most one poll per POLL_INTERVAL per session.
    let Some(marker) = throttle_marker(ctx.fs, &session_id) else {
        return HookOutput::allow();
    };
    if !poll_allowed(ctx.fs, &marker) {
        return HookOutput::allow();
    }
    // Record the poll attempt up front so a slow/failed Hookdeck call still
    // counts against the throttle window (don't hammer the API on errors).
    record_poll(ctx.fs, &marker);

    // 4. Poll Hookdeck for the newest Linear-source events.
    let all_events = poll_hookdeck_events(&api_key);
    if all_events.is_empty() {
        return HookOutput::allow();
    }

    // 5. Cursor: process only events newer than the last-seen one.
    let Some(cursor_file) = cursor_path(ctx.fs, &session_id) else {
        return HookOutput::allow();
    };
    let cursor = read_cursor(ctx.fs, &cursor_file);
    let new_events = events_after_cursor(&all_events, cursor.as_deref());

    // Advance the cursor to the newest event id regardless of drift (the page
    // is newest-first, so element 0 is the highwater). Persisting even when
    // there's no drift means we won't re-scan the same events next window.
    if let Some(newest) = all_events.first() {
        write_cursor(ctx.fs, &cursor_file, &newest.event_id);
    }

    if new_events.is_empty() {
        return HookOutput::allow();
    }

    // 6. Compute drift; only inject when a new event disagrees with 🔄.
    let drifts = compute_drifts(&linked, new_events);
    if drifts.is_empty() {
        return HookOutput::allow();
    }

    let context = render_drift_context(&drifts);

    // Emit a channel event so the drift surfaces as a real-time push too.
    let summary = format!(
        "{} task(s) drifted from Linear (in-progress → terminal, via Hookdeck)",
        drifts.len()
    );
    let mut meta = serde_json::Map::new();
    meta.insert(
        "drift_count".to_string(),
        serde_json::Value::from(drifts.len()),
    );
    meta.insert(
        "linear_ids".to_string(),
        serde_json::Value::from(
            drifts
                .iter()
                .map(|d| d.linear_id.clone())
                .collect::<Vec<_>>(),
        ),
    );
    crate::channel_events::emit(
        ctx.fs,
        ctx.env,
        "linear_inbound_drift",
        &summary,
        meta,
        Some(session_id.as_str()),
        input.cwd.as_deref(),
        Some("linear_inbound_sync"),
    );

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- state-name → emoji/status mapping --------------------------------

    #[test]
    fn test_map_linear_state_in_progress_family() {
        assert_eq!(map_linear_state("In Progress"), Some(DesiredStatus::InProgress));
        assert_eq!(map_linear_state("Started"), Some(DesiredStatus::InProgress));
        assert_eq!(map_linear_state("Code Review"), Some(DesiredStatus::InProgress));
        assert_eq!(DesiredStatus::InProgress.emoji(), "🔄");
        assert_eq!(DesiredStatus::InProgress.task_status(), "in_progress");
    }

    #[test]
    fn test_map_linear_state_completed_family() {
        assert_eq!(map_linear_state("Done"), Some(DesiredStatus::Completed));
        assert_eq!(map_linear_state("Completed"), Some(DesiredStatus::Completed));
        assert_eq!(map_linear_state("QA Testing"), Some(DesiredStatus::Completed));
        assert_eq!(map_linear_state("Merged"), Some(DesiredStatus::Completed));
        assert_eq!(DesiredStatus::Completed.emoji(), "✅");
        assert_eq!(DesiredStatus::Completed.task_status(), "completed");
    }

    #[test]
    fn test_map_linear_state_canceled_family() {
        assert_eq!(map_linear_state("Canceled"), Some(DesiredStatus::Canceled));
        assert_eq!(map_linear_state("Cancelled"), Some(DesiredStatus::Canceled));
        assert_eq!(map_linear_state("Duplicate"), Some(DesiredStatus::Canceled));
        assert_eq!(DesiredStatus::Canceled.emoji(), "❌");
        assert_eq!(DesiredStatus::Canceled.task_status(), "failed");
    }

    #[test]
    fn test_map_linear_state_non_actionable() {
        assert_eq!(map_linear_state("Backlog"), None);
        assert_eq!(map_linear_state("Todo"), None);
        assert_eq!(map_linear_state("Triage"), None);
        assert_eq!(map_linear_state(""), None);
        assert_eq!(map_linear_state("weird"), None);
    }

    // --- linear id extraction --------------------------------------------

    #[test]
    fn test_extract_linear_id() {
        assert_eq!(
            extract_linear_id("🔄 [P1] Do thing @linear:FIR-123"),
            Some("FIR-123")
        );
        assert_eq!(extract_linear_id("Task @linear:SYN-42"), Some("SYN-42"));
        assert_eq!(extract_linear_id("no tag here"), None);
    }

    // --- in-progress detection -------------------------------------------

    #[test]
    fn test_is_in_progress() {
        let by_status = Task {
            id: "1".into(),
            subject: "x".into(),
            status: "in_progress".into(),
        };
        let by_emoji = Task {
            id: "2".into(),
            subject: "🔄 x @linear:FIR-1".into(),
            status: "pending".into(),
        };
        let neither = Task {
            id: "3".into(),
            subject: "✅ done".into(),
            status: "completed".into(),
        };
        assert!(is_in_progress(&by_status));
        assert!(is_in_progress(&by_emoji));
        assert!(!is_in_progress(&neither));
    }

    #[test]
    fn test_collect_linked_in_progress() {
        let tasks = vec![
            Task {
                id: "1".into(),
                subject: "🔄 work @linear:FIR-1".into(),
                status: "in_progress".into(),
            },
            // in-progress but no linear tag → skipped
            Task {
                id: "2".into(),
                subject: "🔄 untagged".into(),
                status: "in_progress".into(),
            },
            // tagged but pending → skipped
            Task {
                id: "3".into(),
                subject: "⬜ later @linear:FIR-9".into(),
                status: "pending".into(),
            },
        ];
        let linked = collect_linked_in_progress(&tasks);
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].task_id, "1");
        assert_eq!(linked[0].linear_id, "FIR-1");
    }

    // --- secrets.toml key resolution -------------------------------------

    #[test]
    fn test_parse_hookdeck_key_from_global() {
        let toml = r#"
[firefly-pro]
LINEAR_API_KEY = "lin_api_ABC123"
[global]
HOOKDECK_API_KEY = "hd_global_key"
"#;
        assert_eq!(
            parse_hookdeck_key_from_secrets(toml),
            Some("hd_global_key".to_string())
        );
    }

    #[test]
    fn test_parse_hookdeck_key_in_account_section() {
        let toml = r#"
[firefly-pro]
HOOKDECK_API_KEY = "hd_account_key"
"#;
        assert_eq!(
            parse_hookdeck_key_from_secrets(toml),
            Some("hd_account_key".to_string())
        );
    }

    #[test]
    fn test_parse_hookdeck_key_absent() {
        let toml = r#"
[global]
LINEAR_API_KEY = "x"
"#;
        assert_eq!(parse_hookdeck_key_from_secrets(toml), None);
    }

    #[test]
    fn test_resolve_hookdeck_api_key_prefers_env() {
        let env = crate::hooks::test_support::StubEnv::with(&[("HOOKDECK_API_KEY", "from_env")]);
        let fs = crate::hooks::test_support::StubFs;
        assert_eq!(
            resolve_hookdeck_api_key(&env, &fs),
            Some("from_env".to_string())
        );
    }

    #[test]
    fn test_resolve_hookdeck_api_key_none_when_nothing() {
        // StubEnv has no var; StubFs read_to_string always errors → no secrets.
        let env = crate::hooks::test_support::StubEnv::new();
        let fs = crate::hooks::test_support::StubFs;
        assert_eq!(resolve_hookdeck_api_key(&env, &fs), None);
    }

    // --- Hookdeck event parsing ------------------------------------------

    /// Build a Hookdeck events-list response wrapping the given Linear webhook
    /// bodies, each as one event with the supplied id (caller supplies
    /// newest-first order).
    fn hookdeck_response(events: &[(&str, serde_json::Value)]) -> serde_json::Value {
        let models: Vec<serde_json::Value> = events
            .iter()
            .map(|(id, webhook)| {
                serde_json::json!({
                    "id": id,
                    "source_id": LINEAR_SOURCE_ID,
                    "data": { "body": webhook }
                })
            })
            .collect();
        serde_json::json!({ "models": models, "count": models.len() })
    }

    fn linear_issue_webhook(identifier: &str, state_name: &str) -> serde_json::Value {
        serde_json::json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "identifier": identifier,
                "title": "Some issue",
                "state": { "name": state_name, "type": "completed" }
            },
            "updatedFrom": { "stateId": "prior" }
        })
    }

    #[test]
    fn test_parse_events_extracts_identifier_and_state() {
        let body = hookdeck_response(&[
            ("evt_2", linear_issue_webhook("FIR-2", "In Progress")),
            ("evt_1", linear_issue_webhook("FIR-1", "Done")),
        ]);
        let events = parse_events(&body);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_id, "evt_2");
        assert_eq!(events[0].identifier, "FIR-2");
        assert_eq!(events[0].state_name, "In Progress");
        assert_eq!(events[1].identifier, "FIR-1");
        assert_eq!(events[1].state_name, "Done");
    }

    #[test]
    fn test_parse_events_skips_non_issue_and_stateless() {
        let comment = serde_json::json!({
            "type": "Comment",
            "data": { "issue": { "identifier": "FIR-9" }, "body": "hi" }
        });
        let issue_no_state = serde_json::json!({
            "type": "Issue",
            "data": { "identifier": "FIR-8", "title": "no state" }
        });
        let body = hookdeck_response(&[
            ("evt_c", comment),
            ("evt_ns", issue_no_state),
            ("evt_ok", linear_issue_webhook("FIR-7", "Done")),
        ]);
        let events = parse_events(&body);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].identifier, "FIR-7");
    }

    #[test]
    fn test_parse_events_empty_on_no_models() {
        let body = serde_json::json!({ "count": 0, "pagination": {} });
        assert!(parse_events(&body).is_empty());
    }

    #[test]
    fn test_parse_linear_webhook_shape() {
        let wh = linear_issue_webhook("FIR-5", "QA Testing");
        assert_eq!(
            parse_linear_webhook(&wh),
            Some(("FIR-5".to_string(), "QA Testing".to_string()))
        );
    }

    // --- cursor advance ---------------------------------------------------

    fn transition(id: &str, identifier: &str, state: &str) -> EventTransition {
        EventTransition {
            event_id: id.into(),
            identifier: identifier.into(),
            state_name: state.into(),
        }
    }

    #[test]
    fn test_events_after_cursor_none_returns_all() {
        let events = vec![
            transition("evt_3", "FIR-3", "Done"),
            transition("evt_2", "FIR-2", "Done"),
            transition("evt_1", "FIR-1", "Done"),
        ];
        assert_eq!(events_after_cursor(&events, None).len(), 3);
    }

    #[test]
    fn test_events_after_cursor_stops_at_cursor() {
        let events = vec![
            transition("evt_3", "FIR-3", "Done"),
            transition("evt_2", "FIR-2", "Done"),
            transition("evt_1", "FIR-1", "Done"),
        ];
        // Cursor = evt_2 → only evt_3 (newer) is new.
        let new = events_after_cursor(&events, Some("evt_2"));
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].event_id, "evt_3");
    }

    #[test]
    fn test_events_after_cursor_unknown_cursor_returns_all() {
        let events = vec![transition("evt_3", "FIR-3", "Done")];
        // Cursor aged out of the page → treat all as new.
        assert_eq!(events_after_cursor(&events, Some("evt_old")).len(), 1);
    }

    #[test]
    fn test_read_write_cursor_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cursor");
        struct RealFs;
        impl FileSystemPort for RealFs {
            fn home_dir(&self) -> Option<PathBuf> {
                dirs::home_dir()
            }
            fn read_to_string(&self, p: &Path) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
                std::fs::read_to_string(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn write(&self, p: &Path, c: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                if let Some(par) = p.parent() {
                    std::fs::create_dir_all(par).map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
                }
                std::fs::write(p, c).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn create_dir_all(&self, p: &Path) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                std::fs::create_dir_all(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn read_dir(&self, _: &Path) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
                Ok(vec![])
            }
            fn exists(&self, p: &Path) -> bool {
                p.exists()
            }
            fn is_dir(&self, p: &Path) -> bool {
                p.is_dir()
            }
            fn metadata(&self, p: &Path) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
                std::fs::metadata(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn append(&self, _: &Path, _: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                Ok(())
            }
        }
        let fs = RealFs;
        assert_eq!(read_cursor(&fs, &path), None);
        write_cursor(&fs, &path, "evt_42");
        assert_eq!(read_cursor(&fs, &path), Some("evt_42".to_string()));
    }

    // --- drift computation -----------------------------------------------

    fn linked(task_id: &str, linear_id: &str) -> LinkedTask {
        LinkedTask {
            task_id: task_id.into(),
            subject: format!("🔄 work @linear:{linear_id}"),
            linear_id: linear_id.into(),
        }
    }

    #[test]
    fn test_compute_drifts_flags_completed_and_canceled_only() {
        let tasks = vec![linked("1", "FIR-1"), linked("2", "FIR-2"), linked("3", "FIR-3")];
        let events = vec![
            transition("e1", "FIR-1", "Done"),
            transition("e2", "FIR-2", "In Progress"), // no drift
            transition("e3", "FIR-3", "Canceled"),
        ];
        let drifts = compute_drifts(&tasks, &events);
        assert_eq!(drifts.len(), 2);
        let ids: Vec<&str> = drifts.iter().map(|d| d.linear_id.as_str()).collect();
        assert!(ids.contains(&"FIR-1"));
        assert!(ids.contains(&"FIR-3"));
        assert!(!ids.contains(&"FIR-2"));
    }

    #[test]
    fn test_compute_drifts_empty_when_all_in_sync() {
        let tasks = vec![linked("1", "FIR-1")];
        let events = vec![transition("e1", "FIR-1", "In Progress")];
        assert!(compute_drifts(&tasks, &events).is_empty());
    }

    #[test]
    fn test_compute_drifts_ignores_unmatched_issues() {
        // Event for an issue with no in-progress task → no drift.
        let tasks = vec![linked("1", "FIR-1")];
        let events = vec![transition("e1", "FIR-99", "Done")];
        assert!(compute_drifts(&tasks, &events).is_empty());
    }

    #[test]
    fn test_compute_drifts_uses_newest_event_per_issue() {
        // Newest-first: the Done event (e2) precedes a stale In Progress (e1).
        let tasks = vec![linked("1", "FIR-1")];
        let events = vec![
            transition("e2", "FIR-1", "Done"),
            transition("e1", "FIR-1", "In Progress"),
        ];
        let drifts = compute_drifts(&tasks, &events);
        assert_eq!(drifts.len(), 1);
        assert_eq!(drifts[0].desired, DesiredStatus::Completed);
    }

    #[test]
    fn test_render_drift_context_lists_each() {
        let drifts = vec![Drift {
            task_id: "7".into(),
            subject: "🔄 ship @linear:FIR-9".into(),
            linear_id: "FIR-9".into(),
            linear_state_name: "Done".into(),
            desired: DesiredStatus::Completed,
        }];
        let ctx = render_drift_context(&drifts);
        assert!(ctx.contains("[Linear Inbound Sync]"));
        assert!(ctx.contains("Hookdeck"));
        assert!(ctx.contains("Task #7"));
        assert!(ctx.contains("FIR-9"));
        assert!(ctx.contains("completed"));
        assert!(ctx.contains("✅"));
    }

    // --- throttle ---------------------------------------------------------

    #[test]
    fn test_poll_allowed_when_no_marker() {
        let fs = crate::hooks::test_support::StubFs; // read_to_string errors
        assert!(poll_allowed(&fs, Path::new("/nope/marker.ts")));
    }

    #[test]
    fn test_poll_throttle_honored_with_recent_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("m.ts");
        // Real-FS helper just for this test.
        struct RealFs;
        impl FileSystemPort for RealFs {
            fn home_dir(&self) -> Option<PathBuf> {
                dirs::home_dir()
            }
            fn read_to_string(&self, p: &Path) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
                std::fs::read_to_string(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn write(&self, p: &Path, c: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                if let Some(par) = p.parent() {
                    std::fs::create_dir_all(par).map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
                }
                std::fs::write(p, c).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn create_dir_all(&self, p: &Path) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                std::fs::create_dir_all(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn read_dir(&self, _: &Path) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
                Ok(vec![])
            }
            fn exists(&self, p: &Path) -> bool {
                p.exists()
            }
            fn is_dir(&self, p: &Path) -> bool {
                p.is_dir()
            }
            fn metadata(&self, p: &Path) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
                std::fs::metadata(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn append(&self, _: &Path, _: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                Ok(())
            }
        }
        let fs = RealFs;
        // Record a poll now → immediately throttled.
        record_poll(&fs, &marker);
        assert!(
            !poll_allowed(&fs, &marker),
            "a just-recorded poll must throttle the next attempt"
        );
        // A marker far in the past → allowed.
        std::fs::write(&marker, (now_unix() - POLL_INTERVAL.as_secs() - 10).to_string()).unwrap();
        assert!(poll_allowed(&fs, &marker));
    }

    // --- process fail-open ------------------------------------------------

    #[test]
    fn test_process_allows_when_no_session() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let out = process(&input, &ctx);
        assert!(out.blocked.is_none());
        assert!(out.hook_specific_output.is_none());
    }

    #[test]
    fn test_process_allows_when_no_tasks_dir() {
        // session id present, but StubFs reports no dirs → allow.
        let input = HookInput {
            session_id: Some("sess-1".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let out = process(&input, &ctx);
        assert!(out.blocked.is_none());
        assert!(out.hook_specific_output.is_none());
    }
}
