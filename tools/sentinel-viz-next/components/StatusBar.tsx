"use client";

import type { GraphResponse } from "../types/api";

interface Props {
  graph: GraphResponse | null;
  connected: boolean;
  error: string | null;
  stuckCount?: number;
  onStuckClick?: () => void;
}

export function StatusBar({ graph, connected, error, stuckCount = 0, onStuckClick }: Props) {
  return (
    <div
      data-testid="status-bar"
      className="flex items-center gap-4 px-3 py-1.5 border-b border-[#30363d] bg-[#161b22] text-[10px] uppercase tracking-wider text-[#6e7681] font-mono"
    >
      <span className="text-[#58a6ff] font-bold">sentinel-viz-next</span>
      <span className={connected ? "text-[#3fb950]" : graph ? "text-[#58a6ff]" : "text-[#d29922]"}>
        {connected ? "● live" : graph ? "● ready" : "○ connecting"}
      </span>
      {graph ? (
        <>
          <span>nodes: {graph.stats.nodes_total}</span>
          <span>edges: {graph.stats.edges_total}</span>
          <span>events: {graph.stats.events_total}</span>
          <span>seq: {graph.max_seq}</span>
          <span className="text-[#484f58]">
            corpus: {graph.stats.corpus_nodes} / {graph.stats.corpus_edges}
          </span>
        </>
      ) : (
        <span>waiting on first snapshot…</span>
      )}
      {stuckCount > 0 ? (
        <button
          type="button"
          onClick={onStuckClick}
          data-testid="stuck-badge"
          className="ml-auto px-2 py-0.5 rounded bg-[#3a0f0f] border border-[#f85149] text-[#f85149] font-bold animate-pulse hover:bg-[#5a1717]"
          title="Sessions awaiting you for >15min — click to focus"
        >
          STUCK: {stuckCount}
        </button>
      ) : null}
      {error ? <span className={`text-[#f85149] ${stuckCount > 0 ? "ml-2" : "ml-auto"}`}>{error}</span> : null}
    </div>
  );
}
