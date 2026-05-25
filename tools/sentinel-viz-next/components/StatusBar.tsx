"use client";

import type { GraphResponse } from "../types/api";

interface Props {
  graph: GraphResponse | null;
  connected: boolean;
  error: string | null;
}

export function StatusBar({ graph, connected, error }: Props) {
  return (
    <div
      data-testid="status-bar"
      className="flex items-center gap-4 px-3 py-1.5 border-b border-[#30363d] bg-[#161b22] text-[10px] uppercase tracking-wider text-[#6e7681] font-mono"
    >
      <span className="text-[#58a6ff] font-bold">sentinel-viz-next</span>
      <span className={connected ? "text-[#3fb950]" : "text-[#d29922]"}>
        {connected ? "● live" : "○ connecting"}
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
      {error ? <span className="text-[#f85149] ml-auto">{error}</span> : null}
    </div>
  );
}
