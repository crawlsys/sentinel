//! File-backed inbox for operator-relayed `RelayInstruction`s.
//!
//! Replaces the in-memory `tokio::sync::mpsc` channel that the
//! pre-persistence handle used to buffer instructions between the
//! WS recv loop (producer) and the daemon's
//! `GET /legatus/inbox/next` HTTP route (consumer). With the
//! channel, a daemon crash between "received over WS" and
//! "drained by hook" lost the instruction. With this file, the
//! queue survives daemon restart: the instruction is on disk
//! before consul gets an `InstructionAck`, and the hook pops it
//! from disk whenever it's next ready.
//!
//! Storage: one JSON-encoded `RelayInstruction` per line, in
//! arrival order. Path defaults to
//! `~/.claude/sentinel/state/legatus-inbox.jsonl` (daemon-global —
//! one daemon hosts one legatus today; per-Claude-Code-session
//! files would only matter under future multi-session hosting).
//!
//! Race-safety: every operation takes an `fs2` advisory exclusive
//! lock on the inbox file before reading/writing. Append-and-flush
//! is one critical section; read-and-rewrite is another. The lock
//! covers concurrent producers (WS recv loops if we ever spawn
//! more than one) and the consumer (HTTP route) on the same
//! daemon. Different processes touching the same file are serialized
//! by the advisory lock too.

// fs2's FileExt methods (lock_exclusive, lock_shared, unlock) share
// names with std::fs::File inherent methods stabilized in Rust 1.89;
// clippy flags them as incompatible with our 1.83 MSRV even though
// fs2's trait impl is what actually compiles. The crate is on our
// dependency list intentionally for portability — silence the lint
// at the module level.
#![allow(clippy::incompatible_msrv)]

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use consul_domain::identity::InstructionId;
use consul_protocol::messages::RelayInstruction;
use fs2::FileExt;
use tracing::warn;

/// Default daemon-global path for the persistent inbox.
///
/// Returns `None` only when `dirs::home_dir()` can't be resolved,
/// which on a sane host should never happen. The daemon falls
/// back to in-memory-only behavior in that case.
#[must_use]
pub fn default_inbox_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join("legatus-inbox.jsonl"),
    )
}

/// File-backed FIFO queue of `RelayInstruction`s.
///
/// Cloneable — internal state is just the path; every operation
/// re-opens the file under a fresh advisory lock. This makes the
/// inbox safe to clone into spawned tasks without worrying about
/// shared `File` handles.
#[derive(Clone, Debug)]
pub struct PersistentInbox {
    path: PathBuf,
}

