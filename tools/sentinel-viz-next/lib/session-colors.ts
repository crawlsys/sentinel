"use client";

/// Stable color assignment per session_id. Each session gets a slot
/// from the OBVIOUS palette below. Slot index is derived from a
/// rolling list of session ids in graph order — the first session
/// the user sees stays red, the second stays amber, etc — so the
/// mapping is stable across SSE ticks.

import type { GraphResponse } from "../types/api";

/// Hand-picked high-saturation hues chosen for legibility at a
/// glance on the dark background. Distinct enough that a quick eye
/// flick across the ticker can match an event to its galaxy. Five
/// slots — matches the K_SESSIONS=5 cap on the server.
export const SESSION_PALETTE: readonly string[] = [
  "#f85149", // red
  "#d29922", // amber
  "#bc8cff", // pink/violet
  "#39c5cf", // cyan
  "#a5d6a7", // mint
];

/// Build a sid → color map from a graph snapshot. Sessions are
/// indexed by their `seq` (insertion order in the bridge) so the
/// mapping is deterministic across renders, not based on hash.
export function sessionColorMap(graph: GraphResponse | null): Map<string, string> {
  const out = new Map<string, string>();
  if (!graph) return out;
  const sessions = graph.nodes
    .filter((n) => n.type === "SentinelSession")
    .slice()
    .sort((a, b) => (a.seq ?? 0) - (b.seq ?? 0));
  sessions.forEach((n, i) => {
    const sid = typeof n.data?.session_id === "string" ? (n.data.session_id as string) : null;
    if (sid && !out.has(sid)) {
      out.set(sid, SESSION_PALETTE[i % SESSION_PALETTE.length]);
    }
  });
  return out;
}

export function colorForSession(
  map: Map<string, string>,
  sessionId: string | null | undefined,
): string {
  if (!sessionId) return "#484f58";
  return map.get(sessionId) ?? "#6e7681";
}
