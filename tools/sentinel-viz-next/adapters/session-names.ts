"use client";

import { useEffect, useState } from "react";

import { fetchSessionName } from "./http";

/// Cache of session_id → display name. Populated lazily as the
/// GraphCanvas requests labels for each SentinelSession node.
/// Values:
///   string  — model-generated name ("viz rewrite")
///   null    — naming disabled, errored, or rate-limited — caller
///             falls back to UUID slice
///   undef   — not yet requested
const cache = new Map<string, string | null>();
const inflight = new Set<string>();
const listeners = new Set<() => void>();

function notify() {
  for (const fn of listeners) fn();
}

/** Snapshot lookup — does NOT trigger a fetch. Returns undefined
 *  when we haven't requested this id yet. */
export function getCachedName(sessionId: string): string | null | undefined {
  return cache.get(sessionId);
}

/** Request a name for the session if we haven't already. Returns the
 *  cached value if known, or triggers an async fetch that will
 *  notify subscribers on completion. */
export function ensureName(sessionId: string): string | null | undefined {
  if (cache.has(sessionId)) return cache.get(sessionId);
  if (inflight.has(sessionId)) return undefined;
  inflight.add(sessionId);
  fetchSessionName(sessionId)
    .then((r) => {
      cache.set(sessionId, r.name);
      inflight.delete(sessionId);
      notify();
    })
    .catch(() => {
      cache.set(sessionId, null);
      inflight.delete(sessionId);
      notify();
    });
  return undefined;
}

export function subscribe(fn: () => void): () => void {
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
  };
}

/** Convenience hook for React components that need to subscribe and
 *  also kick off a name request. */
export function useSessionName(sessionId: string | null | undefined): string | null | undefined {
  const [, setTick] = useState(0);
  useEffect(() => subscribe(() => setTick((n) => n + 1)), []);
  if (!sessionId) return undefined;
  return ensureName(sessionId);
}

/** Test-only — drop everything. */
export function _resetSessionNames() {
  cache.clear();
  inflight.clear();
}
