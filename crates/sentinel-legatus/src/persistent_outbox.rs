//! File-backed outbox for sentinel→consul escalation events.
//!
//! Symmetric to [`crate::persistent_inbox::PersistentInbox`] (which
//! handles operator→agent direction). This outbox handles the
//! agent→operator direction: every `EscalationKind` event that
//! [`LegatusHandle::escalate`] enqueues lands on disk before the WS
//! recv loop signs+sends it, so a daemon crash between
//! "escalation queued" and "WS bytes written" doesn't lose the event.
//!
//! [`LegatusHandle::escalate`]: crate::handle::LegatusHandle::escalate
//!
//! ## Why escalations need persistence
//!
//! Most escalations are self-correcting (heartbeats fire every 20s,
//! `SessionCompleted` re-fires on the next Stop, etc.). The
//! exception is per-instruction `InstructionResult` — emitted ONCE
//! per cancel via the loopback channel in
//! [`crate::client::handle_inbound`]. If the daemon crashes after
//! `inbox.remove_by_id` succeeds but before the loopback's
//! `InstructionResult { Declined }` reaches consul over the WS,
//! consul never sees the closing record. Its
//! `DispatchedInstructionsLog` entry stays at `cancelled_at = None`
//! forever; the resolution layer keeps surfacing the cancelled
//! instruction as a candidate next time the operator types
//! "cancel". Real bug, fixed by this outbox.
//!
//! ## Delivery semantics
//!
//! At-least-once. On startup the WS recv loop replays any pending
//! disk entries through the same code path as a fresh
//! `escalate(...)` call. If the daemon crashed between "WS send
//! succeeded" and "disk head removed", consul receives a duplicate
//! event on the next daemon start. Consul-side handlers must be
//! idempotent (they already are — every escalation kind is keyed
//! by a stable id: `instruction_id` for `InstructionResult` /
//! `InstructionAcknowledged`, `session_id` for the lifecycle events).
//!
//! ## Storage shape
//!
//! One JSON-encoded `OutboxItem` per line at
//! `~/.claude/sentinel/state/legatus-escalations.jsonl`. An
//! `OutboxItem` wraps the `EscalationKind` with the `at_ms`
//! timestamp captured at append-time, which is reused at envelope-
//! send time so the wire timestamp matches what the outbox stored.
//! That stable per-entry timestamp is what makes lifecycle-key
//! matching (`SessionBlocked { session_id, detected_at_ms }`)
//! possible from the operator-ack side. Same fs2 advisory-lock
//! discipline as `PersistentInbox`. Same atomic-rewrite strategy
//! for `remove_head` (read all lines under lock, skip the first
//! valid entry, rewrite the remainder).
//!
//! **Backwards compat**: pre-refactor on-disk entries are bare
//! `EscalationKind` JSON (no wrapper). [`parse_entry`] tries the
//! new wrapped shape first, then falls back to bare-event with
//! `at_ms = 0`. Pre-refactor lifecycle entries therefore cannot
//! match a fresh ack (timestamp 0 won't equal any real ack
//! timestamp), so they fall through to the `remove_head` cleanup
//! path. Acceptable on a per-machine upgrade.

#![allow(clippy::incompatible_msrv)]

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use consul_domain::identity::InstructionId;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::handle::EscalationKind;

/// Default daemon-global path for the persistent escalation outbox.
///
/// Returns `None` only when `dirs::home_dir()` can't be resolved —
/// the daemon falls back to in-memory-only behavior in that case.
#[must_use]
pub fn default_outbox_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join("legatus-escalations.jsonl"),
    )
}

/// One queued escalation, with the timestamp that was captured at
/// `LegatusHandle::escalate` time and is reused at envelope-send
/// time. The match key for lifecycle-keyed `EscalationAck`s.
///
/// Wire format (one per line): `{"event": {...}, "at_ms": ...}`.
/// Construction via [`OutboxItem::new`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboxItem {
    /// The escalation payload (variant + variant-specific fields).
    pub event: EscalationKind,
    /// Unix-epoch milliseconds at append time. Reused as the
    /// envelope's `*_at_ms` for lifecycle variants and as the
    /// outbox-side half of the `(session_id, *_at_ms)` ack key.
    /// Defaults to `0` when deserializing legacy bare-`EscalationKind`
    /// entries (see crate docs).
    #[serde(default)]
    pub at_ms: u64,
}

