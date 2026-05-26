"use client";

import { useEffect, useRef, useState } from "react";

import type { GraphResponse } from "../types/api";
import { fetchGraph, streamUrl } from "./api";

/// Subscribes to /api/stream and yields the most recent full snapshot.
/// Falls back to polling /api/graph if EventSource is unavailable.
/// Pass `focusedSession` (a data.session_id) to ask the server to
/// expand that session's window from the default 12 nodes to 36.
/// Three-state liveness signal — matches what an operator can act on:
///   - "live"  — fresh data (last SSE message <5s ago)
///   - "stale" — data is here but no fresh stream (5-30s gap)
///   - "down"  — no data OR stream truly disconnected (>30s gap)
///   - "init"  — never received a message yet
///
/// The previous boolean `connected` flag was true the whole time
/// between message-received and the 30s timeout, so brief drops
/// went unnoticed. The "stale" tier makes a 20s SSE blip visible
/// without making the indicator flicker every few seconds.
export type StreamLiveness = "live" | "stale" | "down" | "init";

const STALE_AFTER_MS = 5_000;
const DOWN_AFTER_MS = 30_000;
const FRESHNESS_TICK_MS = 1_500;

export function useGraphStream(focusedSession: string | null = null): {
  graph: GraphResponse | null;
  error: string | null;
  connected: boolean;
  liveness: StreamLiveness;
} {
  const [graph, setGraph] = useState<GraphResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);
  const [liveness, setLiveness] = useState<StreamLiveness>("init");
  const sourceRef = useRef<EventSource | null>(null);

  useEffect(() => {
    if (typeof window === "undefined") {
      return;
    }

    let cancelled = false;
    const abort = new AbortController();

    const loadSnapshot = async (signal?: AbortSignal) => {
      try {
        // Use fetchGraph's new default (300) so initial snapshot
        // pulls the full backend window. P3-29 bump.
        const data = await fetchGraph(undefined, signal, { focusedSession });
        if (!cancelled) {
          setGraph(data);
          setError(data.error ?? null);
        }
      } catch (err) {
        const isAbort = err instanceof Error && err.name === "AbortError";
        if (!cancelled && !isAbort) {
          setError(humanError(err));
        }
      }
    };

    void loadSnapshot(abort.signal);

    if (typeof EventSource === "undefined") {
      const interval = window.setInterval(() => void loadSnapshot(), 2_000);
      return () => {
        cancelled = true;
        abort.abort();
        window.clearInterval(interval);
      };
    }

    const es = new EventSource(streamUrl());
    sourceRef.current = es;
    let lastMessageAt = 0;
    // Re-evaluate freshness on a small ticker so the indicator
    // crosses live→stale→down on its own without needing a new
    // SSE event to advance state. setLiveness is identity-safe
    // (same value short-circuits in React).
    const freshnessTimer = window.setInterval(() => {
      if (cancelled) return;
      if (lastMessageAt === 0) return; // still "init"
      const gap = Date.now() - lastMessageAt;
      setLiveness((prev) => {
        const next: StreamLiveness =
          gap < STALE_AFTER_MS ? "live" : gap < DOWN_AFTER_MS ? "stale" : "down";
        return next === prev ? prev : next;
      });
    }, FRESHNESS_TICK_MS);
    // NOTE: do NOT mark connected=true in onopen. Localhost SSE socket
    // opens before the initial snapshot is parsed, which makes the
    // status bar skip the "● ready" state entirely. The status flow
    // we want is: "○ connecting" → "● ready" (snapshot loaded) →
    // "● live" (first SSE message received). Only set connected=true
    // when actual data arrives.
    es.onerror = () => {
      if (cancelled) return;
      const since = Date.now() - lastMessageAt;
      if (lastMessageAt === 0 || since > DOWN_AFTER_MS) {
        setConnected(false);
        setLiveness("down");
        setError("sentinel API unreachable — auto-reconnecting");
      } else if (since > STALE_AFTER_MS) {
        // We have data but the stream is misbehaving. Surface
        // stale-not-down so the operator sees the indicator change
        // immediately on the next freshness tick.
        setLiveness("stale");
      }
    };
    es.onmessage = (e) => {
      if (cancelled) return;
      lastMessageAt = Date.now();
      setConnected(true);
      setLiveness("live");
      try {
        const data = JSON.parse(e.data) as GraphResponse;
        setGraph(data);
        setError(data.error ?? null);
      } catch (err) {
        setError(`bad response from sentinel API: ${String(err)}`);
      }
    };
    return () => {
      cancelled = true;
      abort.abort();
      window.clearInterval(freshnessTimer);
      es.close();
      sourceRef.current = null;
    };
  }, [focusedSession]);

  return { graph, error, connected, liveness };
}

function humanError(err: unknown): string {
  const m = String(err);
  if (m.includes("graph: 5") || m.includes("HTTP 5")) {
    return "sentinel API is unreachable (5xx)";
  }
  if (m.includes("Failed to fetch") || m.includes("NetworkError")) {
    return "sentinel API unreachable — is the Rust API running?";
  }
  if (m.includes("AbortError")) {
    return "request cancelled";
  }
  return `snapshot failed (${m.replace(/^Error: /, "")})`;
}
