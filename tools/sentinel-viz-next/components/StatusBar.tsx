"use client";

import { Box, Chip, IconButton, Stack, Tooltip, Typography } from "@mui/material";
import SettingsIcon from "@mui/icons-material/SettingsOutlined";

import type { GraphResponse } from "../types/api";
import { AUTO_WATCH_DISABLED, AUTO_WATCH_IGNORE_ATTR } from "../hooks/auto-watch";
import type { StreamLiveness } from "../adapters/sse";
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
  const effective: StreamLiveness =
    liveness ?? (connected ? "live" : graph ? "down" : "init");
  switch (effective) {
    case "live":
      return { text: "live", color: "#4A9E5C", glyph: "●", pulse: true };
    case "stale":
      return { text: "stale", color: "#D4A843", glyph: "●", pulse: false };
    case "down":
      return graph
        ? { text: "ready", color: "#5B9BF6", glyph: "●", pulse: false }
        : { text: "down", color: "#D71921", glyph: "○", pulse: false };
    case "init":
    default:
      return graph
        ? { text: "ready", color: "#5B9BF6", glyph: "●", pulse: false }
        : { text: "connecting", color: "#D4A843", glyph: "○", pulse: false };
  }
}

const STAT_LABEL_SX = {
  fontFamily: "var(--font-space-mono), monospace",
  fontSize: 10,
  letterSpacing: "0.08em",
  textTransform: "uppercase",
  color: "var(--text-secondary)",
};

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
  const livenessAttr = liveness ?? (connected ? "live" : graph ? "down" : "init");
  return (
    <Box
      data-testid="status-bar"
      sx={{
        display: "flex",
        flexWrap: "wrap",
        alignItems: "center",
        columnGap: 3,
        rowGap: 0.5,
        px: 2,
        py: 0.75,
        borderBottom: "1px solid var(--border)",
        bgcolor: "var(--surface)",
      }}
    >
      <Typography
        component="span"
        sx={{ ...STAT_LABEL_SX, color: "var(--info)", fontWeight: 700 }}
      >
        sentinel-viz
      </Typography>

      <Tooltip
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
        <Typography
          component="span"
          data-testid="liveness-indicator"
          data-liveness={livenessAttr}
          sx={{ ...STAT_LABEL_SX, color: live.color }}
        >
          {live.glyph} {live.text}
        </Typography>
      </Tooltip>

      {graph ? (
        <>
          <Typography component="span" sx={STAT_LABEL_SX}>nodes: {graph.stats.nodes_total}</Typography>
          <Typography component="span" sx={STAT_LABEL_SX}>edges: {graph.stats.edges_total}</Typography>
          <Typography component="span" sx={STAT_LABEL_SX}>events: {graph.stats.events_total}</Typography>
          <Typography
            component="span"
            data-testid="dev-telemetry"
            sx={{ ...STAT_LABEL_SX, color: "var(--text-disabled)" }}
          >
            seq: {graph.max_seq} · corpus: {graph.stats.corpus_nodes} / {graph.stats.corpus_edges}
          </Typography>
        </>
      ) : (
        <Typography component="span" sx={STAT_LABEL_SX}>
          waiting on first snapshot…
        </Typography>
      )}

      <Stack direction="row" spacing={1} sx={{ ml: "auto", alignItems: "center" }}>
        <KpiBar />
        <Chip
          data-testid="auto-watch-toggle"
          data-auto-on={autoOn ? "true" : "false"}
          data-auto-reason={autoReason}
          label={AUTO_WATCH_DISABLED ? "AUTO DISABLED" : `AUTO ${autoOn ? "ON" : "OFF"}`}
          onClick={AUTO_WATCH_DISABLED ? undefined : onToggleAuto}
          clickable={!AUTO_WATCH_DISABLED}
          title={
            AUTO_WATCH_DISABLED
              ? "auto-watch disabled for this demo"
              : autoOn
              ? `auto-watch ON (${autoReason}) — click to disable; auto re-enables on blur or 10m idle`
              : `auto-watch OFF (${autoReason}) — click to enable, or it re-enables on blur / 10m idle`
          }
          sx={{
            borderColor: autoOn && !AUTO_WATCH_DISABLED ? "var(--success)" : "var(--border)",
            color: autoOn && !AUTO_WATCH_DISABLED ? "var(--success)" : "var(--text-secondary)",
            fontWeight: 700,
          }}
          {...{ [AUTO_WATCH_IGNORE_ATTR]: "" }}
        />

        {stuckCount > 0 ? (
          <Tooltip title="Sessions awaiting you for >15min — click to focus">
            <Chip
              data-testid="stuck-badge"
              label={`STUCK: ${stuckCount}`}
              onClick={onStuckClick}
              clickable
              sx={{
                borderColor: "var(--accent)",
                color: "var(--accent)",
                bgcolor: "rgba(215,25,33,0.10)",
                fontWeight: 700,
                animation: "pulse 1.6s ease-in-out infinite",
                "@keyframes pulse": {
                  "0%, 100%": { opacity: 1 },
                  "50%": { opacity: 0.6 },
                },
              }}
            />
          </Tooltip>
        ) : null}

        {error ? (
          <Typography component="span" sx={{ ...STAT_LABEL_SX, color: "var(--accent)" }}>
            {error}
          </Typography>
        ) : null}

        <Tooltip title="settings">
          <IconButton
            data-testid="open-settings"
            aria-label="open settings"
            onClick={onOpenSettings}
            size="small"
          >
            <SettingsIcon fontSize="small" />
          </IconButton>
        </Tooltip>
      </Stack>
    </Box>
  );
}