impl OutboxItem {
    /// Build an item from an event + timestamp.
    #[must_use]
    pub const fn new(event: EscalationKind, at_ms: u64) -> Self {
        Self { event, at_ms }
    }
}

/// Discriminant for lifecycle-keyed removal. Mirrors the
/// per-variant arms of `EscalationKind` that lack
/// `instruction_id`. Used by [`PersistentEscalationOutbox::remove_lifecycle`]
/// to authorise removal of an entry whose at-ms timestamp matches
/// an inbound `EscalationAck` lifecycle key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleKind {
    /// Matches [`EscalationKind::Blocked`].
    Blocked,
    /// Matches [`EscalationKind::Completed`].
    Completed,
    /// Matches [`EscalationKind::Failed`].
    Failed,
}

/// File-backed FIFO queue of pending escalation events.
///
/// Cloneable — internal state is just the path; every operation
/// re-opens the file under a fresh advisory lock. Safe to clone
/// into spawned tasks.
#[derive(Clone, Debug)]
pub struct PersistentEscalationOutbox {
    path: PathBuf,
}

impl PersistentEscalationOutbox {
    /// Construct over an explicit path. Used by tests + custom
    /// deployments.
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The path this outbox writes to. Public for diagnostics.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // `PathBuf::as_path` is not const.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append an [`OutboxItem`] to the back of the queue. Creates
    /// the parent dir + file on demand. Best-effort: I/O errors
    /// are logged at `warn` so the
    /// [`crate::handle::LegatusHandle::escalate`] caller never
    /// blocks on a transient filesystem issue. The in-memory
    /// mpsc still gets the event regardless of disk-write outcome,
    /// so a successful escalation just degrades to non-durable
    /// rather than failing.
    pub fn append(&self, item: &OutboxItem) {
        if let Some(parent) = self.path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                warn!(?err, ?parent, "persistent_outbox: create_dir_all failed");
                return;
            }
        }
        let line = match serde_json::to_string(item) {
            Ok(s) => s,
            Err(err) => {
                warn!(?err, "persistent_outbox: item serialize failed");
                return;
            }
        };
        // `.read(true)` is REQUIRED alongside `.append(true)`: on
        // Windows an append-only handle has just FILE_APPEND_DATA
        // access, and `fs2`'s `lock_exclusive` (LockFileEx) needs
        // FILE_READ_DATA or FILE_WRITE_DATA — without it the lock
        // silently fails and the early-return below drops the write.
        // We grant READ rather than WRITE because `.write(true)` +
        // `.append(true)` together give the handle a non-append write
        // cursor on Windows, so successive appends overwrite earlier
        // entries instead of extending the file. `.read(true)` satisfies
        // LockFileEx while leaving pure append-at-EOF semantics intact.
        let mut file = match OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)
        {
            Ok(f) => f,
            Err(err) => {
                warn!(?err, path = ?self.path, "persistent_outbox: open(append) failed");
                return;
            }
        };
        if let Err(err) = file.lock_exclusive() {
            warn!(?err, "persistent_outbox: lock_exclusive failed");
            return;
        }
        let result = writeln!(file, "{line}").and_then(|()| file.flush());
        let _ = file.unlock();
        if let Err(err) = result {
            warn!(?err, "persistent_outbox: append write failed");
        }
    }

    /// Remove the oldest queued event. Called by the WS recv loop
    /// after a successful `send_signed` of that event. Returns
    /// `true` if a head entry was removed, `false` on empty queue
    /// / missing file / lock failure.
    ///
    /// Same atomic-rewrite discipline as `PersistentInbox::try_pop`:
    /// open RW under exclusive lock, read all lines, drop the
    /// first valid one, rewrite the remainder. Concurrent
    /// `append`s serialize on the same lock.
    pub fn remove_head(&self) -> bool {
        let file = match OpenOptions::new().read(true).write(true).open(&self.path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
            Err(err) => {
                warn!(?err, path = ?self.path, "persistent_outbox: open(rw) failed");
                return false;
            }
        };
        if let Err(err) = file.lock_exclusive() {
            warn!(?err, "persistent_outbox: lock_exclusive failed");
            return false;
        }
        let removed = remove_head_under_lock(&file);
        let _ = file.unlock();
        removed
    }

    /// Remove the first queued event matching `instruction_id`,
    /// regardless of its position in the FIFO. Used by the
    /// inbound-`EscalationAck` handler in
    /// [`crate::client::handle_inbound`]: when consul confirms
    /// processing of an `InstructionAck` / `InstructionResult`
    /// event, the matching outbox entry can be removed
    /// regardless of whether `remove_head` already fired on the
    /// post-`send_signed` path.
    ///
    /// Returns `true` when a matching entry was found and
    /// removed; `false` on empty queue / missing file / lock
    /// failure / no match.
    ///
    /// **Scope**: matches only the per-instruction variants
    /// (`EscalationKind::InstructionAck`,
    /// `EscalationKind::InstructionResult`). Lifecycle variants
    /// (`Blocked` / `Completed` / `Failed`) don't carry an
    /// `instruction_id` and are skipped by this method —
    /// removing them by `(session_id, *_at_ms)` key requires
    /// storing the sent-time timestamp on disk too, which is
    /// the next refactor in this arc.
    pub fn remove_by_instruction_id(&self, instruction_id: InstructionId) -> bool {
        let file = match OpenOptions::new().read(true).write(true).open(&self.path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
            Err(err) => {
                warn!(?err, path = ?self.path, "persistent_outbox: open(rw) failed");
                return false;
            },
        };
        if let Err(err) = file.lock_exclusive() {
            warn!(?err, "persistent_outbox: lock_exclusive failed");
            return false;
        }
        let removed = remove_by_instruction_id_under_lock(&file, instruction_id);
        let _ = file.unlock();
        removed
    }

    /// Read all queued items without removing them. Called on
    /// daemon startup to replay pending escalations through the
    /// mpsc channel before entering the select loop.
    pub fn snapshot(&self) -> Vec<OutboxItem> {
        let file = match OpenOptions::new().read(true).open(&self.path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                warn!(?err, path = ?self.path, "persistent_outbox: open(read) failed");
                return Vec::new();
            }
        };
        if let Err(err) = file.lock_shared() {
            warn!(?err, "persistent_outbox: lock_shared failed");
            return Vec::new();
        }
        let out: Vec<OutboxItem> = BufReader::new(&file)
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| parse_entry(&l))
            .collect();
        let _ = file.unlock();
        out
    }

    /// Remove the first queued entry whose `(LifecycleKind, at_ms)`
    /// pair matches. Used by the inbound-`EscalationAck` handler
    /// when consul confirms processing of a `SessionBlocked` /
    /// `SessionCompleted` / `SessionFailed` event keyed by
    /// `(session_id, *_at_ms)`.
    ///
    /// Per-instruction variants (`InstructionAck` /
    /// `InstructionResult`) are NOT matched by this method — use
    /// [`Self::remove_by_instruction_id`] for those.
    ///
    /// Returns `true` when a matching entry was found + removed;
    /// `false` on empty queue / missing file / lock failure /
    /// no match.
    pub fn remove_lifecycle(&self, kind: LifecycleKind, at_ms: u64) -> bool {
        let file = match OpenOptions::new().read(true).write(true).open(&self.path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
            Err(err) => {
                warn!(?err, path = ?self.path, "persistent_outbox: open(rw) failed");
                return false;
            },
        };
        if let Err(err) = file.lock_exclusive() {
            warn!(?err, "persistent_outbox: lock_exclusive failed");
            return false;
        }
        let removed = remove_lifecycle_under_lock(&file, kind, at_ms);
        let _ = file.unlock();
        removed
    }

    /// Number of queued events. Counts only lines that parse
    /// successfully. Diagnostic helper.
    #[must_use]
    pub fn len(&self) -> usize {
        self.snapshot().len()
    }

    /// True when the queue is empty (or the backing file doesn't
    /// exist yet).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Atomic-rewrite shape mirroring `remove_head_under_lock`, but
