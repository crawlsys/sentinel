# Hookdeck dedupe + replay

Two orthogonal noise-reduction primitives for the Hookdeck webhook pipeline,
both landed as part of the Hookdeck first-class upgrade, work stream 4c.

## 1. Coalescing buffer (`sentinel_application::dedupe`)

A webhook burst — e.g. Linear bulk-assign firing 50 `Issue.update` events in
<2 seconds — should wake the session once on the final state, not fifty
times. `Coalescer` buffers events on a sliding quiet window keyed on
`(source, resource_id, event_type)` and emits only the last value once the
window elapses.

### Key

```rust
DedupKey {
    source: String,          // "linear", "github", "vercel"...
    resource_id: Option<String>,  // "FPCRM-329", "owner/repo#123"
    event_type: String,      // "Issue.update", "check_run.completed"
}
```

The key is extracted from the decoded `ChannelEvent.meta` map — specifically
`meta.source`, `meta.resource_id`, and either `meta.event_type` or the
top-level `event` field. Events without a `meta.source` are **not
coalescable** (they bypass the buffer and emit immediately). That keeps
agent-completion and build-done notifications free of any added latency.

### Window

3 seconds, sliding. Each fresh arrival resets the quiet timer — a slow-drip
storm (one event every 2s) will hold the buffer open until the source
actually quiets down. A background task in `sentinel-mcp` flushes every
500ms (`MissedTickBehavior::Skip` so load spikes shed).

### Coalesce annotation

When > 1 event collapses, the emitted `ChannelEvent` has
`meta.coalesce_count = N` inserted. Single-event emissions are unannotated so
downstream observers can tell bursts from ones.

### Testing

`dedupe.rs` injects a `Clock` trait so tests advance mock time instantly.
Eight deterministic unit tests cover: burst coalescing, sliding-window
extension, distinct keys staying separate, non-coalescable bypass,
superseded-path cleanup, force-flush, meta-less falls-back to top-level
event, resource-less keys still work.

## 2. Replay (catchup) — `sentinel_application::webhook_replay`

When a session crashes or exits while webhooks are still firing, the next
session misses those events. Replay reconstructs a compact catchup summary:

> `[HOOKDECK REPLAY] Since 2026-04-22T09:24Z: 5 events — 3 CI runs, 2 Linear state changes`

### Flow

```
SessionStart
  → sentinel-mcp.get_last_webhook_ts()                   # read marker
  → hookdeck-mcp.list_events_since(since)                # fetch events
  → sentinel decoders (4b) normalise each event          # → Vec<DecodedWebhook>
  → sentinel-mcp.summarize_replay_events(events)         # bucket + rank
  → inject banner string as channel event
```

### Marker file

Per-session at `~/.claude/sentinel/state/{session_id}/last_webhook_ts.txt`
— a plain RFC3339 timestamp. Atomic write via tempfile + rename.
Corrupt / empty / missing files are treated as "no prior window" so the
first session (or a deliberately reset marker) skips replay cleanly.

### Who stamps the marker

The sentinel-mcp drain loop. Every successful channel notification updates
the marker to the delivered event's own `ts`. That means the stored value
tracks "events actually delivered to the session", not "wall-clock at
delivery" — avoiding a split-brain where the marker advances past events
that failed to deliver.

### Summary shape

`ReplayResult` → `{ since, until, event_count, buckets, highlights, banner }`

Buckets are counted by human-friendly labels (`CI runs`, `Linear state
changes`, `PRs merged`...) via `bucket_label_for`. Highlights are scored:
failures (100) > closed/merged (50) > generic updates (20) > everything else
(1). Top 5 highlights only.

### Testing

`webhook_replay.rs` has ten unit tests: round-trip marker file, missing /
corrupt / empty file handling, bucket ordering (count desc, label asc),
failure prioritisation, empty-window banner, busy-window banner, first-run
banner, state dir layout.

## Filter expressions (applied via `mcp__hookdeck__update_connection_rules`)

Source-level filters cut noise before it reaches the gateway's destination.
All five match the Hookdeck filter DSL (`$eq` / `$neq` / `$or` / `$in` /
`$endsWith` / `$not` / `$exist`).

### Linear `Issue.update` — state transitions only

```json
{
  "type": "filter",
  "body": {
    "action": "update",
    "updatedFrom": { "stateId": { "$exist": true } }
  }
}
```

Linear populates `updatedFrom` only with the fields that actually changed.
Gating on `updatedFrom.stateId` keeps description / priority / assignee
edits out of the session — those aren't state transitions.

### GitHub `check_run.completed` — failure conclusions only

```json
{
  "type": "filter",
  "body": {
    "check_run": {
      "conclusion": { "$or": ["failure", "cancelled", "timed_out", "action_required"] }
    }
  }
}
```

`success` is background noise — we poll PR CI from the session's own cron
jobs anyway. Only actionable conclusions wake the session.

### GitHub `issue_comment.created` — humans + coderabbitai only

```json
{
  "type": "filter",
  "body": {
    "$or": [
      { "sender": { "login": { "$not": { "$endsWith": "[bot]" } } } },
      { "sender": { "login": "coderabbitai" } }
    ]
  }
}
```

Drops Dependabot / Renovate / GitHub Actions bot comments. CodeRabbit
review comments are explicitly allowed through.

### Vercel `deployment.*` — errors only

```json
{
  "type": "filter",
  "body": {
    "$or": [
      { "type": "deployment.error" },
      { "payload": { "state": { "$or": ["ERROR", "FAILED"] } } }
    ]
  }
}
```

Successful deploys surface via the session's own `git push` → auto-monitor
cron jobs.

### Railway `deployment.*` — errors only

```json
{
  "type": "filter",
  "body": { "status": { "$or": ["FAILED", "CRASHED", "REMOVED", "ERROR"] } }
}
```

Same rationale as Vercel.

## Application procedure

Filter rules are prepended to each connection's existing rules so they run
before retries:

1. `mcp__hookdeck__get_connection(connection_id)` → read current `rules`.
2. Prepend the filter rule object from above.
3. `mcp__hookdeck__update_connection_rules(connection_id, rules_json)`.
4. Verify: `mcp__hookdeck__get_connection` — confirm both the filter + any
   prior `retry` / `transform` rules are present.

The specific connection IDs per source are listed in the sibling audit
document (`hookdeck-sources.md`, owned by work stream 4a).
