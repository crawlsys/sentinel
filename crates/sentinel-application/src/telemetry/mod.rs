//! LEG-258 telemetry pipeline — ship the sentinel KPI report inputs into
//! the Cloudflare R2 data lake, resilient and reliable.
//!
//! Two decoupled one-shot stages (designed to be timer-fired, crash-safe by
//! construction — all state is the checkpoint file plus the spool dir):
//!
//! 1. **collect** (LEG-259, [`ledger`]) — tail the append-only per-harness
//!    hook-invocation ledgers via `(dev, inode) + offset` checkpoints and
//!    stage zstd-compressed NDJSON batches in the local spool.
//! 2. **ship** (LEG-260, [`ship`]) — drain the spool to R2 with content-hash
//!    object keys so retries are idempotent; spool files are deleted only
//!    after a confirmed PUT.
//!
//! Snapshot-style sources (LEG-261, [`snapshot`]) plug into the same
//! [`ledger::TelemetrySource`] trait and the `snapshots` map in
//! [`checkpoint::Checkpoint`]: KPI scan summaries plus the agent $/issue
//! usage rollup and session→issue map (the ticket-cost association
//! streams), each shipped only when its content hash changes.
//!
//! 3. **report** (LEG-258, [`lake`] + [`report`]) — the read side: list +
//!    fetch the shipped NDJSON objects from R2 ([`lake`], the only IO) and
//!    aggregate them into fleet-activity metrics ([`report`], pure). Headline
//!    numbers: "last updated" and "unique clients reporting in". `report`'s
//!    `aggregate`/`render_*` are IO-free so a future Cloudflare read-side
//!    service can reuse the same shape over an R2 binding.

pub mod checkpoint;
pub mod lake;
pub mod ledger;
pub mod report;
pub mod ship;
pub mod sigv4;
pub mod snapshot;
pub mod spool;
