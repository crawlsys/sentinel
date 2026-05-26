"use client";

import { useEffect, useMemo, useState } from "react";

import { EventTicker } from "../components/EventTicker";
import { GraphCanvas } from "../components/GraphCanvas";
import { PanelInspector } from "../components/PanelInspector";
import { SessionConsole } from "../components/SessionConsole";
import { SettingsModal } from "../components/SettingsModal";
import { StatusBar } from "../components/StatusBar";
import { sessionColorMap } from "../lib/session-colors";
import { useGraphStream } from "../lib/sse";
import { maybeFireStuckAlert, stuckSessions } from "../lib/stuck";
import { useAutoWatch } from "../lib/auto-watch";

export default function Page() {
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
  const [anchorTs, setAnchorTs] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // We resolve `selectedSessionId` from the previous graph snapshot
  // so the focused-session re-fetch can fire without waiting on the
  // current snapshot's node list (it might not include this session
  // yet). Stored in a state hook to break the SSE→graph→selection
  // chain.
  const [pendingFocusSession, setPendingFocusSession] = useState<string | null>(null);

  const { graph, error, connected } = useGraphStream(pendingFocusSession);

  const selectedNode = useMemo(() => {
    if (!graph || !selectedNodeId) return null;
    return graph.nodes.find((n) => n.id === selectedNodeId) ?? null;
  }, [graph, selectedNodeId]);

  // Resolve the session_id for the selected node (session OR a child
  // tool-call): both shapes expose session_id in data.
  const selectedSessionId = useMemo(() => {
    const sid = selectedNode?.data?.session_id;
    return typeof sid === "string" ? sid : null;
  }, [selectedNode]);

  // Sync the focus state so the SSE/initial-fetch loop re-issues
  // with `focused_session=<sid>` whenever the selection changes.
  useEffect(() => {
    setPendingFocusSession(selectedSessionId);
  }, [selectedSessionId]);

  const stuck = useMemo(() => stuckSessions(graph), [graph]);
  const sessionColors = useMemo(() => sessionColorMap(graph), [graph]);
  const stuckMeta = useMemo(() => {
    const m = new Map<string, import("../components/EventTicker").StuckMeta>();
    for (const n of stuck) {
      const sid = n.data?.session_id;
      if (typeof sid !== "string") continue;
      m.set(sid, {
        ageSecs: n.last_activity_age_s ?? 0,
        kind: n.awaiting_kind ?? null,
        question: n.awaiting_question ?? null,
      });
    }
    return m;
  }, [stuck]);

  const auto = useAutoWatch(false);

  useEffect(() => {
    maybeFireStuckAlert(stuck.length, stuck);
  }, [stuck]);

  // Auto-watch: when on and the graph snapshot ticks, jump selection
  // to the freshest event (latest of graph.events). Operator
  // interaction immediately disables auto via the auto-watch hook,
  // so this only fires while the user is genuinely hands-off.
  useEffect(() => {
    if (!auto.on || !graph || graph.events.length === 0) return;
    const latest = graph.events[graph.events.length - 1];
    const tcid = typeof latest.payload.tool_call_id === "string"
      ? (latest.payload.tool_call_id as string)
      : null;
    const sid = typeof latest.payload.session_id === "string"
      ? (latest.payload.session_id as string)
      : null;
    const ts = typeof latest.payload.ts_sec === "string"
      ? (latest.payload.ts_sec as string)
      : (typeof latest.payload.ts === "string" ? (latest.payload.ts as string) : latest.ts);
    if (tcid) selectNode(tcid, ts);
    else if (sid) selectNode(`SentinelSession#${sid}`, ts);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [auto.on, graph?.max_seq]);

  function selectNode(nodeId: string | null, ts?: string) {
    setSelectedNodeId(nodeId);
    setAnchorTs(ts ?? null);
  }

  function focusFirstStuck() {
    if (stuck.length === 0) return;
    const first = stuck[0];
    const ts = typeof first.data?.started_at === "string" ? (first.data.started_at as string) : undefined;
    selectNode(first.id, ts);
  }

  return (
    <main className="flex flex-col h-screen">
      <StatusBar
        graph={graph}
        connected={connected}
        error={error}
        stuckCount={stuck.length}
        onStuckClick={focusFirstStuck}
        onOpenSettings={() => setSettingsOpen(true)}
        autoOn={auto.on}
        autoReason={auto.reason}
        onToggleAuto={() => auto.set(!auto.on)}
      />
      <div className="flex flex-1 min-h-0">
        <div className="flex-1 min-w-0 min-h-0 relative">
          <GraphCanvas
            graph={graph}
            selectedNodeId={selectedNodeId}
            onSelectNode={(id) => selectNode(id)}
            sessionColors={sessionColors}
          />
          {!graph ? (
            <div
              data-testid="loading-overlay"
              className="absolute inset-0 flex items-center justify-center text-[#6e7681] font-mono text-xs pointer-events-none"
            >
              <div className="flex flex-col items-center gap-3">
                <div className="w-8 h-8 border-2 border-[#30363d] border-t-[#58a6ff] rounded-full animate-spin" />
                <span>{error ?? "connecting to sentinel.db…"}</span>
              </div>
            </div>
          ) : null}
        </div>
        <PanelInspector
          node={selectedNode}
          anchorTs={anchorTs}
          onClose={() => selectNode(null)}
        />
        <EventTicker
          events={graph?.events ?? []}
          onSelectNode={(id, ts) => selectNode(id, ts)}
          sessionColors={sessionColors}
          stuckMeta={stuckMeta}
        />
      </div>
      <SessionConsole
        graph={graph}
        sessionColors={sessionColors}
        selectedSessionId={selectedSessionId}
      />
      <SettingsModal open={settingsOpen} onClose={() => setSettingsOpen(false)} />
    </main>
  );
}
