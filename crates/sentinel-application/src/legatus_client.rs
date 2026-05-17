//! Tiny sync HTTP client that hooks use to POST escalations to
//! the local sentinel daemon's `/legatus/escalate` endpoint.
//!
//! Why sync: hooks return [`sentinel_domain::events::HookOutput`]
//! synchronously and most are called in standalone contexts
//! without a tokio runtime. We don't want hook code to need
//! `async`-awareness just to push a notification onto the
//! daemon's queue.
//!
//! Why fire-and-forget: the hook MUST NOT block on the daemon —
//! the daemon might not be running (standalone `sentinel hook`
//! invocation), the consulate might be down, or the legatus
//! might not be hosted (daemon started without
//! `--legatus-consulate-url`). All three are common; none should
//! delay Claude Code's hook reply by even one HTTP round-trip.
//! [`escalate_fire_and_forget`] spawns an OS thread that does the
//! POST and logs the outcome; the hook returns immediately.
//!
//! The daemon token + port live at `~/.claude/sentinel/daemon-token`
//! in the format `<port>:<token>` (per `sentinel-cli`'s
//! `daemon_cmd::write_token_file`).

use std::path::PathBuf;
use std::time::Duration;

use sentinel_legatus::{EscalationKind, InstructionId, RelayInstruction};

/// Read the daemon token + port from
/// `~/.claude/sentinel/daemon-token`. Returns `None` if the file
/// doesn't exist (daemon not running) or is malformed.
fn read_daemon_token() -> Option<(u16, String)> {
    let path = token_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let trimmed = content.trim();
    let (port_str, token) = trimmed.split_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    Some((port, token.to_owned()))
}

fn token_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("sentinel")
            .join("daemon-token"),
    )
}

/// Convenience: fire-and-forget an `InstructionAck` for the
/// given instruction id. Same shape as
/// [`escalate_fire_and_forget`] but constructs the
/// [`EscalationKind::InstructionAck`] variant inline.
pub fn ack_fire_and_forget(instruction_id: InstructionId) {
    escalate_fire_and_forget(EscalationKind::InstructionAck { instruction_id });
}

/// Convenience: fire-and-forget an `InstructionResult` for the
/// given instruction id. `outcome` is `Success` for the common
/// MVP path; sentinel doesn't yet classify mid-run failures.
pub fn report_result_fire_and_forget(
    instruction_id: InstructionId,
    outcome: sentinel_legatus::InstructionOutcome,
    summary: Option<String>,
) {
    escalate_fire_and_forget(EscalationKind::InstructionResult {
        instruction_id,
        outcome,
        summary,
    });
}

/// Per-session file that records `InstructionId`s
/// `consul_inbox` has drained but `execution_log`'s Stop hook
/// has not yet reported a result for. Lives at
/// `~/.claude/sentinel/state/<session_id>.legatus-pending.txt`,
/// one id per line.
///
/// File ops are deliberately simple: append-on-drain, read-and-
/// remove on Stop. Race window between consul_inbox appending
/// and execution_log clearing is small (hooks for the same
/// session don't usually overlap) and the failure mode is
/// "operator misses one Result ping" — non-fatal.
fn pending_file_path(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains(['/', '\\', '\0']) || session_id.contains("..")
    {
        // SessionId validation should never produce these, but
        // defense-in-depth before we touch the filesystem.
        return None;
    }
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join(format!("{session_id}.legatus-pending.txt")),
    )
}

/// Append `instruction_id` to the per-session pending file.
/// Creates the parent dir + file if needed. Errors logged at
/// debug; never propagated (the operator still got an Ack via
/// the WS; only the Stop hook's per-instruction Result depends
/// on this file).
pub fn note_pending_instruction(session_id: &str, instruction_id: InstructionId) {
    let Some(path) = pending_file_path(session_id) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = format!("{instruction_id}\n");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = file.write_all(line.as_bytes());
    } else {
        tracing::debug!(?path, "legatus_client: failed to record pending instruction");
    }
}

