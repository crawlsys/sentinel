"use client";

/// Stable color assignment per session_id. Each session gets a slot
/// from the OBVIOUS palette below. Slot index is derived directly
/// from the session_id so refreshes, graph window changes, and
/// synthetic session ordering cannot reshuffle colours.

import type { GraphResponse } from "../types/api";

/// Hand-picked high-saturation hues chosen for legibility at a
/// glance on the dark background. Keep the palette larger than the
/// usual active fleet so session_id hashing rarely puts concurrent
/// agents on the same or visually adjacent colour.
export const SESSION_PALETTE: readonly string[] = [
  "#f85149", // red
  "#39c5cf", // cyan
  "#a5d6a7", // mint
  "#bc8cff", // violet
  "#f2cc60", // yellow
  "#58a6ff", // blue
  "#ff7b72", // coral
  "#7ee787", // green
  "#ffa657", // orange
  "#56d4dd", // teal
  "#db61a2", // magenta
  "#d29922", // amber
];

/// Build a sid → color map from a graph snapshot.
export function sessionColorMap(graph: GraphResponse | null): Map<string, string> {
  const out = new Map<string, string>();
  if (!graph) return out;
  const sessions = graph.nodes.filter((n) => n.type === "SentinelSession");
  sessions.forEach((n) => {
    const sid = typeof n.data?.session_id === "string" ? (n.data.session_id as string) : null;
    if (sid && !out.has(sid)) {
      out.set(sid, SESSION_PALETTE[stableSessionIndex(sid) % SESSION_PALETTE.length]);
    }
  });
  return out;
}

export function stableSessionIndex(sessionId: string): number {
  let hash = 0x811c9dc5;
  for (let i = 0; i < sessionId.length; i += 1) {
    hash ^= sessionId.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193);
  }
  return hash >>> 0;
}

export function colorForSession(
  map: Map<string, string>,
  sessionId: string | null | undefined,
): string {
  if (!sessionId) return "#484f58";
  return map.get(sessionId) ?? "#6e7681";
}