/// filters by `instruction_id` instead of "first valid line".
/// Matches the first entry whose `EscalationKind` carries that
/// id (the two variants that do are `InstructionAck` and
/// `InstructionResult`).
fn remove_by_instruction_id_under_lock(
    mut file: &File,
    instruction_id: InstructionId,
) -> bool {
    let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();

    let mut removed = false;
    let mut remainder: Vec<String> = Vec::with_capacity(lines.len());

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        if removed {
            remainder.push(line);
            continue;
        }
        match parse_entry(&line) {
            Some(item) if event_matches_instruction_id(&item.event, instruction_id) => {
                removed = true;
            },
            Some(_) => remainder.push(line),
            None => {
                warn!(line = %line, "persistent_outbox: dropping malformed line during remove_by_instruction_id");
            },
        }
    }

    if let Err(err) = file.seek(SeekFrom::Start(0)) {
        warn!(?err, "persistent_outbox: seek(0) failed during remove_by_instruction_id");
        return removed;
    }
    if let Err(err) = file.set_len(0) {
        warn!(?err, "persistent_outbox: set_len(0) failed during remove_by_instruction_id");
        return removed;
    }
    for line in &remainder {
        if let Err(err) = writeln!(file, "{line}") {
            warn!(?err, "persistent_outbox: remainder writeln failed during remove_by_instruction_id");
            return removed;
        }
    }
    removed
}

