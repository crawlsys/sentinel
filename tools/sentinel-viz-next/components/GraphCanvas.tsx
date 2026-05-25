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
  const [viewport, setViewport] = useState<ViewportSize>({ width: 0, height: 0 });

  useEffect(() => {
    const svg = svgRef.current;
    if (!svg) return;

    const updateViewport = () => {
      const rect = svg.getBoundingClientRect();
      setViewport((prev) => {
        if (Math.round(prev.width) === Math.round(rect.width) && Math.round(prev.height) === Math.round(rect.height)) {
          return prev;
        }
        return { width: rect.width, height: rect.height };
      });
    };

    updateViewport();

    if (typeof ResizeObserver === "undefined") {
      window.addEventListener("resize", updateViewport);
      return () => window.removeEventListener("resize", updateViewport);
    }

    const observer = new ResizeObserver(updateViewport);
    observer.observe(svg);
    return () => observer.disconnect();
  }, []);

  // Map graph data to sim nodes/links (stable identity by id).
  const { simNodes, simLinks, nodeById } = useMemo(() => {
    if (!graph) return { simNodes: [] as SimNode[], simLinks: [] as SimLink[], nodeById: new Map<string, SimNode>() };
    const byId = new Map<string, SimNode>();
    const nodes: SimNode[] = graph.nodes.map((n) => {
      const sim: SimNode = {
        id: n.id,
        kind: n.type,
        outcome: typeof n.data?.outcome === "string" ? (n.data.outcome as string) : undefined,
        session_status: n.session_status ?? undefined,
        category: n.category ?? undefined,
        ref: n,
      };
      byId.set(n.id, sim);
      return sim;
    });
    const links: SimLink[] = graph.edges
      .filter((e) => byId.has(e.source) && byId.has(e.target))
      .map((e: Edge) => ({ source: e.source, target: e.target, kind: e.type }));
    return { simNodes: nodes, simLinks: links, nodeById: byId };
  }, [graph]);

  // Build / update the d3 simulation.
  useEffect(() => {
    if (!svgRef.current || !gRef.current) return;

    const width = viewport.width || svgRef.current.clientWidth || 800;
    const height = viewport.height || svgRef.current.clientHeight || 600;
    const centerX = Math.max(width / 2, 0);
    const centerY = Math.max(height / 2, 0);

    const sim = d3Force
      .forceSimulation<SimNode, SimLink>(simNodes)
      .force("charge", d3Force.forceManyBody<SimNode>().strength(-120))
      .force(
        "link",
        d3Force
          .forceLink<SimNode, SimLink>(simLinks)
          .id((d) => d.id)
          .distance(60)
          .strength(0.5),
      )
      .force("center", d3Force.forceCenter(0, 0))
      .force("collide", d3Force.forceCollide<SimNode>().radius(18));
    simRef.current = sim;

    const svg = select(svgRef.current);
    const g = select(gRef.current);

    const link = g
      .selectAll<SVGLineElement, SimLink>("line.edge")
      .data(simLinks, (d) => `${typeof d.source === "string" ? d.source : d.source.id}->${typeof d.target === "string" ? d.target : d.target.id}:${d.kind}`)
      .join((enter) =>
        enter
          .append("line")
          .attr("class", "edge")
          .attr("stroke", "#30363d")
          .attr("stroke-width", 0.7)
          .attr("stroke-opacity", 0.6),
      );

    const node = g
      .selectAll<SVGGElement, SimNode>("g.node")
      .data(simNodes, (d) => d.id)
      .join((enter) => {
        const grp = enter.append("g").attr("class", "node").style("cursor", "pointer");
        grp
          .append("circle")
          .attr("r", (d) => (d.kind === "SentinelSession" ? 9 : d.kind === "SentinelToolCall" ? 6 : 4))
          .attr("fill", (d) =>
            d.kind === "SentinelSession"
              ? statusColor(d.session_status)
              : nodeColor(d.kind, d.outcome, d.category),
          )
          .attr("stroke", "#0d1117")
          .attr("stroke-width", 1.5);
        grp
          .append("text")
          .attr("x", 12)
          .attr("y", 4)
          .attr("font-size", 9)
          .attr("fill", "#c9d1d9")
          .text((d) => {
            if (d.kind === "SentinelSession") {
              const sid = typeof d.ref.data?.session_id === "string" ? (d.ref.data.session_id as string) : d.id;
              return sid.length > 12 ? `${sid.slice(0, 8)}...` : sid;
            }
            return "";
          });
        return grp;
      });

    node.on("click", (_, d) => onSelectNode(d.id));

    sim.on("tick", () => {
      link
        .attr("x1", (d) => (typeof d.source === "object" ? d.source.x ?? 0 : 0))
        .attr("y1", (d) => (typeof d.source === "object" ? d.source.y ?? 0 : 0))
        .attr("x2", (d) => (typeof d.target === "object" ? d.target.x ?? 0 : 0))
        .attr("y2", (d) => (typeof d.target === "object" ? d.target.y ?? 0 : 0));
      node.attr("transform", (d) => `translate(${d.x ?? 0}, ${d.y ?? 0})`);
    });

    // Zoom & pan. Center the simulation in the actual SVG viewport so fixed
    // side panels cannot push the graph outside a narrow canvas.
    const zoom = d3Zoom
      .zoom<SVGSVGElement, unknown>()
      .scaleExtent([0.2, 4])
      .on("zoom", (event) => {
        g.attr("transform", event.transform.toString());
      });
    svg.call(zoom).call(zoom.transform, d3Zoom.zoomIdentity.translate(centerX, centerY));
    zoomRef.current = zoom;
    nodesRef.current = simNodes;

    return () => {
      sim.stop();
      svg.on(".zoom", null);
    };
  }, [simNodes, simLinks, onSelectNode, viewport.width, viewport.height]);

  // Highlight + pan to the selected node.
  useEffect(() => {
    if (!gRef.current || !svgRef.current) return;
    select(gRef.current)
      .selectAll<SVGGElement, SimNode>("g.node")
      .select("circle")
      .attr("stroke", (d) => (d.id === selectedNodeId ? "#58a6ff" : "#0d1117"))
      .attr("stroke-width", (d) => (d.id === selectedNodeId ? 3 : 1.5));

    if (!selectedNodeId || !zoomRef.current) return;
    const target = nodesRef.current.find((n) => n.id === selectedNodeId);
    if (!target || target.x == null || target.y == null) return;

    const width = viewport.width || svgRef.current.clientWidth || 800;
    const height = viewport.height || svgRef.current.clientHeight || 600;
    // Centre on the node at a comfortable zoom level (k=1.4).
    const k = 1.4;
    const tx = width / 2 - target.x * k;
    const ty = height / 2 - target.y * k;
    select(svgRef.current)
      .transition()
      .duration(450)
      .call(zoomRef.current.transform, d3Zoom.zoomIdentity.translate(tx, ty).scale(k));
  }, [selectedNodeId, simNodes, viewport.width, viewport.height]);

  return (
    <svg
      ref={svgRef}
      data-testid="graph-canvas"
      width="100%"
      height="100%"
      style={{ background: "#0d1117", display: "block" }}
      role="img"
      aria-label={`Sentinel activity graph with ${nodeById.size} nodes`}
    >
      <g ref={gRef} />
    </svg>
  );
}
