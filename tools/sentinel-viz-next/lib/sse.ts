"use client";

import { useEffect, useRef, useState } from "react";

import type { GraphResponse } from "../types/api";
import { streamUrl } from "./api";

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
    if (typeof window === "undefined" || typeof EventSource === "undefined") {
      return;
    }
    let cancelled = false;
    const es = new EventSource(streamUrl());
    sourceRef.current = es;
    es.onopen = () => {
      if (!cancelled) setConnected(true);
    };
    es.onerror = () => {
      if (!cancelled) {
        setConnected(false);
        setError("stream disconnected — auto-reconnecting…");
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
      es.close();
      sourceRef.current = null;
    };
  }, []);

  return { graph, error, connected };
}