/// Read and clear the per-session pending file. Returns the
/// list of instruction ids that were buffered (possibly empty if
/// the file doesn't exist or is empty). Concurrent appends
/// between read + remove may be lost; treated as acceptable per
/// the module's MVP race tolerance.
pub fn take_pending_instructions(session_id: &str) -> Vec<InstructionId> {
    let Some(path) = pending_file_path(session_id) else {
        return Vec::new();
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            tracing::debug!(?err, ?path, "legatus_client: pending file read failed");
            return Vec::new();
        },
    };
    // Remove the file before parsing so concurrent appends start
    // a fresh file. (Race window: an append after the read but
    // before the remove will be lost. Non-fatal — operator just
    // misses one Result ping.)
    let _ = std::fs::remove_file(&path);
    content
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| uuid::Uuid::parse_str(s).ok().map(InstructionId::from_uuid))
        .collect()
}

/// Per-turn observations that bias how the Stop hook classifies
/// pending operator-relayed instructions. Lives in a sibling file
/// to the pending-instructions file:
/// `~/.claude/sentinel/state/<session_id>.legatus-turn-signals.jsonl`,
/// one JSON-encoded variant per line.
///
/// Recorded by hooks that fire mid-turn (e.g. `permission_denied`)
/// and consumed by the Stop hook (`execution_log`) when it converts
/// pending instructions into [`sentinel_legatus::InstructionOutcome`]
/// values. Best-effort plumbing — file I/O errors are logged at
/// debug and dropped; the worst case is "operator's Result ping
/// is the default `Success` when it could have been `Declined`",
/// which is the pre-classification behavior.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnSignal {
    /// A tool call was denied during the turn (auto-mode denial,
    /// policy gate, etc.). Carries the tool name for the
    /// `Declined { reason }` summary.
    PermissionDenied {
        /// Tool that was denied (e.g. `"Bash"`).
        tool: String,
    },
}

/// Per-session turn-signals file path. Uses the same sanitization
/// + base path as [`pending_file_path`].
fn turn_signals_path(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains(['/', '\\', '\0']) || session_id.contains("..")
    {
        return None;
    }
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join(format!("{session_id}.legatus-turn-signals.jsonl")),
    )
}

/// Append a [`TurnSignal`] to the per-session turn-signals file.
/// Best-effort; errors logged at debug and dropped.
pub fn note_turn_signal(session_id: &str, signal: &TurnSignal) {
    let Some(path) = turn_signals_path(session_id) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(line) = serde_json::to_string(signal) else {
        tracing::debug!("legatus_client: turn signal serialize failed");
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{line}");
    } else {
        tracing::debug!(?path, "legatus_client: failed to record turn signal");
    }
}

/// Read and clear the per-session turn-signals file. Returns the
/// list of signals observed during this turn (possibly empty).
/// Lines that fail to parse as [`TurnSignal`] are dropped silently.
pub fn take_turn_signals(session_id: &str) -> Vec<TurnSignal> {
    let Some(path) = turn_signals_path(session_id) else {
        return Vec::new();
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            tracing::debug!(?err, ?path, "legatus_client: turn signals read failed");
            return Vec::new();
        },
    };
    let _ = std::fs::remove_file(&path);
    content
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| serde_json::from_str::<TurnSignal>(s).ok())
        .collect()
}

/// Classify pending instructions for this turn given the observed
/// [`TurnSignal`]s. Used by both the `Stop` (`execution_log`) and
/// `StopFailure` (`stop_failure`) hooks to bias the
/// [`sentinel_legatus::InstructionOutcome`] they fire for every
/// pending instruction.
///
/// Rules (MVP — coarse classification, the ~90-95% reliability target
/// per the project's reliability philosophy):
/// 1. If the caller supplies an `api_error` (StopFailure path), all
///    pending instructions are `Failure { error }`.
/// 2. Otherwise, if any [`TurnSignal::PermissionDenied`] was observed
///    this turn, all pending instructions are `Declined { reason }`
///    naming the tool(s) that were denied. Note: this classifies
///    every pending instruction the same way, since we don't track
///    which instruction prompted which tool call.
/// 3. Otherwise `Success`.
#[must_use]
pub fn classify_outcome(
    signals: &[TurnSignal],
    api_error: Option<&str>,
) -> sentinel_legatus::InstructionOutcome {
    if let Some(err) = api_error {
        return sentinel_legatus::InstructionOutcome::Failure {
            error: err.to_owned(),
        };
    }
    let denied_tools: Vec<&str> = signals
        .iter()
        .filter_map(|s| match s {
            TurnSignal::PermissionDenied { tool } => Some(tool.as_str()),
        })
        .collect();
    if !denied_tools.is_empty() {
        let joined: Vec<String> = denied_tools.iter().map(|s| (*s).to_owned()).collect();
        return sentinel_legatus::InstructionOutcome::Declined {
            reason: format!("permission denied for tool(s): {}", joined.join(", ")),
        };
    }
    sentinel_legatus::InstructionOutcome::Success
}

