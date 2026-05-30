//! Linear → native-task INBOUND sync hook (the inbound half of the
//! Linear↔native-task two-way sync).
//!
//! Runs on `UserPromptSubmit`. Best-effort and **MUST fail open** — it never
//! blocks the user's prompt under any circumstance.
//!
//! ## What it does
//!
//! 1. Reads the active session's native tasks (same on-disk format as
//!    `task_persist`) and finds those tagged `@linear:{ID}` that are currently
//!    *in progress* (two-emoji status prefix `🔄`).
//! 2. For each distinct Linear issue ID, queries the Linear GraphQL API
//!    (`https://api.linear.app/graphql`) for the issue's current workflow state
//!    name + type. The query is batched across all IDs in a single request.
//! 3. Maps the Linear state type to a desired native-task status emoji:
//!    - `started` (In Progress) → `🔄` (no drift)
//!    - `completed` (Done / QA states surfaced as completed) → suggest `✅`
//!    - `canceled` → suggest `❌`
//! 4. When (and only when) there is actual drift, injects a concise context
//!    block telling the agent which tasks to `TaskUpdate`. The hook itself
//!    cannot call `TaskUpdate` — it injects an instruction, mirroring the
//!    established pattern in `task_completed.rs`.
//!
//! ## Throttle
//!
//! Polls the Linear API at most once per [`POLL_INTERVAL`] per session. A
//! timestamp marker is written to `~/.claude/sentinel/state/`. Within the
//! window the hook short-circuits to `allow()` without any network call.
//!
//! ## Key resolution
//!
//! `LINEAR_API_KEY` is resolved from the process env first (populated from
//! `~/.claude/settings.json`), then — if absent — by reading
//! `~/.claude/sentinel/secrets.toml` directly and scanning each `[account]`
//! section for a `LINEAR_API_KEY`. No MCP server is involved.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::{EnvPort, FileSystemPort, HookContext};

/// Minimum interval between Linear API polls, per session.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Linear GraphQL endpoint.
const LINEAR_GRAPHQL_URL: &str = "https://api.linear.app/graphql";

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

