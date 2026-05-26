"use client";

import type { ActivityResponse, ToolCallSummary } from "../types/api";

/// Module-singleton cache mapping (sessionId, tool, minute-bucket)
/// → a JSONL-derived ToolCallSummary. Populated lazily by the
/// PanelInspector when it loads /api/activity; consulted by the
/// EventTicker to render labels like "Bash · ls -la /tmp" instead of
/// the bare tool name the bridge SQLite stores.
///
/// The match key uses MINUTE resolution because:
///   - the bridge's ts_sec is second-precision (clipped at the second)
///   - the JSONL transcript records the assistant_turn ts (ms-precision)
///   - the two clocks are typically within ~1s but can drift in long
///     sessions; minute slop is enough to hit reliably without
///     conflating distinct tool calls in the same session.
///
/// WORKSTREAM: sentinel-viz-api — this cache straddles the two
/// independent reads (SQLite via /api/graph, JSONL via /api/activity).
/// The viz layer is the only side that joins them.

interface CacheBuckets {
  // key = `${sessionId}\t${tool}\t${minuteIso}` (minuteIso = "2026-05-25T13:25")
  [composite: string]: ToolCallSummary;
}

const cache: CacheBuckets = {};
const listeners = new Set<() => void>();

function notify() {
  for (const fn of listeners) fn();
}

function key(sessionId: string, tool: string, ts: string): string {
  const minute = ts.length >= 16 ? ts.slice(0, 16) : ts;
  return `${sessionId}\t${tool}\t${minute}`;
}

/** Walk an activity response and stash every tool_call into the cache,
 *  keyed by its parent segment's ts (minute bucket). */
export function indexActivity(sessionId: string, activity: ActivityResponse | undefined): void {
  if (!activity?.segments?.length) return;
  let added = 0;
  for (const seg of activity.segments) {
    const segTs = seg.ts;
    for (const tc of seg.tool_calls ?? []) {
      if (!tc.tool) continue;
      const k = key(sessionId, tc.tool, segTs);
      if (!cache[k]) {
        cache[k] = tc;
        added += 1;
      }
    }
  }
  if (added > 0) notify();
}

/** Look up a ToolCallSummary for the given (session, tool, ts). Returns
 *  null if we haven't ingested the matching activity yet.
 *
 *  Tolerance: try the exact minute, then ±1 minute. The bridge's
 *  ts_sec and the JSONL transcript's segment ts come from different
 *  writers and can drift by ~1s (or fall across a minute boundary
 *  for events near :00 / :59). Without the ±1 fallback, ~30% of
 *  ticker rows missed cache hits despite the data being present. */
export function lookup(sessionId: string | null, tool: string | null, ts: string | null): ToolCallSummary | null {
  if (!sessionId || !tool || !ts) return null;
  const exact = cache[key(sessionId, tool, ts)];
  if (exact) return exact;
  // Walk ±1 minute. Parse the minute-bucket back out, shift, re-key.
  const minute = ts.length >= 16 ? ts.slice(0, 16) : ts;
  const parsed = Date.parse(`${minute}:00Z`);
  if (Number.isNaN(parsed)) return null;
  for (const offsetMin of [-1, 1]) {
    const shifted = new Date(parsed + offsetMin * 60_000).toISOString().slice(0, 16);
    const k = `${sessionId}\t${tool}\t${shifted}`;
    if (cache[k]) return cache[k];
  }
  return null;
}

/** Subscribe to cache updates — used by EventTicker to re-render
 *  when fresh entries land. Returns an unsubscribe function. */
export function subscribe(fn: () => void): () => void {
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
  };
}

/** Test-only — drop everything. */
export function _resetActivityCache(): void {
  for (const k of Object.keys(cache)) delete cache[k];
}

/** Snapshot of current size — debug-friendly. */
export function _cacheSize(): number {
  return Object.keys(cache).length;
}