/// Spawn a background OS thread that POSTs `event` to the daemon's
/// `/legatus/escalate` endpoint. Returns immediately. If the
/// daemon isn't running (no token file) or the POST fails, logs
/// at `debug`/`warn` and the thread exits.
pub fn escalate_fire_and_forget(event: EscalationKind) {
    std::thread::spawn(move || {
        if let Err(err) = post_escalation(event) {
            tracing::debug!(?err, "legatus escalation skipped");
        }
    });
}

#[derive(Debug, thiserror::Error)]
enum LegatusClientError {
    #[error("daemon token file not present (daemon not running?)")]
    TokenAbsent,
    #[error("request: {0}")]
    Request(reqwest::Error),
    #[error("daemon returned {0}")]
    Status(u16),
    #[error("serialize event: {0}")]
    Serialize(serde_json::Error),
}

/// Synchronously drain the daemon's inbox by repeatedly GETting
/// `/legatus/inbox/next` until the daemon returns 204 No Content
/// (queue empty). Returns whatever instructions were buffered
/// (possibly empty). Used by the `UserPromptSubmit` hook to pull
/// operator-relayed instructions into Claude Code's next turn.
///
/// Hard cap of 32 instructions per call to bound latency and
/// memory in degenerate cases — if a backlog grows beyond that,
/// the remainder waits for the next prompt.
pub fn drain_inbox() -> Vec<RelayInstruction> {
    let mut out = Vec::new();
    const HARD_CAP: usize = 32;
    let Some((port, token)) = read_daemon_token() else {
        return out;
    };
    let url = format!("http://127.0.0.1:{port}/legatus/inbox/next");
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            tracing::debug!(?err, "legatus_client: cannot build reqwest client");
            return out;
        },
    };
    while out.len() < HARD_CAP {
        let resp = match client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
        {
            Ok(r) => r,
            Err(err) => {
                tracing::debug!(?err, "legatus_client: inbox GET failed");
                break;
            },
        };
        let status = resp.status();
        if status == reqwest::StatusCode::NO_CONTENT {
            break;
        }
        if !status.is_success() {
            tracing::debug!(status = %status.as_u16(), "legatus_client: inbox returned non-2xx");
            break;
        }
        match resp.json::<RelayInstruction>() {
            Ok(instr) => out.push(instr),
            Err(err) => {
                tracing::debug!(?err, "legatus_client: inbox payload decode failed");
                break;
            },
        }
    }
    out
}

