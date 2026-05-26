/// Pure layout helpers for the activity graph (GraphCanvas).
///
/// Everything here is deliberately free of React and d3 so it can be
/// unit-tested in isolation. GraphCanvas builds its `SimNode`s as a
/// superset of `LayoutNode` and passes them straight through.

import type { Node } from "../types/api";

/// The structural subset of a graph node that the layout math needs.
/// GraphCanvas's `SimNode` is a superset of this (it adds d3 simulation
/// fields like x/y/vx/vy), so it satisfies this shape directly.
export interface LayoutNode {
  id: string;
  kind: string;
  /** session_id this node belongs to, or null for orphans. */
  sid?: string | null;
  /** 0-based rank from the chain head (most recent TC = 0). Undefined
   *  for sessions and TCs not on a derived chain. */
  chainRank?: number;
  ref: Node;
}

/// A directed `next_tool_call` link between two node ids.
export interface LayoutLink {
  source: string | { id: string };
  target: string | { id: string };
  kind: string;
}

/// Golden-angle spiral step. Pi * (3 - sqrt(5)) ≈ 2.39996 rad ≈ 137.5°.
/// Same constant sunflower seed-head spacing uses; gives evenly packed
/// nodes with no two ever colliding angularly.
export const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));
/// Base spacing between adjacent TCs along the spiral.
export const SPIRAL_BASE_R = 22;

export function spiralOffset(rank: number): { dx: number; dy: number } {
  // Rank 0 (chain head) sits very close to the centre; later ranks
  // spiral outward proportional to sqrt(rank).
  const r = SPIRAL_BASE_R * Math.sqrt(rank + 0.5);
  const theta = rank * GOLDEN_ANGLE;
  return { dx: Math.cos(theta) * r, dy: Math.sin(theta) * r };
}

/// Opacity by chain rank. Head of the chain (rank 0) is full; each
/// step back fades by ~0.14. After ~6 hops the node nearly disappears.
/// Non-chain nodes (sessions, prompts) stay full.
export function chainOpacity(d: Pick<LayoutNode, "chainRank">): number {
  if (d.chainRank == null) return 1.0;
  return Math.max(0.18, 1.0 - d.chainRank * 0.14);
}

/// Label rendered next to a SentinelSession node. Asks the injected
/// name resolver for a cached human name; falls back to a UUID slice
/// when naming is disabled or hasn't returned yet. `resolveName` is
/// injected (rather than importing the names cache directly) so this
/// stays pure and unit-testable.
export function sessionLabel(
  d: Pick<LayoutNode, "kind" | "id" | "ref">,
  resolveName: (sid: string) => string | null | undefined,
): string {
  if (d.kind !== "SentinelSession") return "";
  const sid = typeof d.ref.data?.session_id === "string" ? (d.ref.data.session_id as string) : d.id;
  if (!sid) return "";
  const named = resolveName(sid);
  if (typeof named === "string" && named.length > 0) return named;
  // Fall back to UUID slice (8 chars).
  return sid.length > 12 ? `${sid.slice(0, 8)}…` : sid;
}

/// Label rendered next to a SentinelToolCall node. Only labels the last
/// 5 TCs per session (the "recent chain") so the eye finds the active
/// head; older calls in the chain are unlabelled and fade out.
export function tcLabel(d: Pick<LayoutNode, "kind" | "chainRank" | "ref">): string {
  if (d.kind !== "SentinelToolCall") return "";
  if (d.chainRank == null || d.chainRank > 4) return "";
  const tool = typeof d.ref.data?.tool === "string" ? (d.ref.data.tool as string) : "";
  if (!tool) return "";
  return tool;
}

function linkEnd(end: string | { id: string }): string {
  return typeof end === "string" ? end : end.id;
}

/// Compute per-TC chain rank by walking `next_tool_call` edges. Each
/// session's chain is laid out chronologically; we walk from the tail
/// (the TC that no `next_tool_call` points OUT FROM, i.e. the most-recent
/// TC) and assign 0,1,2,... back along the chain. Mutates `chainRank`
/// on the passed nodes in place.
export function annotateChainRanks<T extends LayoutNode>(nodes: T[], links: LayoutLink[]): void {
  // Build directed adjacency on next_tool_call edges only.
  const inbound = new Map<string, string>(); // target → source
  const outbound = new Map<string, string>(); // source → target
  for (const l of links) {
    if (l.kind !== "next_tool_call") continue;
    const s = linkEnd(l.source);
    const t = linkEnd(l.target);
    inbound.set(t, s);
    outbound.set(s, t);
  }
  // Index nodes by id once so the backward walk is O(chain length)
  // instead of O(nodes) per hop (was a nodes.find inside the loop).
  const byId = new Map<string, T>();
  for (const n of nodes) byId.set(n.id, n);
  // Tails of chains: TC nodes with inbound but no outbound (last in their
  // session). We also seed isolated TCs with rank 0 so they get labelled
  // if there are any non-chain TCs in the window.
  for (const n of nodes) {
    if (n.kind !== "SentinelToolCall") continue;
    if (outbound.has(n.id)) continue;
    // walk backwards assigning rank.
    let cur: string | undefined = n.id;
    let rank = 0;
    const seen = new Set<string>();
    while (cur && !seen.has(cur)) {
      seen.add(cur);
      const node = byId.get(cur);
      if (!node) break;
      // Only assign if this is the smallest rank we've seen for this node.
      if (node.chainRank == null || rank < node.chainRank) {
        node.chainRank = rank;
      }
      cur = inbound.get(cur);
      rank += 1;
    }
  }
}
