"use client";

import type { GraphResponse } from "../types/api";
import { AUTO_WATCH_IGNORE_ATTR } from "../lib/auto-watch";
import type { StreamLiveness } from "../lib/sse";
import { KpiBar } from "./KpiBar";

interface Props {
  graph: GraphResponse | null;
  /** Boolean form retained for back-compat with tests that only
   *  care about "is there a stream at all". Prefer `liveness`. */
  connected: boolean;
  /** Three-state freshness signal. When omitted, falls back to the
   *  boolean `connected` flag and treats it as live ↔ down. */
  liveness?: StreamLiveness;
  error: string | null;
  stuckCount?: number;
  onStuckClick?: () => void;
  onOpenSettings?: () => void;
  autoOn?: boolean;
  autoReason?: "operator" | "interaction" | "blur" | "idle";
  onToggleAuto?: () => void;
}

interface LivenessLabel {
  text: string;
  color: string;
  glyph: string;
  pulse: boolean;
}

export function livenessLabel(
  liveness: StreamLiveness | undefined,
  connected: boolean,
  graph: GraphResponse | null,
): LivenessLabel {
  // Prefer the explicit three-state signal. Fall back to the
  // boolean if the caller hasn't wired it up yet.
  const effective: StreamLiveness =
    liveness ?? (connected ? "live" : graph ? "down" : "init");
  switch (effective) {
    case "live":
      return { text: "live", color: "#4A9E5C", glyph: "●", pulse: true };
    case "stale":
      // Real signal: data is here but the stream stopped within
      // the last 30s. Operators looking at the dashboard for a
      // decision need to know they're seeing data that may be a
      // few seconds out of date.
      return { text: "stale", color: "#D4A843", glyph: "●", pulse: false };
    case "down":
      // We have a graph but the stream is gone. "ready" is the
      // legacy label; keep it for muscle-memory.
      return graph
        ? { text: "ready", color: "#5B9BF6", glyph: "●", pulse: false }
        : { text: "down", color: "#D71921", glyph: "○", pulse: false };
    case "init":
    default:
      // Graph snapshot fetched but no SSE message yet → "ready"
      // (we have data, just not a live stream). Pre-snapshot we're
      // genuinely connecting.
      return graph
        ? { text: "ready", color: "#5B9BF6", glyph: "●", pulse: false }
        : { text: "connecting", color: "#D4A843", glyph: "○", pulse: false };
  }
}

export function StatusBar({
  graph,
  connected,
  liveness,
  error,
  stuckCount = 0,
  onStuckClick,
  onOpenSettings,
  autoOn = false,
  autoReason = "operator",
  onToggleAuto,
}: Props) {
  const live = livenessLabel(liveness, connected, graph);
  return (
    <div
      data-testid="status-bar"
      className="flex flex-wrap items-center gap-x-4 gap-y-1 px-3 py-1.5 border-b border-[#222] bg-[#111] text-[10px] uppercase tracking-wider text-[#999] font-mono"
    >
      <span className="text-[#5B9BF6] font-bold">sentinel-viz-next</span>
      <span
        data-testid="liveness-indicator"
        data-liveness={liveness ?? (connected ? "live" : graph ? "down" : "init")}
        style={{ color: live.color }}
        className={live.pulse ? "" : ""}
        title={
          live.text === "stale"
            ? "SSE stream paused — data may be a few seconds out of date"
            : live.text === "down"
              ? "SSE stream disconnected — auto-reconnecting"
              : live.text === "live"
                ? "live stream — last update <5s ago"
                : "connecting to sentinel API…"
        }
      >
        {live.glyph} {live.text}
      </span>
      {graph ? (
        <>
          {/* Operator-relevant counts at full contrast. */}
          <span>nodes: {graph.stats.nodes_total}</span>
          <span>edges: {graph.stats.edges_total}</span>
          <span>events: {graph.stats.events_total}</span>
          {/* Dev telemetry — bridge sequence and corpus totals.
              Useful for debugging the bridge / db growth but
              actively noisy in the operator's primary view.
              Visually de-emphasised. Could move behind a debug
              toggle later. */}
          <span className="text-[#666]" data-testid="dev-telemetry">
            seq: {graph.max_seq} · corpus: {graph.stats.corpus_nodes} / {graph.stats.corpus_edges}
          </span>
        </>
      ) : (
        <span>waiting on first snapshot…</span>
      )}
      <div className="ml-auto flex items-center gap-2">
        <KpiBar />
        <button
          type="button"
          onClick={onToggleAuto}
          data-testid="auto-watch-toggle"
          {...{ [AUTO_WATCH_IGNORE_ATTR]: "" }}
          className={`px-2 py-0.5 rounded border font-bold tracking-wider ${
            autoOn
              ? "bg-[#0A1A10] border-[#4A9E5C] text-[#4A9E5C]"
              : "bg-[#111] border-[#222] text-[#999] hover:text-[#E8E8E8]"
          }`}
          title={
            autoOn
              ? `auto-watch ON (${autoReason}) — click to disable; auto re-enables on blur or 10m idle`
              : `auto-watch OFF (${autoReason}) — click to enable, or it re-enables on blur / 10m idle`
          }
        >
          AUTO {autoOn ? "ON" : "OFF"}
        </button>
        {stuckCount > 0 ? (
          <button
            type="button"
            onClick={onStuckClick}
            data-testid="stuck-badge"
            className="px-2 py-0.5 rounded bg-[#1a0606] border border-[#D71921] text-[#D71921] font-bold animate-pulse hover:bg-[#3a1010]"
            title="Sessions awaiting you for >15min — click to focus"
          >
            STUCK: {stuckCount}
          </button>
        ) : null}
        {error ? <span className="text-[#D71921]">{error}</span> : null}
        <button
          type="button"
          onClick={onOpenSettings}
          data-testid="open-settings"
          aria-label="open settings"
          title="settings"
          className="text-[#999] hover:text-[#E8E8E8] text-[14px] leading-none"
        >
          ⚙
        </button>
      </div>
    </div>
  );
}
