"use client";

import * as d3Force from "d3-force";
import * as d3Zoom from "d3-zoom";
import { select } from "d3-selection";
import "d3-transition"; // side-effect: extends Selection.prototype.transition
import { useEffect, useMemo, useRef, useState } from "react";

import type { Edge, GraphResponse, Node } from "../types/api";
import { nodeColor, statusColor } from "../lib/format";
import { ensureName, subscribe as subscribeSessionNames } from "../lib/session-names";

interface SimNode extends d3Force.SimulationNodeDatum {
  id: string;
  kind: string;
  outcome?: string;
  session_status?: string;
  category?: string;
  /** 0-based rank from each session's chain head (most recent TC = 0).
   *  Undefined for sessions and TCs that aren't on a derived chain. */
  chainRank?: number;
  /** The session_id this node belongs to (string), or null for
   *  orphan nodes. Used to look up the per-session anchor. */
  sid?: string | null;
  ref: Node;
}

/** Position assigned to a session within the viewport. The session
 *  bubble pins HERE; its TCs spiral outward from this point. */
interface SessionAnchor {
  sid: string;
  /** Centre x/y in sim coordinates (sim is centred at 0,0). */
  cx: number;
  cy: number;
}

/// Golden-angle spiral step. Pi * (3 - sqrt(5)) ≈ 2.39996 rad ≈ 137.5°.
/// Same constant sunflower seed-head spacing uses; gives evenly packed
/// nodes with no two ever colliding angularly.
const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));
/// Base spacing between adjacent TCs along the spiral.
const SPIRAL_BASE_R = 22;

function spiralOffset(rank: number): { dx: number; dy: number } {
  // Rank 0 (chain head) sits very close to the centre; later ranks
  // spiral outward proportional to sqrt(rank).
  const r = SPIRAL_BASE_R * Math.sqrt(rank + 0.5);
  const theta = rank * GOLDEN_ANGLE;
  return { dx: Math.cos(theta) * r, dy: Math.sin(theta) * r };
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
  sessionColors?: Map<string, string>;
}

interface ViewportSize {
  width: number;
  height: number;
}

/** Label rendered next to a SentinelSession node. Asks the
 *  naming subsystem for a cached human name; falls back to UUID
 *  slice when naming is disabled or hasn't returned yet. */
function sessionLabel(d: SimNode): string {
  if (d.kind !== "SentinelSession") return "";
  const sid = typeof d.ref.data?.session_id === "string" ? (d.ref.data.session_id as string) : d.id;
  if (!sid) return "";
  const named = ensureName(sid);
  if (typeof named === "string" && named.length > 0) return named;
  // Fall back to UUID slice (8 chars).
  return sid.length > 12 ? `${sid.slice(0, 8)}…` : sid;
}

/** Label rendered next to a SentinelToolCall node. Only labels the
 *  last 5 TCs per session (the "recent chain") so the eye finds the
 *  active head; older calls in the chain are unlabelled and fade out. */
function tcLabel(d: SimNode): string {
  if (d.kind !== "SentinelToolCall") return "";
  if (d.chainRank == null || d.chainRank > 4) return "";
  const tool = typeof d.ref.data?.tool === "string" ? (d.ref.data.tool as string) : "";
  if (!tool) return "";
  return tool;
}

/** Opacity by chain rank. Head of the chain (rank 0) is full; each
 *  step back fades by ~0.12. After ~6 hops the node nearly disappears.
 *  Non-chain nodes (sessions, prompts) stay full. */
function chainOpacity(d: SimNode): number {
  if (d.chainRank == null) return 1.0;
  return Math.max(0.18, 1.0 - d.chainRank * 0.14);
}

/** Compute per-TC chain rank by walking `next_tool_call` edges.
 *  Each session's chain is laid out chronologically; we walk from
 *  the tail (the TC that no `next_tool_call` points OUT FROM, i.e.
 *  the most-recent TC) and assign 0,1,2,... back along the chain. */
