"use client";

import { useEffect, useRef, useState } from "react";

import type { GraphResponse } from "../types/api";
import { fetchGraph, streamUrl } from "./api";

/// Subscribes to /api/stream and yields the most recent full snapshot.
/// Falls back to polling /api/graph if EventSource is unavailable.
export function useGraphStream(): {
  graph: GraphResponse | null;
  error: string | null;
  connected: boolean;
} {
  const [graph, setGraph] = useState<GraphResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);
  const sourceRef = useRef<EventSource | null>(null);

  useEffect(() => {
    if (typeof window === "undefined") {
      return;
    }

    let cancelled = false;
    const abort = new AbortController();

    const loadSnapshot = async (signal?: AbortSignal) => {
      try {
        const data = await fetchGraph(100, signal);
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
    // NOTE: do NOT mark connected=true in onopen. Localhost SSE socket
    // opens before the initial snapshot is parsed, which makes the
    // status bar skip the "● ready" state entirely. The status flow
    // we want is: "○ connecting" → "● ready" (snapshot loaded) →
    // "● live" (first SSE message received). Only set connected=true
    // when actual data arrives.
    es.onerror = () => {
      if (cancelled) return;
      const since = Date.now() - lastMessageAt;
      if (lastMessageAt === 0 || since > 30_000) {
        setConnected(false);
        setError("sentinel API unreachable — auto-reconnecting");
      }
    };
    es.onmessage = (e) => {
      if (cancelled) return;
      lastMessageAt = Date.now();
      setConnected(true);
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
      es.close();
      sourceRef.current = null;
    };
  }, []);

  return { graph, error, connected };
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
