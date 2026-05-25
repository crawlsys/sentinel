"use client";

import * as d3Force from "d3-force";
import * as d3Zoom from "d3-zoom";
import { select } from "d3-selection";
import "d3-transition"; // side-effect: extends Selection.prototype.transition
import { useEffect, useMemo, useRef, useState } from "react";

import type { Edge, GraphResponse, Node } from "../types/api";
import { nodeColor, statusColor } from "../lib/format";

interface SimNode extends d3Force.SimulationNodeDatum {
  id: string;
  kind: string;
  outcome?: string;
  session_status?: string;
  category?: string;
  ref: Node;
}

interface SimLink extends d3Force.SimulationLinkDatum<SimNode> {
  source: string | SimNode;
  target: string | SimNode;
  kind: string;
}

interface Props {
  graph: GraphResponse | null;
  selectedNodeId: string | null;
  onSelectNode: (nodeId: string | null) => void;
}

interface ViewportSize {
  width: number;
  height: number;
}

export function GraphCanvas({ graph, selectedNodeId, onSelectNode }: Props) {
  const svgRef = useRef<SVGSVGElement | null>(null);
  const gRef = useRef<SVGGElement | null>(null);
  const simRef = useRef<d3Force.Simulation<SimNode, SimLink> | null>(null);
  const zoomRef = useRef<d3Zoom.ZoomBehavior<SVGSVGElement, unknown> | null>(null);
  const nodesRef = useRef<SimNode[]>([]);
  const linksRef = useRef<SimLink[]>([]);
  const onSelectRef = useRef(onSelectNode);
  onSelectRef.current = onSelectNode;
  const [viewport, setViewport] = useState<ViewportSize>({ width: 0, height: 0 });

  // Track viewport size.
  useEffect(() => {
    const svg = svgRef.current;
    if (!svg) return;
    const update = () => {
      const r = svg.getBoundingClientRect();
      setViewport((prev) =>
        Math.round(prev.width) === Math.round(r.width)
          && Math.round(prev.height) === Math.round(r.height)
          ? prev
          : { width: r.width, height: r.height },
      );
    };
    update();
    if (typeof ResizeObserver === "undefined") {
      window.addEventListener("resize", update);
      return () => window.removeEventListener("resize", update);
    }
    const obs = new ResizeObserver(update);
    obs.observe(svg);
    return () => obs.disconnect();
  }, []);

  // One-time setup: create the simulation, zoom, and SVG groups.
  // We never tear this down across data updates.
  useEffect(() => {
    if (!svgRef.current || !gRef.current) return;
    const svg = select(svgRef.current);
    const g = select(gRef.current);

    const sim = d3Force
      .forceSimulation<SimNode, SimLink>(nodesRef.current)
      .force("charge", d3Force.forceManyBody<SimNode>().strength(-180))
      .force(
        "link",
        d3Force
          .forceLink<SimNode, SimLink>(linksRef.current)
          .id((d) => d.id)
          .distance((l) => (l.kind === "next_tool_call" ? 35 : 70))
          .strength((l) => (l.kind === "next_tool_call" ? 0.9 : 0.4)),
      )
      .force("center", d3Force.forceCenter(0, 0))
      .force("collide", d3Force.forceCollide<SimNode>().radius(18))
      .alphaDecay(0.05);
    simRef.current = sim;

    sim.on("tick", () => {
      g.selectAll<SVGLineElement, SimLink>("line.edge")
        .attr("x1", (d) => (typeof d.source === "object" ? d.source.x ?? 0 : 0))
        .attr("y1", (d) => (typeof d.source === "object" ? d.source.y ?? 0 : 0))
        .attr("x2", (d) => (typeof d.target === "object" ? d.target.x ?? 0 : 0))
        .attr("y2", (d) => (typeof d.target === "object" ? d.target.y ?? 0 : 0));
      g.selectAll<SVGGElement, SimNode>("g.node")
        .attr("transform", (d) => `translate(${d.x ?? 0}, ${d.y ?? 0})`);
    });

    const zoom = d3Zoom
      .zoom<SVGSVGElement, unknown>()
      .scaleExtent([0.2, 4])
      .on("zoom", (event) => {
        g.attr("transform", event.transform.toString());
      });
    zoomRef.current = zoom;
    svg.call(zoom);
    // Initial centre at origin; data effect will recentre once we know the viewport.

    return () => {
      sim.stop();
      svg.on(".zoom", null);
      simRef.current = null;
      zoomRef.current = null;
    };
  }, []);

  // Memoise the desired node/link set from the graph prop.
  const desired = useMemo(() => {
    if (!graph) return { nodes: [] as SimNode[], links: [] as SimLink[] };
    const nodes: SimNode[] = graph.nodes.map((n) => ({
      id: n.id,
      kind: n.type,
      outcome: typeof n.data?.outcome === "string" ? (n.data.outcome as string) : undefined,
      session_status: n.session_status ?? undefined,
      category: n.category ?? undefined,
      ref: n,
    }));
    const ids = new Set(nodes.map((n) => n.id));
    const links: SimLink[] = graph.edges
      .filter((e) => ids.has(e.source) && ids.has(e.target))
      .map((e: Edge) => ({ source: e.source, target: e.target, kind: e.type }));
    return { nodes, links };
  }, [graph]);

  // Incremental data update: preserve positions for nodes that survive,
  // seed new ones near the centroid of the existing layout, then warm
  // the sim gently. This is what gives the "pulse" feel instead of a
  // jerky re-layout on every SSE tick.
  useEffect(() => {
    if (!simRef.current || !svgRef.current || !gRef.current) return;
    const sim = simRef.current;
    const g = select(gRef.current);

    const prev = new Map(nodesRef.current.map((n) => [n.id, n]));
    const nextNodes: SimNode[] = desired.nodes.map((n) => {
      const old = prev.get(n.id);
      if (old) {
        // Carry forward physics state so the node stays put across renders.
        return Object.assign(n, {
          x: old.x,
          y: old.y,
          vx: old.vx,
          vy: old.vy,
          fx: old.fx,
          fy: old.fy,
        });
      }
      return n;
    });

    nodesRef.current = nextNodes;
    linksRef.current = desired.links;

    sim.nodes(nextNodes);
    (sim.force("link") as d3Force.ForceLink<SimNode, SimLink> | null)?.links(desired.links);
    // Tiny warm-up so new nodes settle, existing ones barely budge.
    sim.alpha(0.18).restart();

    // Render edges
    g.selectAll<SVGLineElement, SimLink>("line.edge")
      .data(desired.links, (d) => `${typeof d.source === "string" ? d.source : d.source.id}->${typeof d.target === "string" ? d.target : d.target.id}:${d.kind}`)
      .join(
        (enter) =>
          enter
            .append("line")
            .attr("class", "edge")
            .attr("stroke", (d) => (d.kind === "next_tool_call" ? "#58a6ff" : "#30363d"))
            .attr("stroke-width", (d) => (d.kind === "next_tool_call" ? 1.2 : 0.7))
            .attr("stroke-opacity", (d) => (d.kind === "next_tool_call" ? 0.8 : 0.5)),
        (update) => update,
        (exit) => exit.remove(),
      );

    // Render nodes
    g.selectAll<SVGGElement, SimNode>("g.node")
      .data(nextNodes, (d) => d.id)
      .join(
        (enter) => {
          const grp = enter
            .append("g")
            .attr("class", "node")
            .attr("data-node-id", (d) => d.id)
            .attr("data-category", (d) => d.category ?? "")
            .attr("data-status", (d) => d.session_status ?? "")
            .attr("data-kind", (d) => d.kind)
            .style("cursor", "pointer")
            .on("click", (_, d) => onSelectRef.current(d.id));
          // Primary circle. Per-status liveness pulses are CSS-driven
          // off the `data-status` attribute (see globals.css).
          grp
            .append("circle")
            .attr("r", (d) =>
              (d.kind === "SentinelSession" ? 9 : d.kind === "SentinelToolCall" ? 6 : 4) * 1.5,
            )
            .attr("fill", (d) =>
              d.kind === "SentinelSession"
                ? statusColor(d.session_status)
                : nodeColor(d.kind, d.outcome, d.category),
            )
            .attr("stroke", "#58a6ff")
            .attr("stroke-width", 2)
            .transition()
            .duration(600)
            .attr("r", (d) =>
              d.kind === "SentinelSession" ? 9 : d.kind === "SentinelToolCall" ? 6 : 4,
            )
            .attr("stroke", "#0d1117")
            .attr("stroke-width", 1.5);
          // Concentric pulse ring — hidden by CSS unless data-status
          // is firing / busy / awaiting_user. Animation defined in
          // globals.css (`ring-expand-*` keyframes).
          grp
            .append("circle")
            .attr("class", "pulse-ring")
            .attr("r", 12)
            .attr("fill", "none");
          grp
            .append("text")
            .attr("x", 12)
            .attr("y", 4)
            .attr("font-size", 9)
            .attr("fill", "#c9d1d9")
            .text((d) => {
              if (d.kind === "SentinelSession") {
                const sid = typeof d.ref.data?.session_id === "string" ? (d.ref.data.session_id as string) : d.id;
                return sid.length > 12 ? `${sid.slice(0, 8)}…` : sid;
              }
              return "";
            });
          return grp;
        },
        (update) => {
          // Re-paint in place — colours / status may have changed.
          update
            .attr("data-category", (d) => d.category ?? "")
            .attr("data-status", (d) => d.session_status ?? "")
            .select("circle")
            .attr("fill", (d) =>
              d.kind === "SentinelSession"
                ? statusColor(d.session_status)
                : nodeColor(d.kind, d.outcome, d.category),
            );
          return update;
        },
        (exit) => exit.remove(),
      );
  }, [desired]);

  // Initial centring once viewport is known and graph has nodes.
  const initialPanRef = useRef(false);
  useEffect(() => {
    if (initialPanRef.current) return;
    if (!svgRef.current || !zoomRef.current) return;
    if (viewport.width === 0 || nodesRef.current.length === 0) return;
    const centerX = viewport.width / 2;
    const centerY = viewport.height / 2;
    select(svgRef.current).call(
      zoomRef.current.transform,
      d3Zoom.zoomIdentity.translate(centerX, centerY),
    );
    initialPanRef.current = true;
  }, [viewport.width, viewport.height, desired]);

  // Highlight + pan to the selected node, plus one-shot click-burst.
  useEffect(() => {
    if (!gRef.current || !svgRef.current) return;
    select(gRef.current)
      .selectAll<SVGGElement, SimNode>("g.node")
      .each(function (d) {
        const grp = select(this);
        // Primary circle (first <circle> only — second is the pulse-ring).
        grp
          .select<SVGCircleElement>("circle:not(.pulse-ring)")
          .attr("stroke", d.id === selectedNodeId ? "#58a6ff" : "#0d1117")
          .attr("stroke-width", d.id === selectedNodeId ? 3 : 1.5);
        // Drop any prior burst — only the freshest click animates.
        grp.selectAll<SVGGElement, unknown>(":scope > g.click-burst").remove();
        if (d.id === selectedNodeId) {
          const burst = grp.append("g").attr("class", "click-burst");
          burst.append("circle").attr("cx", 0).attr("cy", 0);
          // CSS animation runs 700ms; clean up afterward so we don't
          // accumulate DOM nodes when the same node is re-clicked.
          window.setTimeout(() => burst.remove(), 800);
        }
      });

    if (!selectedNodeId || !zoomRef.current) return;
    const target = nodesRef.current.find((n) => n.id === selectedNodeId);
    if (!target || target.x == null || target.y == null) return;

    const width = viewport.width || svgRef.current.clientWidth || 800;
    const height = viewport.height || svgRef.current.clientHeight || 600;

    // Pan-grace: preserve the user's current zoom unless the node
    // would otherwise be invisible at that scale. Don't yank to a
    // hardcoded k=1.4 every click — the user complained the auto-
    // jump UX was disorienting.
    const cur = d3Zoom.zoomTransform(svgRef.current);
    const targetScreenX = target.x * cur.k + cur.x;
    const targetScreenY = target.y * cur.k + cur.y;
    const EDGE_PAD = 80;
    const nodeOffscreen =
      targetScreenX < EDGE_PAD
      || targetScreenX > width - EDGE_PAD
      || targetScreenY < EDGE_PAD
      || targetScreenY > height - EDGE_PAD;
    const nearCentre =
      Math.abs(targetScreenX - width / 2) < width * 0.22
      && Math.abs(targetScreenY - height / 2) < height * 0.22;
    // If the node is already visible AND roughly centred, leave the
    // viewport alone. The click-burst + stroke highlight are enough
    // signal that something was selected.
    if (!nodeOffscreen && nearCentre) return;

    // If user has zoomed way out (k < 0.5) bring it back to 1.0 for
    // legibility. Otherwise keep their scale.
    const k = cur.k < 0.5 ? 1.0 : cur.k;
    const tx = width / 2 - target.x * k;
    const ty = height / 2 - target.y * k;
    select(svgRef.current)
      .transition()
      .duration(700)
      .ease((t) => t * (2 - t)) // ease-out-quad
      .call(zoomRef.current.transform, d3Zoom.zoomIdentity.translate(tx, ty).scale(k));
  }, [selectedNodeId, viewport.width, viewport.height, desired]);

  return (
    <svg
      ref={svgRef}
      data-testid="graph-canvas"
      width="100%"
      height="100%"
      style={{ background: "#0d1117", display: "block" }}
      role="img"
      aria-label={`Sentinel activity graph with ${desired.nodes.length} nodes`}
    >
      <g ref={gRef} />
    </svg>
  );
}
