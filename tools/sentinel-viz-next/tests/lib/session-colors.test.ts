import { describe, expect, it } from "vitest";

import { sessionColorMap } from "../../domain/session-colors";
import type { GraphResponse, Node } from "../../types/api";

function session(sid: string, seq: number): Node {
  return {
    id: `SentinelSession#${seq}`,
    type: "SentinelSession",
    data: { session_id: sid },
    ts: "2026-05-28T00:00:00Z",
    seq,
  };
}

function graph(nodes: Node[]): GraphResponse {
  return {
    nodes,
    edges: [],
    events: [],
    max_seq: 1,
    window_limit: 750,
    stats: {
      nodes_total: nodes.length,
      edges_total: 0,
      by_type: {},
      by_outcome: {},
      events_total: 0,
      corpus_nodes: nodes.length,
      corpus_edges: 0,
      corpus_by_type: {},
      corpus_by_outcome: {},
    },
  };
}

describe("sessionColorMap", () => {
  it("assigns a session the same color regardless of visible ordering", () => {
    const sid = "3948edc9-c7df-4f91-a1fe-79869e8c3959";
    const first = sessionColorMap(graph([
      session(sid, 1),
      session("ba60ddc6-c78c-40b2-b49f-8a32e933a410", 2),
    ])).get(sid);
    const second = sessionColorMap(graph([
      session("ba60ddc6-c78c-40b2-b49f-8a32e933a410", 1),
      session("b1ac83df-5baf-4f99-8ae8-8cbff6d1fb42", 2),
      session(sid, 99),
    ])).get(sid);

    expect(second).toBe(first);
  });

  it("spreads the current four-agent demo session ids across distinct colors", () => {
    const sids = [
      "4ff056c1-8d44-4efb-bba5-1477f800f3c1",
      "69ba1614-8660-4f98-a2db-2fcf23a4ae2b",
      "5ace4fa2-e16e-4a4e-900d-894866158480",
      "194a9d37-9848-469e-b6bc-646418217e55",
    ];
    const colors = sids.map((sid, i) => sessionColorMap(graph(
      sids.map((s, j) => session(s, j + i)),
    )).get(sid));

    expect(new Set(colors).size).toBe(sids.length);
  });
});
