"use client";

import {
  fetchRollupSummary,
  type RollupMember,
  type RollupSummaryResponse,
} from "./http";

/// Module-level cache for rollup-summary blurbs. Keyed by
/// cache_key (a stable hash of the rollup's signature — the
/// EventTicker computes this so identical rollups across
/// re-renders map to one entry).
///
/// Three reasons we cache here in addition to the server:
///   1. The server's cache lives in-process; if viz-api restarts
///      mid-session the operator would see "(summarizing…)"
///      flicker back. Client cache survives.
///   2. Eliminates duplicate fetches when React re-renders the
///      ticker before the first response lands.
///   3. Lets subscribers re-render without re-fetching once the
///      blurb is in hand.
///
/// Entries are weak by convention — there's no eviction, but
/// rollup signatures are bounded by the visible event window
/// (~750 events × low rollup density), so the map stays small.

interface CacheEntry {
  /** Final blurb. `null` once the server returned but had no
   *  text (no model, llm-error). Distinguishes "we asked, got
   *  nothing" from "we haven't asked yet" (the latter is a
   *  missing map entry). */
  summary: string | null;
  /** Source label from the server response, kept for debugging. */
  source: string;
}

const cache = new Map<string, CacheEntry>();
const inflight = new Map<string, Promise<RollupSummaryResponse>>();
const subscribers = new Set<() => void>();

/// Look up a cached blurb without firing a request.
/// Returns:
///   undefined → never fetched (caller should call `request()`)
///   { summary: string | null } → server responded
export function peek(cacheKey: string): CacheEntry | undefined {
  return cache.get(cacheKey);
}

/// Subscribe to cache updates. Returns an unsubscribe function.
/// EventTicker uses this to re-render when a blurb lands so the
/// `(summarizing…)` placeholder swaps in-place.
export function subscribe(fn: () => void): () => void {
  subscribers.add(fn);
  return () => subscribers.delete(fn);
}

function notify(): void {
  for (const fn of subscribers) fn();
}

/// Fire a rollup-summary request, de-duplicated by cache_key.
/// Returns a promise resolving to the response (already cached).
/// Multiple concurrent callers with the same key share one
/// in-flight request.
///
/// Idempotent: re-calling after the response lands hits the
/// cache and returns immediately.
export async function request(
  cacheKey: string,
  sessionId: string,
  members: RollupMember[],
): Promise<RollupSummaryResponse> {
  const cached = cache.get(cacheKey);
  if (cached) {
    return {
      cache_key: cacheKey,
      session_id: sessionId,
      summary: cached.summary,
      source: cached.source,
      cached: true,
    };
  }
  const existing = inflight.get(cacheKey);
  if (existing) return existing;

  const p = fetchRollupSummary({ cache_key: cacheKey, session_id: sessionId, members })
    .then((resp) => {
      cache.set(cacheKey, { summary: resp.summary, source: resp.source });
      notify();
      return resp;
    })
    .catch((err) => {
      // Network failure — DON'T cache. The next render attempt
      // gets a fresh try. (Server-side LLM failures DO cache,
      // because the response came back with summary=null and
      // source=llm-error — that's the server's call.)
      console.warn("[rollup-summary] fetch failed:", err);
      throw err;
    })
    .finally(() => {
      inflight.delete(cacheKey);
    });

  inflight.set(cacheKey, p);
  return p;
}

/// Drop everything. Exported for tests; not used in production.
export function _reset(): void {
  cache.clear();
  inflight.clear();
  subscribers.clear();
}
