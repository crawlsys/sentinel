import { describe, it, expect } from "vitest";

import { livenessLabel } from "../../components/StatusBar";
import type { GraphResponse } from "../../types/api";

const fakeGraph: GraphResponse = {
  nodes: [],
  edges: [],
  events: [],
  max_seq: 1,
  window_limit: 100,
  stats: {
    nodes_total: 0,
    edges_total: 0,
    by_type: {},
    by_outcome: {},
    events_total: 0,
    corpus_nodes: 0,
    corpus_edges: 0,
    corpus_by_type: {},
    corpus_by_outcome: {},
  },
};

/// The liveness signal is what an operator reads to decide whether
/// they can trust the dashboard. Pin every transition tightly —
/// silent drift from "live" → "stale" because someone changed a
/// fallback would mean operators acting on stale data.

describe("livenessLabel — three-state stream freshness signal", () => {
  it("live → green ● with pulse hint", () => {
    const l = livenessLabel("live", true, fakeGraph);
    expect(l.text).toBe("live");
    expect(l.color).toBe("#4A9E5C");
    expect(l.glyph).toBe("●");
  });

  it("stale → amber ●; this is the post-P3-21 win — operator sees a 5-30s SSE pause", () => {
    const l = livenessLabel("stale", true, fakeGraph);
    expect(l.text).toBe("stale");
    expect(l.color).toBe("#D4A843");
  });

  it("down with graph data → blue ● 'ready' (cached snapshot still trustworthy enough to view)", () => {
    const l = livenessLabel("down", false, fakeGraph);
    expect(l.text).toBe("ready");
    expect(l.color).toBe("#5B9BF6");
  });

  it("down with NO graph data → red ○ 'down' (full failure mode)", () => {
    const l = livenessLabel("down", false, null);
    expect(l.text).toBe("down");
    expect(l.color).toBe("#D71921");
    expect(l.glyph).toBe("○");
  });

  it("init with graph already fetched falls through to 'ready', NOT 'connecting'", () => {
    // P3-21 regression: previously this returned "connecting" even
    // when the initial snapshot fetch had landed but the SSE hadn't
    // sent its first message yet. Operators saw "connecting" with
    // a fully populated dashboard, which is wrong.
    const l = livenessLabel("init", false, fakeGraph);
    expect(l.text).toBe("ready");
    expect(l.color).toBe("#5B9BF6");
  });

  it("init with NO graph yet → connecting (cold start)", () => {
    const l = livenessLabel("init", false, null);
    expect(l.text).toBe("connecting");
    expect(l.glyph).toBe("○");
  });

  it("undefined liveness falls back to the boolean connected flag (back-compat)", () => {
    // Old tests / callers that hadn't been migrated yet shouldn't
    // see a regression. true → live, false-with-graph → ready,
    // false-without-graph → connecting.
    expect(livenessLabel(undefined, true, fakeGraph).text).toBe("live");
    expect(livenessLabel(undefined, false, fakeGraph).text).toBe("ready");
    expect(livenessLabel(undefined, false, null).text).toBe("connecting");
  });
});