fn event_matches_instruction_id(event: &EscalationKind, target: InstructionId) -> bool {
    match event {
        EscalationKind::InstructionAck { instruction_id }
        | EscalationKind::InstructionResult { instruction_id, .. } => *instruction_id == target,
        EscalationKind::Blocked { .. }
        | EscalationKind::Completed { .. }
        | EscalationKind::Failed { .. } => false,
    }
}

/// Atomic-rewrite shape for lifecycle-key removal. Matches the
/// first entry whose `(variant, at_ms)` pair equals the input.
fn remove_lifecycle_under_lock(mut file: &File, kind: LifecycleKind, at_ms: u64) -> bool {
    let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();

    let mut removed = false;
    let mut remainder: Vec<String> = Vec::with_capacity(lines.len());

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        if removed {
            remainder.push(line);
            continue;
        }
        match parse_entry(&line) {
            Some(item) if item.at_ms == at_ms && event_matches_lifecycle(&item.event, kind) => {
                removed = true;
            },
            Some(_) => remainder.push(line),
            None => {
                warn!(line = %line, "persistent_outbox: dropping malformed line during remove_lifecycle");
            },
        }
    }

    if let Err(err) = file.seek(SeekFrom::Start(0)) {
        warn!(?err, "persistent_outbox: seek(0) failed during remove_lifecycle");
        return removed;
    }
    if let Err(err) = file.set_len(0) {
        warn!(?err, "persistent_outbox: set_len(0) failed during remove_lifecycle");
        return removed;
    }
    for line in &remainder {
        if let Err(err) = writeln!(file, "{line}") {
            warn!(?err, "persistent_outbox: remainder writeln failed during remove_lifecycle");
            return removed;
        }
    }
    removed
}

fn event_matches_lifecycle(event: &EscalationKind, kind: LifecycleKind) -> bool {
    matches!(
        (event, kind),
        (EscalationKind::Blocked { .. }, LifecycleKind::Blocked)
            | (EscalationKind::Completed { .. }, LifecycleKind::Completed)
            | (EscalationKind::Failed { .. }, LifecycleKind::Failed)
    )
}