impl PersistentInbox {
    /// Construct over an explicit path. Used by tests + custom
    /// deployments.
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The path this inbox writes to. Public for diagnostics.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // PathBuf::as_path is not const.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a `RelayInstruction` to the back of the queue.
    /// Creates the parent dir + file on demand. Best-effort:
    /// I/O errors are logged at `warn` so the WS loop never
    /// blocks on a transient filesystem issue. (Consul will
    /// re-send if the operator re-instructs; persistence is a
    /// reliability improvement, not a correctness gate.)
    pub fn append(&self, instr: &RelayInstruction) {
        if let Some(parent) = self.path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                warn!(?err, ?parent, "persistent_inbox: create_dir_all failed");
                return;
            }
        }
        let line = match serde_json::to_string(instr) {
            Ok(s) => s,
            Err(err) => {
                warn!(?err, "persistent_inbox: instruction serialize failed");
                return;
            }
        };
        let mut file = match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(f) => f,
            Err(err) => {
                warn!(?err, path = ?self.path, "persistent_inbox: open(append) failed");
                return;
            }
        };
        if let Err(err) = file.lock_exclusive() {
            warn!(?err, "persistent_inbox: lock_exclusive failed");
            return;
        }
        let result = writeln!(file, "{line}");
        // Drop the lock by dropping `file` at end of scope; we
        // also explicitly unlock to be defensive across rustc
        // versions that may keep the handle alive longer.
        let _ = file.unlock();
        if let Err(err) = result {
            warn!(?err, "persistent_inbox: append write failed");
        }
    }

    /// Pop the oldest queued instruction. Returns `None` if the
    /// file is missing or empty.
    ///
    /// Atomic-rewrite strategy: read all lines under lock, skip
    /// the first valid one as the "popped" item, rewrite the
    /// remainder. A concurrent `append` during the read+rewrite
    /// window is serialized by the advisory lock — appends block
    /// until the rewrite completes.
    ///
    /// Lines that fail to parse as `RelayInstruction` are
    /// silently dropped (logged at `warn`). They typically come
    /// from a schema change in `consul-protocol` — the consumer
    /// is the authoritative format owner.
    pub fn try_pop(&self) -> Option<RelayInstruction> {
        let file = match OpenOptions::new().read(true).write(true).open(&self.path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                warn!(?err, path = ?self.path, "persistent_inbox: open(rw) failed");
                return None;
            }
        };
        if let Err(err) = file.lock_exclusive() {
            warn!(?err, "persistent_inbox: lock_exclusive failed");
            return None;
        }
        let popped = pop_first_under_lock(&file);
        let _ = file.unlock();
        popped
    }

    /// Read all queued instructions without removing them. Used
    /// for diagnostics + on-startup rehydration of in-memory
    /// caches (if any layer wants one).
    ///
    /// Parse failures are dropped silently.
    pub fn snapshot(&self) -> Vec<RelayInstruction> {
        let file = match OpenOptions::new().read(true).open(&self.path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                warn!(?err, path = ?self.path, "persistent_inbox: open(read) failed");
                return Vec::new();
            }
        };
        if let Err(err) = file.lock_shared() {
            warn!(?err, "persistent_inbox: lock_shared failed");
            return Vec::new();
        }
        let out: Vec<RelayInstruction> = BufReader::new(&file)
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<RelayInstruction>(&l).ok())
            .collect();
        let _ = file.unlock();
        out
    }

    /// Remove the queued instruction with the given id, if present.
    /// Returns `true` if a match was found and removed, `false`
    /// otherwise (queue empty, file missing, or no matching id).
    ///
    /// Used by the cancel path: when consul sends a
    /// `CancelInstruction`, legatus calls this to drop the
    /// still-queued instruction. The cancel is definitive in the
    /// `true` case; if `false`, either the instruction was already
    /// drained into the model's context window (too late) or never
    /// arrived (rare protocol race).
    ///
    /// Same atomic-rewrite + advisory-lock discipline as
    /// [`Self::try_pop`]: read all lines under exclusive lock,
    /// filter out the matching id, rewrite the remainder.
    /// Concurrent appends serialize on the lock.
    pub fn remove_by_id(&self, instruction_id: InstructionId) -> bool {
        let file = match OpenOptions::new().read(true).write(true).open(&self.path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
            Err(err) => {
                warn!(?err, path = ?self.path, "persistent_inbox: open(rw) failed");
                return false;
            },
        };
        if let Err(err) = file.lock_exclusive() {
            warn!(?err, "persistent_inbox: lock_exclusive failed");
            return false;
        }
        let removed = remove_by_id_under_lock(&file, instruction_id);
        let _ = file.unlock();
        removed
    }

    /// Number of queued instructions (counts only lines that parse
    /// successfully). Diagnostic helper.
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

/// Single-shot read-skip-rewrite, run with the file already
/// exclusively locked. Returns the popped instruction (or `None`
/// if the file is empty / all lines are malformed).
/// Single-shot read-filter-rewrite for [`PersistentInbox::remove_by_id`],
/// run with the file already exclusively locked. Returns true if a
/// matching id was found and removed. Mirrors `pop_first_under_lock`'s
/// rewrite discipline (`seek(0)` + `set_len(0)` + `writeln` remainder).
fn remove_by_id_under_lock(mut file: &File, instruction_id: InstructionId) -> bool {
    let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();

    let mut removed = false;
    let mut remainder: Vec<String> = Vec::with_capacity(lines.len());

    for line in lines {
        if line.trim().is_empty() {
            continue; // skip blank lines silently
        }
        match serde_json::from_str::<RelayInstruction>(&line) {
            Ok(instr) if !removed && instr.instruction_id == instruction_id => {
                // Found the cancellation target — skip it on rewrite.
                removed = true;
            },
            Ok(_) => {
                remainder.push(line);
            },
            Err(err) => {
                warn!(?err, "persistent_inbox: dropping malformed line during cancel");
                // Don't put malformed lines back — they'd just
                // re-trigger the same failure forever.
            },
        }
    }

    if let Err(err) = file.seek(SeekFrom::Start(0)) {
        warn!(?err, "persistent_inbox: seek(0) failed during cancel");
        return removed;
    }
    if let Err(err) = file.set_len(0) {
        warn!(?err, "persistent_inbox: set_len(0) failed during cancel");
        return removed;
    }
    for line in &remainder {
        if let Err(err) = writeln!(file, "{line}") {
            warn!(?err, "persistent_inbox: remainder writeln failed during cancel");
            return removed;
        }
    }
    removed
}

