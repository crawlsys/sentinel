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
          setError(null);
        }
      } catch (err) {
        const isAbort = err instanceof Error && err.name === "AbortError";
        if (!cancelled && !isAbort) {
          setError(`snapshot failed: ${String(err)}`);
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
    es.onopen = () => {
      if (!cancelled) setConnected(true);
    };
    es.onerror = () => {
      if (!cancelled) {
        setConnected(false);
        setError("stream disconnected - auto-reconnecting...");
      }
    };
    es.onmessage = (e) => {
      if (cancelled) return;
      try {
        const data = JSON.parse(e.data) as GraphResponse;
        setGraph(data);
        setError(null);
      } catch (err) {
        setError(`bad payload: ${String(err)}`);
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
