"use client";

import { useMemo, useState } from "react";

import { EventTicker } from "../components/EventTicker";
import { GraphCanvas } from "../components/GraphCanvas";
import { PanelInspector } from "../components/PanelInspector";
import { StatusBar } from "../components/StatusBar";
import { useGraphStream } from "../lib/sse";

export default function Page() {
  const { graph, error, connected } = useGraphStream();
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);

  const selectedNode = useMemo(() => {
    if (!graph || !selectedNodeId) return null;
    return graph.nodes.find((n) => n.id === selectedNodeId) ?? null;
  }, [graph, selectedNodeId]);

  return (
    <main className="flex flex-col h-screen">
      <StatusBar graph={graph} connected={connected} error={error} />
      <div className="flex flex-1 min-h-0">
        <div className="flex-1 min-w-0 min-h-0 relative">
          <GraphCanvas
            graph={graph}
            selectedNodeId={selectedNodeId}
            onSelectNode={setSelectedNodeId}
          />
        </div>
        <PanelInspector node={selectedNode} onClose={() => setSelectedNodeId(null)} />
        <EventTicker
          events={graph?.events ?? []}
          onSelectNode={(id) => setSelectedNodeId(id)}
        />
      </div>
    </main>
  );
}
