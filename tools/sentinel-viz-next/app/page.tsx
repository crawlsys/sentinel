"use client";

import { useEffect, useMemo, useState } from "react";

import { EventTicker } from "../components/EventTicker";
import { PanelInspector } from "../components/PanelInspector";
import { SessionConsole } from "../components/SessionConsole";
import { SessionStripsPanel } from "../components/SessionStripsPanel";
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

  const { graph, error, connected, liveness } = useGraphStream(pendingFocusSession);

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
  // P3-29 (corrects P3-28 overcorrection). The earlier
  // `status in {dormant, dead}` filter dropped sessions the bridge
  // marks dormant after only a few minutes of inactivity — even
  // when those sessions are the operator's primary work, so the
  // ticker went absurdly sparse.
  //
  // New rule, computed per session:
  //   - dead status        → dormant (always filter)
  //   - awaiting_user      → ALWAYS keep (stuck sessions are signal)
  //   - newest event > 6h  → dormant
  //   - otherwise          → keep
  //
  // Also handles sessions appearing in graph.events without a node
  // (K_SESSIONS overflow): use the event ts to decide freshness.
  const dormantSessionIds = useMemo(() => {
    const set = new Set<string>();
    if (!graph) return set;
    const STALE_AFTER_MS = 6 * 60 * 60 * 1000; // 6h
    const now = Date.now();

    // sid → newest event ts (ms) — built from events list.
    const newestEventTs = new Map<string, number>();
    for (const e of graph.events) {
      const sid =
        typeof e.payload?.session_id === "string" ? (e.payload.session_id as string) : null;
      if (!sid) continue;
      // Bridge writes ts_sec without TZ; treat as UTC.
      const tsStr =
        typeof e.payload?.ts_sec === "string"
          ? (e.payload.ts_sec as string)
          : typeof e.payload?.ts === "string"
            ? (e.payload.ts as string)
            : e.ts;
      const parseable = /T\d{2}:\d{2}:\d{2}(\.\d+)?$/.test(tsStr) ? `${tsStr}Z` : tsStr;
      const t = Date.parse(parseable);
      if (Number.isNaN(t)) continue;
      const cur = newestEventTs.get(sid);
      if (cur == null || t > cur) newestEventTs.set(sid, t);
    }

    // Status-aware pass: awaiting_user always kept, dead always
    // filtered, otherwise defer to age.
    const nodeStatus = new Map<string, string>();
    for (const n of graph.nodes) {
      if (n.type !== "SentinelSession") continue;
      const sid = typeof n.data?.session_id === "string" ? (n.data.session_id as string) : null;
      if (sid && n.session_status) nodeStatus.set(sid, n.session_status);
    }

    const allSids = new Set<string>([...newestEventTs.keys(), ...nodeStatus.keys()]);
    for (const sid of allSids) {
      const status = nodeStatus.get(sid);
      // Stuck sessions always visible.
      if (status === "awaiting_user") continue;
      // Event ts is the source of truth — the bridge's "dead"
      // status fires after 30min idle, but a session with a 35-
      // minute-old event is NOT dead from the operator's point
      // of view. Only filter when both signals agree (no recent
      // event AND the bridge already gave up on it), OR when
      // there's clearly no recent activity.
      const newest = newestEventTs.get(sid);
      if (newest == null) {
        // No event ts: defer to node status.
        if (status === "dead") set.add(sid);
        continue;
      }
      // We have an event ts. Use it as the truth.
      if (now - newest > STALE_AFTER_MS) {
        set.add(sid);
      }
    }
    return set;
  }, [graph]);
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
    else if (sid) selectSessionBySid(sid, ts);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [auto.on, graph?.max_seq]);

  function selectNode(nodeId: string | null, ts?: string) {
    setSelectedNodeId(nodeId);
    setAnchorTs(ts ?? null);
  }

  // P3-36 bug fix: session node IDs are `SentinelSession#<seq>`
  // not `SentinelSession#<session_id>`. Centralise the lookup so
  // every caller that wants to select a session by sid resolves
  // it to the actual graph-node id once. Returns null when the
  // session isn't currently in graph.nodes.
  function selectSessionBySid(sid: string | null, ts?: string) {
    if (!sid) {
      selectNode(null);
      return;
    }
    const node = graph?.nodes.find(
      (n) =>
        n.type === "SentinelSession" &&
        typeof n.data?.session_id === "string" &&
        (n.data.session_id as string) === sid,
    );
    selectNode(node?.id ?? null, ts);
  }

  // sid → graph-node id map, passed into children that take a
  // bare session_id from their event payloads (EventTicker) and
  // need to translate it to a clickable node-id.
  const sessionNodeIds = useMemo(() => {
    const m = new Map<string, string>();
    if (!graph) return m;
    for (const n of graph.nodes) {
      if (n.type !== "SentinelSession") continue;
      const sid = n.data?.session_id;
      if (typeof sid === "string") m.set(sid, n.id);
    }
    return m;
  }, [graph]);

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
        liveness={liveness}
        error={error}
        stuckCount={stuck.length}
        onStuckClick={focusFirstStuck}
        onOpenSettings={() => setSettingsOpen(true)}
        autoOn={auto.on}
        autoReason={auto.reason}
        onToggleAuto={() => auto.set(!auto.on)}
      />
      {/* Below md (~768px) we stack the three panels vertically so
          the graph SVG doesn't collapse to 0px under the 360px-wide
          siblings. The outer container becomes scrollable on
          mobile; on desktop the inner row layout returns and
          overflow stays clipped. */}
      <div className="flex flex-col md:flex-row flex-1 min-h-0 overflow-y-auto md:overflow-hidden">
        <div className="flex-1 min-w-0 min-h-[50vh] md:min-h-0 relative">
          {/* P3-31: replaced the force-directed GraphCanvas with
              multi-sparkline session strips. The graph showed
              every tool call as a node; with the P3-29 backlog
              bump that became 300-node spaghetti. Strips give a
              denser, more legible view of per-session rhythm
              across a configurable window. */}
          <SessionStripsPanel
            graph={graph}
            stuck={stuck}
            dormantSessionIds={dormantSessionIds}
            selectedSessionId={selectedSessionId}
            onSelectSession={(sid) => selectSessionBySid(sid)}
          />
          {!graph ? (
            <div
              data-testid="loading-overlay"
              className="absolute bottom-3 right-3 flex items-center gap-2 text-[#6e7681] font-mono text-[10px] uppercase tracking-wider pointer-events-none"
            >
              <div className="w-3 h-3 border-2 border-[#30363d] border-t-[#58a6ff] rounded-full animate-spin" />
              <span>{error ?? "loading sentinel snapshot…"}</span>
            </div>
          ) : null}
        </div>
        {/* P3-36: mobile-modal shell. Below md the inspector sits
            far down the stacked column — taps on ticker / strips
            look dead because the inspector is off-screen. Now:
              - mobile + selection → fixed inset-0 modal overlay
                with backdrop. Tap backdrop OR the X button closes.
              - mobile + no selection → display:none so it doesn't
                steal space below the events feed.
              - md+ → `contents` lets the existing flex-row layout
                continue rendering the inspector as a side panel
                exactly as before. */}
        <div
          data-testid="inspector-shell"
          data-modal-open={selectedNode ? "true" : undefined}
          onClick={(e) => {
            // Backdrop click closes — only when the click landed
            // on the shell itself, not on the inspector content.
            if (e.target === e.currentTarget) selectNode(null);
          }}
          className={`md:contents ${
            selectedNode
              ? "fixed inset-0 z-40 bg-black/70 flex flex-col"
              : "hidden"
          }`}
        >
          <PanelInspector
            node={selectedNode}
            anchorTs={anchorTs}
            onClose={() => selectNode(null)}
          />
        </div>
        <EventTicker
          events={graph?.events ?? []}
          onSelectNode={(id, ts) => selectNode(id, ts)}
          sessionColors={sessionColors}
          stuckMeta={stuckMeta}
          dormantSessionIds={dormantSessionIds}
          sessionNodeIds={sessionNodeIds}
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
