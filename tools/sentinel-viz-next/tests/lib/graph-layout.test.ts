import { describe, expect, it } from "vitest";

import type { Node } from "../../types/api";
import {
  GOLDEN_ANGLE,
  SPIRAL_BASE_R,
  type LayoutLink,
  type LayoutNode,
  annotateChainRanks,
  chainOpacity,
  sessionLabel,
  spiralOffset,
  tcLabel,
} from "../../lib/graph-layout";

/// Build a minimal Node shaped like the API response. Only `data` is
/// load-bearing for the label helpers.
function node(data: Record<string, unknown>): Node {
  return {
    id: typeof data.id === "string" ? data.id : "n",
    type: "x",
    data,
    ts: "2026-05-26T00:00:00Z",
    seq: 0,
  };
}

function tc(id: string, opts: Partial<LayoutNode> & { tool?: string } = {}): LayoutNode {
  return {
    id,
    kind: "SentinelToolCall",
    sid: opts.sid ?? "sess",
    chainRank: opts.chainRank,
    ref: node({ tool: opts.tool }),
  };
}

function link(source: string, target: string, kind = "next_tool_call"): LayoutLink {
  return { source, target, kind };
}

describe("spiralOffset", () => {
  it("rank 0 sits at the base radius (sqrt(0.5)) and angle 0", () => {
    const { dx, dy } = spiralOffset(0);
    const r = SPIRAL_BASE_R * Math.sqrt(0.5);
    // theta = 0 → dx = r, dy = 0.
    expect(dx).toBeCloseTo(r, 6);
    expect(dy).toBeCloseTo(0, 6);
  });

  it("radius grows with sqrt(rank + 0.5)", () => {
    const r3 = Math.hypot(spiralOffset(3).dx, spiralOffset(3).dy);
    expect(r3).toBeCloseTo(SPIRAL_BASE_R * Math.sqrt(3.5), 6);
  });

  it("successive ranks advance by the golden angle", () => {
    const a1 = Math.atan2(spiralOffset(1).dy, spiralOffset(1).dx);
    const a2 = Math.atan2(spiralOffset(2).dy, spiralOffset(2).dx);
    // Normalise the wrapped delta into (-pi, pi].
    let delta = a2 - a1;
    while (delta <= -Math.PI) delta += 2 * Math.PI;
    while (delta > Math.PI) delta -= 2 * Math.PI;
    const goldenWrapped =
      GOLDEN_ANGLE > Math.PI ? GOLDEN_ANGLE - 2 * Math.PI : GOLDEN_ANGLE;
    expect(delta).toBeCloseTo(goldenWrapped, 6);
  });
});

describe("chainOpacity", () => {
  it("is full (1.0) for non-chain nodes", () => {
    expect(chainOpacity({ chainRank: undefined })).toBe(1.0);
  });

  it("fades ~0.14 per rank", () => {
    expect(chainOpacity({ chainRank: 0 })).toBeCloseTo(1.0, 6);
    expect(chainOpacity({ chainRank: 1 })).toBeCloseTo(0.86, 6);
    expect(chainOpacity({ chainRank: 3 })).toBeCloseTo(0.58, 6);
  });

  it("clamps to a 0.18 floor for deep ranks", () => {
    expect(chainOpacity({ chainRank: 20 })).toBe(0.18);
  });
});

describe("sessionLabel", () => {
  const resolveNone = () => null;

  it("returns empty for non-session nodes", () => {
    expect(sessionLabel({ kind: "SentinelToolCall", id: "x", ref: node({}) }, resolveNone)).toBe("");
  });

  it("prefers a resolved human name when available", () => {
    const d = { kind: "SentinelSession", id: "id", ref: node({ session_id: "abcdef1234567890" }) };
    expect(sessionLabel(d, () => "pretty name")).toBe("pretty name");
  });

  it("falls back to an 8-char UUID slice when no name and id is long", () => {
    const d = { kind: "SentinelSession", id: "id", ref: node({ session_id: "abcdef1234567890" }) };
    expect(sessionLabel(d, resolveNone)).toBe("abcdef12…");
  });

  it("returns the raw sid when short", () => {
    const d = { kind: "SentinelSession", id: "id", ref: node({ session_id: "short" }) };
    expect(sessionLabel(d, resolveNone)).toBe("short");
  });

  it("uses node id as the sid when data.session_id is absent", () => {
    const d = { kind: "SentinelSession", id: "fallbackid", ref: node({}) };
    expect(sessionLabel(d, resolveNone)).toBe("fallbackid");
  });
});