fn pop_first_under_lock(mut file: &File) -> Option<RelayInstruction> {
    let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();

    let mut popped: Option<RelayInstruction> = None;
    let mut remainder: Vec<String> = Vec::with_capacity(lines.len());

    for line in lines {
        if popped.is_some() {
            remainder.push(line);
            continue;
        }
        if line.trim().is_empty() {
            continue; // skip blank lines silently
        }
        match serde_json::from_str::<RelayInstruction>(&line) {
            Ok(instr) => popped = Some(instr),
            Err(err) => {
                warn!(?err, "persistent_inbox: dropping malformed line");
                // Don't put malformed lines back — they'd just
                // re-trigger the same failure forever.
            }
        }
    }

    // Rewrite the remainder. We truncate the file to zero and
    // write back. Doing it in-place (without truncate) risks
    // leaving a stale tail when remainder is shorter than the
    // previous content.
    if let Err(err) = file.seek(SeekFrom::Start(0)) {
        warn!(?err, "persistent_inbox: seek(0) failed during pop");
        return popped;
    }
    if let Err(err) = file.set_len(0) {
        warn!(?err, "persistent_inbox: set_len(0) failed during pop");
        return popped;
    }
    for line in &remainder {
        if let Err(err) = writeln!(file, "{line}") {
            warn!(?err, "persistent_inbox: remainder writeln failed");
            return popped;
        }
    }
    popped
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use consul_domain::identity::{InstructionId, SessionId};
    use consul_protocol::messages::RelayInstruction;
    use tempfile::tempdir;

    use super::*;

    fn instr(content: &str) -> RelayInstruction {
        RelayInstruction {
            instruction_id: InstructionId::new(),
            target_session_id: SessionId::new_v7(),
            content: content.into(),
            destructive: false,
        }
    }

    fn inbox_at(dir: &tempfile::TempDir) -> PersistentInbox {
        PersistentInbox::new(dir.path().join("legatus-inbox.jsonl"))
    }

    #[test]
    fn empty_inbox_pops_none() {
        let dir = tempdir().unwrap();
        let inbox = inbox_at(&dir);
        assert!(inbox.try_pop().is_none());
        assert!(inbox.is_empty());
        assert_eq!(inbox.len(), 0);
    }

    #[test]
    fn append_then_pop_returns_same_instruction() {
        let dir = tempdir().unwrap();
        let inbox = inbox_at(&dir);
        let a = instr("first");
        inbox.append(&a);
        let popped = inbox.try_pop().unwrap();
        assert_eq!(popped.instruction_id, a.instruction_id);
        assert_eq!(popped.content, "first");
        assert!(inbox.try_pop().is_none());
    }

    #[test]
    fn fifo_order_preserved_across_appends_and_pops() {
        let dir = tempdir().unwrap();
        let inbox = inbox_at(&dir);
        let a = instr("first");
        let b = instr("second");
        let c = instr("third");
        inbox.append(&a);
        inbox.append(&b);
        inbox.append(&c);
        assert_eq!(inbox.len(), 3);
        assert_eq!(inbox.try_pop().unwrap().content, "first");
        assert_eq!(inbox.try_pop().unwrap().content, "second");
        assert_eq!(inbox.try_pop().unwrap().content, "third");
        assert!(inbox.try_pop().is_none());
    }

    #[test]
    fn append_after_pop_extends_remainder() {
        // Models the WS-recv-then-hook-drain-then-WS-recv pattern.
        let dir = tempdir().unwrap();
        let inbox = inbox_at(&dir);
        inbox.append(&instr("first"));
        inbox.append(&instr("second"));
        assert_eq!(inbox.try_pop().unwrap().content, "first");
        inbox.append(&instr("third"));
        assert_eq!(inbox.try_pop().unwrap().content, "second");
        assert_eq!(inbox.try_pop().unwrap().content, "third");
        assert!(inbox.is_empty());
    }

    #[test]
    fn snapshot_does_not_consume() {
        let dir = tempdir().unwrap();
        let inbox = inbox_at(&dir);
        inbox.append(&instr("a"));
        inbox.append(&instr("b"));
        let snap = inbox.snapshot();
        assert_eq!(snap.len(), 2);
        // Still poppable after snapshot.
        assert_eq!(inbox.try_pop().unwrap().content, "a");
    }

    #[test]
    fn malformed_lines_are_dropped_on_pop() {
        let dir = tempdir().unwrap();
        let inbox = inbox_at(&dir);
        let path = inbox.path().to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Pre-seed the file with a malformed line, then a valid
        // instruction. pop should skip the bad line and return
        // the good one.
        let good = instr("good");
        let good_json = serde_json::to_string(&good).unwrap();
        std::fs::write(&path, format!("not-valid-json\n{good_json}\n")).unwrap();
        let popped = inbox.try_pop().unwrap();
        assert_eq!(popped.content, "good");
    }

    #[test]
    fn instructions_survive_simulated_daemon_restart() {
        // Append, then create a new PersistentInbox over the same
        // path — that simulates the daemon dropping its in-memory
        // state but the file remaining on disk. The new inbox
        // should pop the queued instruction.
        let dir = tempdir().unwrap();
        let path = dir.path().join("legatus-inbox.jsonl");
        let first_inbox = PersistentInbox::new(path.clone());
        first_inbox.append(&instr("survived"));
        drop(first_inbox);

        let second_inbox = PersistentInbox::new(path);
        assert_eq!(second_inbox.len(), 1);
        let popped = second_inbox.try_pop().unwrap();
        assert_eq!(popped.content, "survived");
    }

    #[test]
    fn concurrent_appends_from_multiple_threads_all_persist() {
        let dir = tempdir().unwrap();
        let inbox = Arc::new(inbox_at(&dir));
        let mut handles = Vec::new();
        for i in 0..20 {
            let inbox = Arc::clone(&inbox);
            handles.push(std::thread::spawn(move || {
                inbox.append(&instr(&format!("msg-{i}")));
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // All 20 should be on disk after the lock-protected appends.
        assert_eq!(inbox.len(), 20);
    }

    #[test]
    fn default_inbox_path_when_home_resolvable() {
        // Sanity — on a normal dev box this resolves; we just
        // confirm the suffix shape.
        let Some(p) = default_inbox_path() else {
            return;
        };
        assert!(p.ends_with("legatus-inbox.jsonl"));
        assert!(p.to_string_lossy().contains(".claude"));
        assert!(p.to_string_lossy().contains("sentinel"));
    }

    #[test]
    fn remove_by_id_on_missing_file_returns_false() {
        let dir = tempdir().unwrap();
        let inbox = inbox_at(&dir);
        let any_id = InstructionId::new();
        assert!(!inbox.remove_by_id(any_id));
    }

    #[test]
    fn remove_by_id_removes_matching_queued_instruction() {
        let dir = tempdir().unwrap();
        let inbox = inbox_at(&dir);
        let a = instr("first");
        let b = instr("second");
        let c = instr("third");
        inbox.append(&a);
        inbox.append(&b);
        inbox.append(&c);

        assert!(inbox.remove_by_id(b.instruction_id));
        // Order of remaining is preserved.
        assert_eq!(inbox.try_pop().unwrap().instruction_id, a.instruction_id);
        assert_eq!(inbox.try_pop().unwrap().instruction_id, c.instruction_id);
        assert!(inbox.try_pop().is_none());
    }

    #[test]
    fn remove_by_id_returns_false_when_id_not_queued() {
        let dir = tempdir().unwrap();
        let inbox = inbox_at(&dir);
        let a = instr("first");
        inbox.append(&a);

        let unknown = InstructionId::new();
        assert!(!inbox.remove_by_id(unknown));
        // Queue should be untouched.
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox.try_pop().unwrap().instruction_id, a.instruction_id);
    }

    #[test]
    fn remove_by_id_only_removes_first_match() {
        // Defensive: instruction ids are unique by construction
        // (uuid::new), but if a duplicate ever lands on disk
        // (replayed envelope, dev seeding, etc.) the remove path
        // should drop one occurrence rather than all, matching
        // the operator's intent of "cancel this one queued copy."
        let dir = tempdir().unwrap();
        let inbox = inbox_at(&dir);
        let a = instr("only");
        let id = a.instruction_id;
        inbox.append(&a);
        // Hand-write a duplicate line via append — same id, same content.
        inbox.append(&a);
        assert_eq!(inbox.len(), 2);

        assert!(inbox.remove_by_id(id));
        // Exactly one copy remains.
        assert_eq!(inbox.len(), 1);
        let remaining = inbox.try_pop().unwrap();
        assert_eq!(remaining.instruction_id, id);
        assert!(inbox.try_pop().is_none());
    }
}
