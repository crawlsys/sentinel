import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { StatusBar } from "../../components/StatusBar";
import { AUTO_WATCH_IGNORE_ATTR } from "../../hooks/auto-watch";
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

  describe("auto-watch toggle", () => {
    it("renders disabled state while demo auto-watch is disabled", () => {
      render(
        <StatusBar graph={fakeGraph} connected={true} error={null} autoOn={false} />,
      );
      const btn = screen.getByTestId("auto-watch-toggle");
      expect(btn.textContent).toMatch(/auto\s+disabled/i);
    });

    it("keeps the state contract via data-auto-on even while the label is disabled", () => {
      render(<StatusBar graph={fakeGraph} connected={true} error={null} autoOn={true} />);
      const btn = screen.getByTestId("auto-watch-toggle");
      expect(btn.textContent).toMatch(/auto\s+disabled/i);
      // Visual cue comes from the MUI sx-based theme; we assert the
      // state contract via data attr rather than a class string.
      expect(btn.getAttribute("data-auto-on")).toBe("true");
    });

    it("does not invoke onToggleAuto while disabled", () => {
      const spy = vi.fn();
      render(
        <StatusBar
          graph={fakeGraph}
          connected={true}
          error={null}
          autoOn={false}
          onToggleAuto={spy}
        />,
      );
      fireEvent.click(screen.getByTestId("auto-watch-toggle"));
      expect(spy).not.toHaveBeenCalled();
    });

    it("carries the data-auto-watch-ignore attribute so its own click doesn't flip auto off", () => {
      render(
        <StatusBar graph={fakeGraph} connected={true} error={null} autoOn={false} />,
      );
      const btn = screen.getByTestId("auto-watch-toggle");
      expect(btn.hasAttribute(AUTO_WATCH_IGNORE_ATTR)).toBe(true);
    });

    it("tooltip explains that auto-watch is disabled", () => {
      const { rerender } = render(
        <StatusBar
          graph={fakeGraph}
          connected={true}
          error={null}
          autoOn={false}
          autoReason="operator"
        />,
      );
      expect(screen.getByTestId("auto-watch-toggle").getAttribute("title")).toMatch(
        /disabled for this demo/i,
      );
      rerender(
        <StatusBar
          graph={fakeGraph}
          connected={true}
          error={null}
          autoOn={true}
          autoReason="idle"
        />,
      );
      expect(screen.getByTestId("auto-watch-toggle").getAttribute("title")).toMatch(
        /disabled for this demo/i,
      );
    });
  });

  describe("stuck badge dedup", () => {
    it("hides STUCK badge when count is 0", () => {
      render(<StatusBar graph={fakeGraph} connected={true} error={null} stuckCount={0} />);
      expect(screen.queryByTestId("stuck-badge")).toBeNull();
    });

    it("shows exactly ONE STUCK badge in the StatusBar (not duplicated in KpiBar)", () => {
      render(<StatusBar graph={fakeGraph} connected={true} error={null} stuckCount={3} />);
      expect(screen.getAllByTestId("stuck-badge")).toHaveLength(1);
      // And the badge announces the count visibly.
      expect(screen.getByTestId("stuck-badge").textContent).toMatch(/3/);
    });

    it("clicking the stuck badge invokes onStuckClick", () => {
      const spy = vi.fn();
      render(
        <StatusBar
          graph={fakeGraph}
          connected={true}
          error={null}
          stuckCount={2}
          onStuckClick={spy}
        />,
      );
      fireEvent.click(screen.getByTestId("stuck-badge"));
      expect(spy).toHaveBeenCalledOnce();
    });
  });
});