describe("tcLabel", () => {
  it("returns empty for non-toolcall nodes", () => {
    expect(tcLabel({ kind: "SentinelSession", chainRank: 0, ref: node({ tool: "Bash" }) })).toBe("");
  });

  it("labels only the freshest 5 (rank 0..4)", () => {
    expect(tcLabel({ kind: "SentinelToolCall", chainRank: 4, ref: node({ tool: "Bash" }) })).toBe("Bash");
    expect(tcLabel({ kind: "SentinelToolCall", chainRank: 5, ref: node({ tool: "Bash" }) })).toBe("");
  });

  it("returns empty when chainRank is undefined", () => {
    expect(tcLabel({ kind: "SentinelToolCall", chainRank: undefined, ref: node({ tool: "Bash" }) })).toBe("");
  });

  it("returns empty when the tool field is missing", () => {
    expect(tcLabel({ kind: "SentinelToolCall", chainRank: 0, ref: node({}) })).toBe("");
  });
});

describe("annotateChainRanks", () => {
  it("ranks a linear chain from the tail backward (most-recent = 0)", () => {
    // a -> b -> c  (c is the tail: nothing points out of it)
    const a = tc("a");
    const b = tc("b");
    const c = tc("c");
    annotateChainRanks([a, b, c], [link("a", "b"), link("b", "c")]);
    expect(c.chainRank).toBe(0);
    expect(b.chainRank).toBe(1);
    expect(a.chainRank).toBe(2);
  });

  it("ignores edges that are not next_tool_call", () => {
    const a = tc("a");
    const b = tc("b");
    annotateChainRanks([a, b], [link("a", "b", "belongs_to")]);
    // No next_tool_call edges → both are tails → both rank 0.
    expect(a.chainRank).toBe(0);
    expect(b.chainRank).toBe(0);
  });

  it("seeds isolated TCs with rank 0", () => {
    const lone = tc("lone");
    annotateChainRanks([lone], []);
    expect(lone.chainRank).toBe(0);
  });

  it("does not assign ranks to non-TC nodes", () => {
    const session: LayoutNode = { id: "s", kind: "SentinelSession", sid: "sess", ref: node({}) };
    const a = tc("a");
    annotateChainRanks([session, a], []);
    expect(session.chainRank).toBeUndefined();
    expect(a.chainRank).toBe(0);
  });

  it("terminates on a cyclic chain instead of looping forever", () => {
    // a -> b -> a forms a cycle; both have outbound so neither is a
    // pure tail — nothing is seeded, and the walk's `seen` guard means
    // the function still returns.
    const a = tc("a");
    const b = tc("b");
    annotateChainRanks([a, b], [link("a", "b"), link("b", "a")]);
    // Both have outbound edges, so the tail-seeding loop skips them and
    // no ranks are assigned — the important property is that it returns.
    expect(a.chainRank).toBeUndefined();
    expect(b.chainRank).toBeUndefined();
  });

  it("accepts object-form link endpoints (resolved d3 nodes)", () => {
    const a = tc("a");
    const b = tc("b");
    const objLink: LayoutLink = { source: { id: "a" }, target: { id: "b" }, kind: "next_tool_call" };
    annotateChainRanks([a, b], [objLink]);
    expect(b.chainRank).toBe(0);
    expect(a.chainRank).toBe(1);
  });

  it("keeps the smallest rank when a node is reachable from two tails", () => {
    // Two tails c and d both lead back to shared ancestor a:
    //   a -> b -> c   and   a -> b is shared, plus a -> d (d is a 2nd tail)
    // Simpler: diamond where a feeds b, b is tail in one path but we also
    // give a a direct longer route. We assert a ends up with the minimum.
    const a = tc("a");
    const b = tc("b");
    const c = tc("c");
    // a->b->c (c tail). a is rank 2 via this path.
    annotateChainRanks([a, b, c], [link("a", "b"), link("b", "c")]);
    expect(a.chainRank).toBe(2);
    // Re-run including a shorter inbound to a would lower it; assert the
    // min-keeping branch by manually pre-seeding a higher rank.
    a.chainRank = 5;
    annotateChainRanks([a, b, c], [link("a", "b"), link("b", "c")]);
    expect(a.chainRank).toBe(2); // 2 < 5, so it was lowered
  });
});