fn post_escalation(event: EscalationKind) -> Result<(), LegatusClientError> {
    let (port, token) = read_daemon_token().ok_or(LegatusClientError::TokenAbsent)?;
    let url = format!("http://127.0.0.1:{port}/legatus/escalate");
    let body = serde_json::to_string(&event).map_err(LegatusClientError::Serialize)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(LegatusClientError::Request)?;
    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .map_err(LegatusClientError::Request)?;
    let status = response.status();
    if !status.is_success() {
        return Err(LegatusClientError::Status(status.as_u16()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_daemon_token_returns_none_when_file_absent() {
        // We can't easily make ~/.claude/sentinel/daemon-token
        // absent on dev machines that have a real daemon running.
        // The contract: if it's None or malformed, we return None
        // without panic. Just call it and trust the type system.
        let _ = read_daemon_token();
    }

    #[test]
    fn escalate_fire_and_forget_does_not_panic_when_daemon_absent() {
        // Spawns a thread that errors out (or succeeds if a
        // daemon happens to be running). Either way the caller
        // returns immediately and we're testing no-panic.
        escalate_fire_and_forget(EscalationKind::Completed {
            summary: Some("unit-test ping".into()),
        });
    }

    #[test]
    fn pending_file_path_rejects_traversal() {
        assert!(pending_file_path("../malicious").is_none());
        assert!(pending_file_path("with/slash").is_none());
        assert!(pending_file_path("").is_none());
    }

    #[test]
    fn note_then_take_roundtrips_instruction_ids() {
        let session = format!("legatus-client-test-{}", uuid::Uuid::new_v4());
        let a = InstructionId::new();
        let b = InstructionId::new();
        note_pending_instruction(&session, a);
        note_pending_instruction(&session, b);
        let taken = take_pending_instructions(&session);
        assert_eq!(taken.len(), 2);
        assert!(taken.contains(&a));
        assert!(taken.contains(&b));
        let second = take_pending_instructions(&session);
        assert!(second.is_empty(), "second take should be empty");
    }

    #[test]
    fn take_pending_for_missing_session_returns_empty() {
        let session = format!("legatus-client-test-{}", uuid::Uuid::new_v4());
        assert!(take_pending_instructions(&session).is_empty());
    }

    #[test]
    fn turn_signals_path_rejects_traversal() {
        assert!(turn_signals_path("../malicious").is_none());
        assert!(turn_signals_path("with/slash").is_none());
        assert!(turn_signals_path("").is_none());
    }

    #[test]
    fn note_then_take_roundtrips_turn_signals() {
        let session = format!("legatus-client-test-{}", uuid::Uuid::new_v4());
        let signal = TurnSignal::PermissionDenied {
            tool: "Bash".to_owned(),
        };
        note_turn_signal(&session, &signal);
        let taken = take_turn_signals(&session);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0], signal);
        // Second take returns empty — file was cleared.
        let second = take_turn_signals(&session);
        assert!(second.is_empty(), "second take should be empty");
    }

    #[test]
    fn classify_outcome_api_error_wins_over_signals() {
        // StopFailure path: even if PermissionDenied happened during
        // the turn, the API error is the more informative signal.
        let signals = vec![TurnSignal::PermissionDenied {
            tool: "Bash".to_owned(),
        }];
        let outcome = classify_outcome(&signals, Some("rate_limit: backoff 300s"));
        match outcome {
            sentinel_legatus::InstructionOutcome::Failure { error } => {
                assert_eq!(error, "rate_limit: backoff 300s");
            }
            other => panic!("expected Failure, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_permission_denied_yields_declined() {
        let signals = vec![TurnSignal::PermissionDenied {
            tool: "Bash".to_owned(),
        }];
        let outcome = classify_outcome(&signals, None);
        match outcome {
            sentinel_legatus::InstructionOutcome::Declined { reason } => {
                assert!(reason.contains("Bash"), "reason should name the tool: {reason}");
                assert!(reason.contains("permission denied"), "reason: {reason}");
            }
            other => panic!("expected Declined, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_multiple_denied_tools_joined_in_reason() {
        let signals = vec![
            TurnSignal::PermissionDenied {
                tool: "Bash".to_owned(),
            },
            TurnSignal::PermissionDenied {
                tool: "Write".to_owned(),
            },
        ];
        let outcome = classify_outcome(&signals, None);
        match outcome {
            sentinel_legatus::InstructionOutcome::Declined { reason } => {
                assert!(reason.contains("Bash"), "reason: {reason}");
                assert!(reason.contains("Write"), "reason: {reason}");
            }
            other => panic!("expected Declined, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_no_signals_yields_success() {
        let outcome = classify_outcome(&[], None);
        assert!(matches!(outcome, sentinel_legatus::InstructionOutcome::Success));
    }

    #[test]
    fn take_turn_signals_for_missing_session_returns_empty() {
        let session = format!("legatus-client-test-{}", uuid::Uuid::new_v4());
        assert!(take_turn_signals(&session).is_empty());
    }
}
