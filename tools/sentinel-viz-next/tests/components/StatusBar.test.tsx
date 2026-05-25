import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";

import { StatusBar } from "../../components/StatusBar";
import type { GraphResponse } from "../../types/api";

const fakeGraph: GraphResponse = {
  nodes: [],
  edges: [],
  events: [],
  max_seq: 1234,
  window_limit: 100,
  stats: {
    nodes_total: 5,
    edges_total: 7,
    by_type: {},
    by_outcome: {},
    events_total: 42,
    corpus_nodes: 100,
    corpus_edges: 200,
    corpus_by_type: {},
    corpus_by_outcome: {},
  },
};

describe("StatusBar", () => {
  it("renders connecting state when graph is null", () => {
    render(<StatusBar graph={null} connected={false} error={null} />);
    expect(screen.getByText(/connecting/i)).toBeInTheDocument();
    expect(screen.getByText(/waiting on first snapshot/i)).toBeInTheDocument();
  });

  it("renders live counts when connected", () => {
    render(<StatusBar graph={fakeGraph} connected={true} error={null} />);
    expect(screen.getByText(/live/i)).toBeInTheDocument();
    expect(screen.getByText(/seq: 1234/)).toBeInTheDocument();
    expect(screen.getByText(/events: 42/)).toBeInTheDocument();
  });

  it("surfaces errors", () => {
    render(<StatusBar graph={fakeGraph} connected={false} error="stream disconnected" />);
    expect(screen.getByText(/stream disconnected/)).toBeInTheDocument();
  });
});