function annotateChainRanks(nodes: SimNode[], links: SimLink[]): void {
  // Build directed adjacency on next_tool_call edges only.
  const inbound = new Map<string, string>(); // target → source
  const outbound = new Map<string, string>(); // source → target
  for (const l of links) {
    if (l.kind !== "next_tool_call") continue;
    const s = typeof l.source === "string" ? l.source : l.source.id;
    const t = typeof l.target === "string" ? l.target : l.target.id;
    inbound.set(t, s);
    outbound.set(s, t);
  }
  // Tails of chains: TC nodes with inbound but no outbound (last in their session).
  // We also seed isolated TCs with rank 0 so they get labelled if there are
  // any non-chain TCs in the window.
  for (const n of nodes) {
    if (n.kind !== "SentinelToolCall") continue;
    if (outbound.has(n.id)) continue;
    // walk backwards assigning rank.
    let cur: string | undefined = n.id;
    let rank = 0;
    const seen = new Set<string>();
    while (cur && !seen.has(cur)) {
      seen.add(cur);
      const node = nodes.find((x) => x.id === cur);
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

export function GraphCanvas({ graph, selectedNodeId, onSelectNode, sessionColors }: Props) {
  const svgRef = useRef<SVGSVGElement | null>(null);
  const gRef = useRef<SVGGElement | null>(null);
  const simRef = useRef<d3Force.Simulation<SimNode, SimLink> | null>(null);
  const zoomRef = useRef<d3Zoom.ZoomBehavior<SVGSVGElement, unknown> | null>(null);
  const nodesRef = useRef<SimNode[]>([]);
  const linksRef = useRef<SimLink[]>([]);
  const onSelectRef = useRef(onSelectNode);
  onSelectRef.current = onSelectNode;
  // Session anchor map kept in a ref so the d3 forces (set up once,
  // accessor closures) can read the freshest value without rebuilding
  // the sim on every data change.
  const anchorsRef = useRef<Map<string, SessionAnchor>>(new Map());
  const [viewport, setViewport] = useState<ViewportSize>({ width: 0, height: 0 });
  // Bump on every session-name arrival so labels redraw.
  const [nameTick, setNameTick] = useState(0);
  useEffect(() => subscribeSessionNames(() => setNameTick((n) => n + 1)), []);

  // Imperatively refresh text labels when names arrive. The data
  // effect won't re-run (graph didn't change) but the cache did.
  useEffect(() => {
    if (!gRef.current) return;
    select(gRef.current)
      .selectAll<SVGGElement, SimNode>("g.node")
      .select<SVGTextElement>("text")
      .text((d) => sessionLabel(d));
  }, [nameTick]);

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

    // Per-session anchors (forceX / forceY) replace the central
    // forceCenter. Each TC/session is pulled toward its session's
    // assigned slot; orphan nodes (no anchor) drift to origin.
    const anchorX = (d: SimNode): number => {
      if (!d.sid) return 0;
      const a = anchorsRef.current.get(d.sid);
      return a ? a.cx : 0;
    };
    const anchorY = (d: SimNode): number => {
      if (!d.sid) return 0;
      const a = anchorsRef.current.get(d.sid);
      return a ? a.cy : 0;
    };

    const sim = d3Force
      .forceSimulation<SimNode, SimLink>(nodesRef.current)
      .force("charge", d3Force.forceManyBody<SimNode>().strength(-90))
      .force(
        "link",
        d3Force
          .forceLink<SimNode, SimLink>(linksRef.current)
          .id((d) => d.id)
          .distance((l) => (l.kind === "next_tool_call" ? 30 : 55))
          .strength((l) => (l.kind === "next_tool_call" ? 0.85 : 0.45)),
      )
      .force(
        "x",
        d3Force
          .forceX<SimNode>(anchorX)
          // Session bubbles get pinned hard at the anchor; their TCs
          // are pulled gently so the chain can fan around the centre.
          .strength((d) => (d.kind === "SentinelSession" ? 0.45 : 0.12)),
      )
      .force(
        "y",
        d3Force
          .forceY<SimNode>(anchorY)
          .strength((d) => (d.kind === "SentinelSession" ? 0.45 : 0.12)),
      )
      .force("collide", d3Force.forceCollide<SimNode>().radius(16))
      .alphaDecay(0.04);
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
    if (!graph) return { nodes: [] as SimNode[], links: [] as SimLink[], anchors: new Map<string, SessionAnchor>() };
    const nodes: SimNode[] = graph.nodes.map((n) => ({
      id: n.id,
      kind: n.type,
      outcome: typeof n.data?.outcome === "string" ? (n.data.outcome as string) : undefined,
      session_status: n.session_status ?? undefined,
      category: n.category ?? undefined,
      sid: typeof n.data?.session_id === "string" ? (n.data.session_id as string) : null,
      ref: n,
    }));
    const ids = new Set(nodes.map((n) => n.id));
    const links: SimLink[] = graph.edges
      .filter((e) => ids.has(e.source) && ids.has(e.target))
      .map((e: Edge) => ({ source: e.source, target: e.target, kind: e.type }));

    // Assign each session a fixed anchor slot. With K=4 the natural
    // layout is a 2x2 grid; arrange them by seq so ordering is
    // stable across SSE ticks.
    const sessionSids: string[] = nodes
      .filter((n) => n.kind === "SentinelSession" && n.sid)
      .sort((a, b) => (a.ref.seq ?? 0) - (b.ref.seq ?? 0))
      .map((n) => n.sid as string);
    const RADIUS = 260; // sim units from origin to each anchor
    const anchors = new Map<string, SessionAnchor>();
    sessionSids.forEach((sid, i) => {
      // Distribute around a circle so any N sessions look balanced.
      const angle = (i / Math.max(sessionSids.length, 1)) * Math.PI * 2 - Math.PI / 2;
      anchors.set(sid, {
        sid,
        cx: Math.cos(angle) * RADIUS,
        cy: Math.sin(angle) * RADIUS,
      });
    });
    return { nodes, links, anchors };
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

    // Annotate chain ranks per session before handing to the sim —
    // labels, fade, AND the golden-spiral position all key off this.
    annotateChainRanks(nextNodes, desired.links);

    // Deterministic layout: pin every node at its computed position
    // via fx/fy. This bypasses d3-force entirely for placement.
    //   - Sessions sit at their anchor slot (circle around origin).
    //   - TCs spiral outward from their session's anchor along a
    //     golden-angle Fibonacci spiral, ordered by chain rank
    //     (rank 0 = head = nearest the centre).
    //   - Hooks / orphans drift via the sim (rare).
    for (const n of nextNodes) {
      if (n.kind === "SentinelSession") {
        const a = n.sid ? desired.anchors.get(n.sid) : null;
        if (a) {
          n.fx = a.cx;
          n.fy = a.cy;
          n.x = a.cx;
          n.y = a.cy;
        }
      } else if (n.kind === "SentinelToolCall" && n.sid && n.chainRank != null) {
        const a = desired.anchors.get(n.sid);
        if (a) {
          const off = spiralOffset(n.chainRank);
          n.fx = a.cx + off.dx;
          n.fy = a.cy + off.dy;
          n.x = a.cx + off.dx;
          n.y = a.cy + off.dy;
        }
      } else {
        // Free node — clear any previous pin.
        n.fx = null;
        n.fy = null;
      }
    }

    nodesRef.current = nextNodes;
    linksRef.current = desired.links;
    anchorsRef.current = desired.anchors;

    sim.nodes(nextNodes);
    (sim.force("link") as d3Force.ForceLink<SimNode, SimLink> | null)?.links(desired.links);
    // PERF: only restart the sim if topology actually changed (new
    // node OR new edge). Pure label/status/age churn is the common
    // SSE case, and that doesn't need a layout pass — every node is
    // pinned via fx/fy anyway. Previously we called .alpha(0.18)
    // .restart() on every SSE tick (250ms), which kept the rAF tick
    // loop perpetually warm for ~4s after each restart.
    const prevIds = prev;
    const newNodeArrived = nextNodes.some((n) => !prevIds.has(n.id));
    const prevEdgeKeys = new Set(
      linksRef.current.map(
        (l) => `${typeof l.source === "string" ? l.source : l.source.id}|${typeof l.target === "string" ? l.target : l.target.id}|${l.kind}`,
      ),
    );
    const newEdgeArrived = desired.links.some(
      (l) =>
        !prevEdgeKeys.has(
          `${typeof l.source === "string" ? l.source : l.source.id}|${typeof l.target === "string" ? l.target : l.target.id}|${l.kind}`,
        ),
    );
    if (newNodeArrived || newEdgeArrived || nodesRef.current.length === 0) {
      // First paint OR genuine topology change → warm the sim.
      sim.alpha(0.18).restart();
    }
    // Otherwise: status / age churn — every node is pinned via
    // fx/fy so layout cannot change, and existing g.node + line.edge
    // DOM nodes already have the right transforms / endpoints from
    // the prior tick. Skipping .restart() entirely keeps the rAF
    // loop cold during steady-state SSE traffic.

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
            .attr("fill", (d) => {
              if (d.kind === "SentinelSession") {
                const sc = d.sid ? sessionColors?.get(d.sid) : undefined;
                return sc ?? statusColor(d.session_status);
              }
              return nodeColor(d.kind, d.outcome, d.category);
            })
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
            .attr("x", 10)
            .attr("y", 4)
            .attr("font-size", 9)
            .attr("fill", "#c9d1d9")
            .attr("opacity", (d) => (d.kind === "SentinelSession" ? 1 : 0.7))
            .text((d) => (d.kind === "SentinelSession" ? sessionLabel(d) : tcLabel(d)));
          // Apply chain-rank fade to the whole group.
          grp.attr("opacity", (d) => chainOpacity(d));
          return grp;
        },
        (update) => {
          // Re-paint in place — colours / status may have changed.
          update
            .attr("data-category", (d) => d.category ?? "")
            .attr("data-status", (d) => d.session_status ?? "")
            .attr("opacity", (d) => chainOpacity(d));
          update.select("circle:not(.pulse-ring)")
            .attr("fill", (d) => {
              if (d.kind === "SentinelSession") {
                const sc = d.sid ? sessionColors?.get(d.sid) : undefined;
                return sc ?? statusColor(d.session_status);
              }
              return nodeColor(d.kind, d.outcome, d.category);
            });
          // Refresh label text — names may have arrived from the
          // naming API; chain ranks may have shifted on SSE update.
          update
            .select("text")
            .text((d) => (d.kind === "SentinelSession" ? sessionLabel(d) : tcLabel(d)));
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
        const isSelected = d.id === selectedNodeId;
        // Primary circle (first <circle> only — second is the pulse-ring).
        grp
          .select<SVGCircleElement>("circle:not(.pulse-ring)")
          .attr("stroke", isSelected ? "#58a6ff" : "#0d1117")
          .attr("stroke-width", isSelected ? 3 : 1.5);
        // Drop any prior burst — only the freshest click animates.
        grp.selectAll<SVGGElement, unknown>(":scope > g.click-burst").remove();
        // Drop any prior selected-marker — only the current selection has one.
        grp.selectAll<SVGGElement, unknown>(":scope > g.selected-marker").remove();
        if (isSelected) {
          // Persistent halo — survives SSE re-renders. Two concentric
          // circles, the outer pulses + dashed for unmissable presence.
          const marker = grp.append("g").attr("class", "selected-marker");
          marker
            .append("circle")
            .attr("class", "halo-inner")
            .attr("r", 13)
            .attr("cx", 0)
            .attr("cy", 0);
          marker
            .append("circle")
            .attr("class", "halo-outer")
            .attr("r", 18)
            .attr("cx", 0)
            .attr("cy", 0);
          // One-shot burst on top (animates outward 0.7s).
          const burst = grp.append("g").attr("class", "click-burst");
          burst.append("circle").attr("cx", 0).attr("cy", 0);
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