/// Map a Linear workflow-state *type* (`state.type` in the GraphQL schema) to
/// the desired native-task status.
///
/// Linear's state types are: `triage`, `backlog`, `unstarted`, `started`,
/// `completed`, `canceled`. We only act on the three that matter for an
/// in-progress native task:
/// - `started` → still in progress (`🔄`)
/// - `completed` → suggest `✅` (Done and QA-style states surface here)
/// - `canceled` → suggest `❌`
///
/// Everything else (`triage`/`backlog`/`unstarted`, or an unknown type) is
/// treated as "no actionable drift" → `None`.
fn map_linear_state(state_type: &str) -> Option<DesiredStatus> {
    match state_type {
        "started" => Some(DesiredStatus::InProgress),
        "completed" => Some(DesiredStatus::Completed),
        "canceled" => Some(DesiredStatus::Canceled),
        _ => None,
    }
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

/// Resolve the Linear API key: env first, then `~/.claude/sentinel/secrets.toml`.
///
/// The secrets.toml is account-keyed TOML; each top-level section is an account
/// and a section may carry a `LINEAR_API_KEY`. We scan all sections and return
/// the first key found (a `[global]` section is included in the scan).
fn resolve_linear_api_key(env: &dyn EnvPort, fs: &dyn FileSystemPort) -> Option<String> {
    if let Some(key) = env.var("LINEAR_API_KEY").filter(|s| !s.is_empty()) {
        return Some(key);
    }
    let home = fs.home_dir()?;
    let path = home
        .join(".claude")
        .join("sentinel")
        .join("secrets.toml");
    let content = fs.read_to_string(&path).ok()?;
    parse_linear_key_from_secrets(&content)
}

/// Parse the first `LINEAR_API_KEY` out of an account-keyed secrets.toml body.
fn parse_linear_key_from_secrets(content: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(content).ok()?;
    let table = value.as_table()?;
    for (_account, section) in table {
        if let Some(section_table) = section.as_table() {
            if let Some(key) = section_table.get("LINEAR_API_KEY").and_then(|v| v.as_str()) {
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

/// A Linear issue's resolved state, keyed by identifier (e.g. `FIR-123`).
#[derive(Debug, Clone)]
struct LinearState {
    identifier: String,
    state_name: String,
    state_type: String,
}

/// Build a batched GraphQL query that fetches each issue by identifier.
///
/// Linear's `issue(id:)` accepts the human identifier (e.g. `"FIR-123"`), so we
/// alias one field per issue: `i0: issue(id: "FIR-123") { identifier state { name type } }`.
fn build_batched_query(ids: &[String]) -> String {
    let mut q = String::from("query {");
    for (idx, id) in ids.iter().enumerate() {
        // ids are validated PREFIX-NUMBER shape, safe to interpolate, but escape
        // quotes defensively anyway.
        let safe = id.replace('"', "\\\"");
        let _ = write!(
            q,
            " i{idx}: issue(id: \"{safe}\") {{ identifier state {{ name type }} }}"
        );
    }
    q.push_str(" }");
    q
}

/// Parse the batched-query response into a list of resolved states.
fn parse_states(body: &serde_json::Value) -> Vec<LinearState> {
    let mut out = Vec::new();
    let Some(data) = body.get("data").and_then(|d| d.as_object()) else {
        return out;
    };
    for issue in data.values() {
        let Some(identifier) = issue.get("identifier").and_then(|v| v.as_str()) else {
            continue;
        };
        let state = issue.get("state");
        let state_name = state
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let state_type = state
            .and_then(|s| s.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(LinearState {
            identifier: identifier.to_string(),
            state_name,
            state_type,
        });
    }
    out
}

/// Query the Linear GraphQL API for the current state of each issue ID.
/// Returns an empty vec on any error (fail-open). Runs inside the shared
/// bounded async runtime so it can never block the hook beyond the budget.
fn query_linear_states(api_key: &str, ids: &[String]) -> Vec<LinearState> {
    if ids.is_empty() {
        return Vec::new();
    }
    let query = build_batched_query(ids);
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
            .post(LINEAR_GRAPHQL_URL)
            .header("Authorization", api_key)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({ "query": query }))
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
            Ok(body) => parse_states(&body),
            Err(_) => Vec::new(),
        }
    })
}

/// A detected drift: an in-progress native task whose Linear issue has moved to
/// a terminal state.
struct Drift {
    task_id: String,
    subject: String,
    linear_id: String,
    linear_state_name: String,
    desired: DesiredStatus,
}

/// Compute drifts: for each linked in-progress task, look up its Linear state
/// and emit a drift only when the desired status is terminal (`✅`/`❌`).
fn compute_drifts(linked: &[LinkedTask], states: &[LinearState]) -> Vec<Drift> {
    let mut drifts = Vec::new();
    for task in linked {
        let Some(state) = states.iter().find(|s| s.identifier == task.linear_id) else {
            continue;
        };
        let Some(desired) = map_linear_state(&state.state_type) else {
            continue;
        };
        // An in-progress task whose Linear issue is still "started" is in sync.
        if desired == DesiredStatus::InProgress {
            continue;
        }
        drifts.push(Drift {
            task_id: task.task_id.clone(),
            subject: task.subject.clone(),
            linear_id: task.linear_id.clone(),
            linear_state_name: state.state_name.clone(),
            desired,
        });
    }
    drifts
}

/// Render the context block injected when drift is detected.
fn render_drift_context(drifts: &[Drift]) -> String {
    let mut ctx = String::from(
        "[Linear Inbound Sync] One or more in-progress tasks have moved in Linear. \
         Reconcile each via TaskUpdate (update status + the two-emoji subject prefix):\n",
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
    // Need a session id to scope tasks + the throttle marker.
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

    // 2. Resolve the API key (env → secrets.toml). No key → silent allow.
    let Some(api_key) = resolve_linear_api_key(ctx.env, ctx.fs) else {
        return HookOutput::allow();
    };

    // 3. Throttle: at most one poll per POLL_INTERVAL per session.
    let Some(marker) = throttle_marker(ctx.fs, &session_id) else {
        return HookOutput::allow();
    };
    if !poll_allowed(ctx.fs, &marker) {
        return HookOutput::allow();
    }
    // Record the poll attempt up front so a slow/failed Linear call still
    // counts against the throttle window (don't hammer the API on errors).
    record_poll(ctx.fs, &marker);

    // 4. Distinct Linear IDs, batched query.
    let mut ids: Vec<String> = linked.iter().map(|t| t.linear_id.clone()).collect();
    ids.sort();
    ids.dedup();
    let states = query_linear_states(&api_key, &ids);
    if states.is_empty() {
        return HookOutput::allow();
    }

    // 5. Compute drift; only inject when there's an actual terminal change.
    let drifts = compute_drifts(&linked, &states);
    if drifts.is_empty() {
        return HookOutput::allow();
    }

    let context = render_drift_context(&drifts);

    // Emit a channel event so the drift surfaces as a real-time push too.
    let summary = format!(
        "{} task(s) drifted from Linear (in-progress → terminal)",
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

    // --- state → emoji/status mapping -------------------------------------

    #[test]
    fn test_map_linear_state_started_is_in_progress() {
        assert_eq!(map_linear_state("started"), Some(DesiredStatus::InProgress));
        assert_eq!(DesiredStatus::InProgress.emoji(), "🔄");
        assert_eq!(DesiredStatus::InProgress.task_status(), "in_progress");
    }

    #[test]
    fn test_map_linear_state_completed_is_done() {
        assert_eq!(map_linear_state("completed"), Some(DesiredStatus::Completed));
        assert_eq!(DesiredStatus::Completed.emoji(), "✅");
        assert_eq!(DesiredStatus::Completed.task_status(), "completed");
    }

    #[test]
    fn test_map_linear_state_canceled_is_failed() {
        assert_eq!(map_linear_state("canceled"), Some(DesiredStatus::Canceled));
        assert_eq!(DesiredStatus::Canceled.emoji(), "❌");
        assert_eq!(DesiredStatus::Canceled.task_status(), "failed");
    }

    #[test]
    fn test_map_linear_state_non_actionable() {
        assert_eq!(map_linear_state("backlog"), None);
        assert_eq!(map_linear_state("unstarted"), None);
        assert_eq!(map_linear_state("triage"), None);
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
    fn test_parse_linear_key_from_secrets() {
        let toml = r#"
[firefly-pro]
LINEAR_API_KEY = "lin_api_ABC123"
[global]
HOOKDECK_API_KEY = "x"
"#;
        assert_eq!(
            parse_linear_key_from_secrets(toml),
            Some("lin_api_ABC123".to_string())
        );
    }

    #[test]
    fn test_parse_linear_key_from_secrets_in_global() {
        let toml = r#"
[global]
LINEAR_API_KEY = "lin_global"
"#;
        assert_eq!(
            parse_linear_key_from_secrets(toml),
            Some("lin_global".to_string())
        );
    }

    #[test]
    fn test_parse_linear_key_from_secrets_absent() {
        let toml = r#"
[global]
HOOKDECK_API_KEY = "x"
"#;
        assert_eq!(parse_linear_key_from_secrets(toml), None);
    }

    #[test]
    fn test_resolve_linear_api_key_prefers_env() {
        let env = crate::hooks::test_support::StubEnv::with(&[("LINEAR_API_KEY", "from_env")]);
        let fs = crate::hooks::test_support::StubFs;
        assert_eq!(
            resolve_linear_api_key(&env, &fs),
            Some("from_env".to_string())
        );
    }

    #[test]
    fn test_resolve_linear_api_key_none_when_nothing() {
        // StubEnv has no var; StubFs read_to_string always errors → no secrets.
        let env = crate::hooks::test_support::StubEnv::new();
        let fs = crate::hooks::test_support::StubFs;
        assert_eq!(resolve_linear_api_key(&env, &fs), None);
    }

    // --- batched query + parse -------------------------------------------

    #[test]
    fn test_build_batched_query() {
        let q = build_batched_query(&["FIR-1".into(), "SYN-2".into()]);
        assert!(q.contains("i0: issue(id: \"FIR-1\")"));
        assert!(q.contains("i1: issue(id: \"SYN-2\")"));
        assert!(q.contains("state { name type }"));
    }

    #[test]
    fn test_parse_states() {
        let body = serde_json::json!({
            "data": {
                "i0": { "identifier": "FIR-1", "state": { "name": "Done", "type": "completed" } },
                "i1": { "identifier": "FIR-2", "state": { "name": "In Progress", "type": "started" } }
            }
        });
        let mut states = parse_states(&body);
        states.sort_by(|a, b| a.identifier.cmp(&b.identifier));
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].identifier, "FIR-1");
        assert_eq!(states[0].state_type, "completed");
        assert_eq!(states[1].state_type, "started");
    }

    #[test]
    fn test_parse_states_empty_on_no_data() {
        let body = serde_json::json!({ "errors": [{ "message": "boom" }] });
        assert!(parse_states(&body).is_empty());
    }

    // --- drift computation -----------------------------------------------

    fn linked(task_id: &str, linear_id: &str) -> LinkedTask {
        LinkedTask {
            task_id: task_id.into(),
            subject: format!("🔄 work @linear:{linear_id}"),
            linear_id: linear_id.into(),
        }
    }

    fn state(id: &str, name: &str, ty: &str) -> LinearState {
        LinearState {
            identifier: id.into(),
            state_name: name.into(),
            state_type: ty.into(),
        }
    }

    #[test]
    fn test_compute_drifts_flags_completed_and_canceled_only() {
        let tasks = vec![linked("1", "FIR-1"), linked("2", "FIR-2"), linked("3", "FIR-3")];
        let states = vec![
            state("FIR-1", "Done", "completed"),
            state("FIR-2", "In Progress", "started"), // no drift
            state("FIR-3", "Canceled", "canceled"),
        ];
        let drifts = compute_drifts(&tasks, &states);
        assert_eq!(drifts.len(), 2);
        let ids: Vec<&str> = drifts.iter().map(|d| d.linear_id.as_str()).collect();
        assert!(ids.contains(&"FIR-1"));
        assert!(ids.contains(&"FIR-3"));
        assert!(!ids.contains(&"FIR-2"));
    }

    #[test]
    fn test_compute_drifts_empty_when_all_in_sync() {
        let tasks = vec![linked("1", "FIR-1")];
        let states = vec![state("FIR-1", "In Progress", "started")];
        assert!(compute_drifts(&tasks, &states).is_empty());
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
            fn read_to_string(&self, p: &Path) -> anyhow::Result<String> {
                Ok(std::fs::read_to_string(p)?)
            }
            fn write(&self, p: &Path, c: &[u8]) -> anyhow::Result<()> {
                if let Some(par) = p.parent() {
                    std::fs::create_dir_all(par)?;
                }
                Ok(std::fs::write(p, c)?)
            }
            fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> {
                Ok(std::fs::create_dir_all(p)?)
            }
            fn read_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> {
                Ok(vec![])
            }
            fn exists(&self, p: &Path) -> bool {
                p.exists()
            }
            fn is_dir(&self, p: &Path) -> bool {
                p.is_dir()
            }
            fn metadata(&self, p: &Path) -> anyhow::Result<std::fs::Metadata> {
                Ok(std::fs::metadata(p)?)
            }
            fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> {
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