fn remove_head_under_lock(mut file: &File) -> bool {
    let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();

    let mut removed = false;
    let mut remainder: Vec<String> = Vec::with_capacity(lines.len());

    for line in lines {
        if removed {
            remainder.push(line);
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        match parse_entry(&line) {
            Some(_) => removed = true,
            None => {
                warn!(line = %line, "persistent_outbox: dropping malformed line");
            }
        }
    }

    if let Err(err) = file.seek(SeekFrom::Start(0)) {
        warn!(?err, "persistent_outbox: seek(0) failed during remove_head");
        return removed;
    }
    if let Err(err) = file.set_len(0) {
        warn!(
            ?err,
            "persistent_outbox: set_len(0) failed during remove_head"
        );
        return removed;
    }
    for line in &remainder {
        if let Err(err) = writeln!(file, "{line}") {
            warn!(?err, "persistent_outbox: remainder writeln failed");
            return removed;
        }
    }
    removed
}

/// Parse one disk line into an `OutboxItem`. Tries the new
/// wrapped shape first; falls back to a bare `EscalationKind`
/// (legacy pre-refactor entry) with `at_ms = 0`. Returns `None`
/// only when neither shape parses — the line is then logged
/// and dropped by the caller.
fn parse_entry(line: &str) -> Option<OutboxItem> {
    if let Ok(item) = serde_json::from_str::<OutboxItem>(line) {
        return Some(item);
    }
    if let Ok(event) = serde_json::from_str::<EscalationKind>(line) {
        return Some(OutboxItem { event, at_ms: 0 });
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use consul_domain::identity::InstructionId;
    use consul_protocol::messages::InstructionOutcome;
    use tempfile::tempdir;

    use super::*;

    fn outbox_at(dir: &tempfile::TempDir) -> PersistentEscalationOutbox {
        PersistentEscalationOutbox::new(dir.path().join("escalations.jsonl"))
    }

    /// Pre-refactor tests appended bare `EscalationKind`. The
    /// helpers now wrap with a deterministic `at_ms = 0` since
    /// instruction-id-keyed cases don't read the timestamp.
    /// Lifecycle tests pass an explicit `at_ms` instead.
    fn ack(id: InstructionId) -> OutboxItem {
        OutboxItem::new(EscalationKind::InstructionAck { instruction_id: id }, 0)
    }

    fn declined(id: InstructionId, reason: &str) -> OutboxItem {
        OutboxItem::new(
            EscalationKind::InstructionResult {
                instruction_id: id,
                outcome: InstructionOutcome::Declined {
                    reason: reason.into(),
                },
                summary: None,
            },
            0,
        )
    }

    fn lifecycle_completed(summary: &str, at_ms: u64) -> OutboxItem {
        OutboxItem::new(
            EscalationKind::Completed {
                summary: Some(summary.into()),
            },
            at_ms,
        )
    }

    fn lifecycle_blocked(at_ms: u64) -> OutboxItem {
        OutboxItem::new(
            EscalationKind::Blocked {
                reason: consul_protocol::messages::BlockReason::PermissionDenied {
                    tool: "Bash".into(),
                },
            },
            at_ms,
        )
    }

    fn lifecycle_failed(error: &str, at_ms: u64) -> OutboxItem {
        OutboxItem::new(
            EscalationKind::Failed {
                error: error.into(),
            },
            at_ms,
        )
    }

    #[test]
    fn empty_outbox_is_empty() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        assert!(outbox.is_empty());
        assert_eq!(outbox.len(), 0);
        assert!(outbox.snapshot().is_empty());
    }

    #[test]
    fn append_then_snapshot_returns_event() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        let id = InstructionId::new();
        outbox.append(&ack(id));
        let snap = outbox.snapshot();
        assert_eq!(snap.len(), 1);
        match &snap[0].event {
            EscalationKind::InstructionAck { instruction_id } => {
                assert_eq!(*instruction_id, id);
            }
            other => panic!("expected InstructionAck, got {other:?}"),
        }
    }

    #[test]
    fn fifo_order_preserved_across_appends() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        let a = InstructionId::new();
        let b = InstructionId::new();
        let c = InstructionId::new();
        outbox.append(&ack(a));
        outbox.append(&ack(b));
        outbox.append(&ack(c));
        let snap = outbox.snapshot();
        assert_eq!(snap.len(), 3);
        for (i, expected) in [a, b, c].iter().enumerate() {
            match &snap[i].event {
                EscalationKind::InstructionAck { instruction_id } => {
                    assert_eq!(instruction_id, expected);
                }
                other => panic!("expected InstructionAck at {i}, got {other:?}"),
            }
        }
    }

    #[test]
    fn remove_head_strips_oldest_first() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        let a = InstructionId::new();
        let b = InstructionId::new();
        outbox.append(&ack(a));
        outbox.append(&ack(b));

        assert!(outbox.remove_head());
        let snap = outbox.snapshot();
        assert_eq!(snap.len(), 1);
        match &snap[0].event {
            EscalationKind::InstructionAck { instruction_id } => {
                assert_eq!(*instruction_id, b);
            }
            other => panic!("expected InstructionAck for b, got {other:?}"),
        }

        assert!(outbox.remove_head());
        assert!(outbox.is_empty());

        // Further remove_head on empty queue returns false.
        assert!(!outbox.remove_head());
    }

    #[test]
    fn remove_head_on_missing_file_returns_false() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        assert!(!outbox.remove_head());
    }

    #[test]
    fn append_after_remove_head_extends_remainder() {
        // Models the loop's send-then-append pattern under load.
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        let first = InstructionId::new();
        let second = InstructionId::new();
        let third = InstructionId::new();
        outbox.append(&ack(first));
        outbox.append(&ack(second));
        assert!(outbox.remove_head()); // removes first
        outbox.append(&ack(third));

        let snap = outbox.snapshot();
        assert_eq!(snap.len(), 2);
        match (&snap[0].event, &snap[1].event) {
            (
                EscalationKind::InstructionAck {
                    instruction_id: head,
                },
                EscalationKind::InstructionAck {
                    instruction_id: tail,
                },
            ) => {
                assert_eq!(*head, second);
                assert_eq!(*tail, third);
            }
            other => panic!("expected two InstructionAcks, got {other:?}"),
        }
    }

    #[test]
    fn malformed_lines_dropped_on_remove_head() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        let path = outbox.path().to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Pre-seed with malformed line then valid event.
        let id = InstructionId::new();
        let good = ack(id);
        let good_json = serde_json::to_string(&good).unwrap();
        std::fs::write(&path, format!("not-valid-json\n{good_json}\n")).unwrap();

        // remove_head drops the malformed line + the valid one
        // (counts the good one as "removed").
        assert!(outbox.remove_head());
        assert!(outbox.is_empty());
    }

    #[test]
    fn declined_outcome_roundtrips_through_disk() {
        // Pin the headline use case: InstructionResult { Declined }
        // from the cancel loopback must survive disk roundtrip.
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        let id = InstructionId::new();
        outbox.append(&declined(id, "cancelled by operator: rollback"));
        let snap = outbox.snapshot();
        assert_eq!(snap.len(), 1);
        match &snap[0].event {
            EscalationKind::InstructionResult {
                instruction_id,
                outcome,
                summary,
            } => {
                assert_eq!(*instruction_id, id);
                assert!(summary.is_none());
                match outcome {
                    InstructionOutcome::Declined { reason } => {
                        assert_eq!(reason, "cancelled by operator: rollback");
                    }
                    other => panic!("expected Declined, got {other:?}"),
                }
            }
            other => panic!("expected InstructionResult, got {other:?}"),
        }
    }

    #[test]
    fn events_survive_simulated_daemon_restart() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("escalations.jsonl");
        let id = InstructionId::new();
        let first = PersistentEscalationOutbox::new(path.clone());
        first.append(&ack(id));
        drop(first);

        let second = PersistentEscalationOutbox::new(path);
        let snap = second.snapshot();
        assert_eq!(snap.len(), 1);
        match &snap[0].event {
            EscalationKind::InstructionAck { instruction_id } => {
                assert_eq!(*instruction_id, id);
            }
            other => panic!("expected InstructionAck, got {other:?}"),
        }
    }

    #[test]
    fn concurrent_appends_all_persist() {
        let dir = tempdir().unwrap();
        let outbox = Arc::new(outbox_at(&dir));
        let mut handles = Vec::new();
        for _ in 0..20 {
            let outbox = Arc::clone(&outbox);
            handles.push(std::thread::spawn(move || {
                outbox.append(&ack(InstructionId::new()));
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(outbox.len(), 20);
    }

    #[test]
    fn default_outbox_path_when_home_resolvable() {
        let Some(p) = default_outbox_path() else {
            return;
        };
        assert!(p.ends_with("legatus-escalations.jsonl"));
        assert!(p.to_string_lossy().contains(".claude"));
        assert!(p.to_string_lossy().contains("sentinel"));
    }

    // ----- remove_by_instruction_id ----------------------------------------

    #[test]
    fn remove_by_instruction_id_removes_matching_ack_entry() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        let a = InstructionId::new();
        let b = InstructionId::new();
        outbox.append(&ack(a));
        outbox.append(&ack(b));

        // Remove b first (not the head — verifies non-FIFO removal).
        assert!(outbox.remove_by_instruction_id(b));
        let snap = outbox.snapshot();
        assert_eq!(snap.len(), 1);
        match &snap[0].event {
            EscalationKind::InstructionAck { instruction_id } => {
                assert_eq!(*instruction_id, a);
            },
            other => panic!("expected InstructionAck for a, got {other:?}"),
        }
    }

    #[test]
    fn remove_by_instruction_id_removes_matching_result_entry() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        let id = InstructionId::new();
        outbox.append(&declined(id, "operator rolled back"));

        assert!(outbox.remove_by_instruction_id(id));
        assert!(outbox.is_empty());
    }

    #[test]
    fn remove_by_instruction_id_no_match_returns_false() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        outbox.append(&ack(InstructionId::new()));
        // Different id → no match.
        assert!(!outbox.remove_by_instruction_id(InstructionId::new()));
        // Queue is unchanged.
        assert_eq!(outbox.len(), 1);
    }

    #[test]
    fn remove_by_instruction_id_on_missing_file_returns_false() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        assert!(!outbox.remove_by_instruction_id(InstructionId::new()));
    }

    #[test]
    fn remove_by_instruction_id_skips_lifecycle_variants() {
        // Lifecycle variants (Blocked/Completed/Failed) don't
        // carry instruction_id — they're not matched by this
        // method. They have their own remove_lifecycle path.
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        outbox.append(&lifecycle_completed("done", 1000));
        // Any instruction id → no match because no entry carries one.
        assert!(!outbox.remove_by_instruction_id(InstructionId::new()));
        assert_eq!(outbox.len(), 1, "lifecycle entry should not be touched");
    }

    #[test]
    fn remove_by_instruction_id_only_removes_first_match() {
        // Defensive: if a duplicate entry ever lands on disk (replay
        // edge case), the remove should drop one occurrence not all.
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        let id = InstructionId::new();
        outbox.append(&ack(id));
        outbox.append(&ack(id));

        assert!(outbox.remove_by_instruction_id(id));
        assert_eq!(outbox.len(), 1, "second copy should remain");
        assert!(outbox.remove_by_instruction_id(id));
        assert!(outbox.is_empty());
    }

    // ----- remove_lifecycle ------------------------------------------------

    #[test]
    fn remove_lifecycle_matches_blocked_by_at_ms() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        outbox.append(&lifecycle_blocked(1_000));
        outbox.append(&lifecycle_blocked(2_000));

        // Remove the second one — verifies non-FIFO removal by key.
        assert!(outbox.remove_lifecycle(LifecycleKind::Blocked, 2_000));
        let snap = outbox.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].at_ms, 1_000);
    }

    #[test]
    fn remove_lifecycle_matches_completed() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        outbox.append(&lifecycle_completed("done", 5_000));
        assert!(outbox.remove_lifecycle(LifecycleKind::Completed, 5_000));
        assert!(outbox.is_empty());
    }

    #[test]
    fn remove_lifecycle_matches_failed() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        outbox.append(&lifecycle_failed("boom", 7_000));
        assert!(outbox.remove_lifecycle(LifecycleKind::Failed, 7_000));
        assert!(outbox.is_empty());
    }

    #[test]
    fn remove_lifecycle_no_match_returns_false() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        outbox.append(&lifecycle_completed("done", 1_000));
        // Wrong at_ms.
        assert!(!outbox.remove_lifecycle(LifecycleKind::Completed, 2_000));
        // Wrong variant.
        assert!(!outbox.remove_lifecycle(LifecycleKind::Blocked, 1_000));
        assert_eq!(outbox.len(), 1, "neither failed match should remove");
    }

    #[test]
    fn remove_lifecycle_skips_per_instruction_variants() {
        // Per-instruction entries (no lifecycle variant) must be
        // unaffected by remove_lifecycle even when at_ms accidentally
        // collides with the instruction's own at_ms.
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        outbox.append(&OutboxItem::new(
            EscalationKind::InstructionAck {
                instruction_id: InstructionId::new(),
            },
            42,
        ));
        assert!(!outbox.remove_lifecycle(LifecycleKind::Completed, 42));
        assert_eq!(outbox.len(), 1);
    }

    #[test]
    fn remove_lifecycle_on_missing_file_returns_false() {
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        assert!(!outbox.remove_lifecycle(LifecycleKind::Completed, 1_000));
    }

    // ----- legacy on-disk shape (pre-refactor entries) --------------------

    #[test]
    fn legacy_bare_escalation_kind_lines_parse_with_at_ms_zero() {
        // Pre-refactor on-disk shape was `EscalationKind` JSON
        // directly. parse_entry must accept those and synthesize
        // `at_ms = 0` so a post-upgrade daemon still drains the
        // pending queue (matching is best-effort: lifecycle entries
        // with at_ms=0 won't match a fresh ack timestamp, so they
        // fall through to remove_head only).
        let dir = tempdir().unwrap();
        let outbox = outbox_at(&dir);
        let path = outbox.path().to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = EscalationKind::Completed {
            summary: Some("legacy".into()),
        };
        let legacy_json = serde_json::to_string(&legacy).unwrap();
        std::fs::write(&path, format!("{legacy_json}\n")).unwrap();

        let snap = outbox.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].at_ms, 0);
        match &snap[0].event {
            EscalationKind::Completed { summary } => assert_eq!(summary.as_deref(), Some("legacy")),
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}
