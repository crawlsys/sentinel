"use client";

import { useEffect, useState } from "react";
import { Box, Stack, Tooltip, Typography } from "@mui/material";

import { apiBase } from "../adapters/http";

interface Kpis {
  sessions_active: number;
  sessions_total: number;
  events_5m: number;
  events_per_min: number;
  tokens_5m: {
    input: number;
    cache_creation: number;
    cache_read: number;
    output: number;
  } | null;
  usd_5m: number | null;
  stuck_count: number;
}

export function KpiBar() {
  const [kpis, setKpis] = useState<Kpis | null>(null);

  useEffect(() => {
    let cancelled = false;
    async function tick() {
      try {
        const r = await fetch(`${apiBase()}/api/kpis`);
        if (!r.ok) return;
        const data = (await r.json()) as Kpis;
        if (!cancelled) setKpis(data);
      } catch {
        /* silent — status bar already shows API errors */
      }
    }
    tick();
    const id = window.setInterval(tick, 5_000);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
  }, []);

  if (!kpis) return null;

  const isIdle = kpis.events_5m === 0;

  return (
    <Tooltip title="auto-refreshes every 5s — derived from the cached graph snapshot + transcript JSONLs">
      <Stack
        data-testid="kpi-bar"
        data-idle={isIdle ? "true" : undefined}
        direction="row"
        spacing={1.5}
        sx={{ alignItems: "center" }}
      >
        <KpiCard
          label="active"
          value={`${kpis.sessions_active} / ${kpis.sessions_total}`}
          accent="var(--success)"
        />
        <KpiCard
          label="evt/min"
          value={isIdle ? "idle" : kpis.events_per_min.toFixed(0)}
          accent={isIdle ? "var(--text-disabled)" : "var(--info)"}
          title={isIdle ? "no events in the last 5 minutes" : undefined}
        />
        <KpiCard
          label="5m"
          value={isIdle ? "—" : String(kpis.events_5m)}
          accent={isIdle ? "var(--text-disabled)" : "#bc8cff"}
          title={isIdle ? "no events in the last 5 minutes" : undefined}
        />
        {kpis.tokens_5m ? (
          <KpiCard
            label="out/5m"
            value={formatTokens(kpis.tokens_5m.output)}
            accent="var(--warning)"
            title={`input ${formatTokens(kpis.tokens_5m.input)} · cache ${formatTokens(kpis.tokens_5m.cache_read)} read · ${formatTokens(kpis.tokens_5m.cache_creation)} write · output ${formatTokens(kpis.tokens_5m.output)}`}
          />
        ) : (
          <KpiCard label="out/5m" value="—" accent="var(--text-disabled)" title="no transcript tokens parsed in window" />
        )}
      </Stack>
    </Tooltip>
  );
}

function KpiCard({
  label,
  value,
  accent,
  title,
}: {
  label: string;
  value: string;
  accent: string;
  title?: string;
}) {
  const body = (
    <Box
      sx={{
        display: "flex",
        alignItems: "baseline",
        gap: 0.75,
        px: 1,
        py: 0.25,
        borderRadius: 1,
        border: "1px solid var(--border)",
        bgcolor: "var(--surface)",
      }}
    >
      <Typography
        component="span"
        sx={{
          fontFamily: "var(--font-space-mono), monospace",
          fontSize: 10,
          letterSpacing: "0.08em",
          textTransform: "uppercase",
          color: "var(--text-secondary)",
        }}
      >
        {label}
      </Typography>
      <Typography
        component="span"
        sx={{
          fontFamily: "var(--font-space-mono), monospace",
          fontSize: 11,
          fontWeight: 700,
          color: accent,
        }}
      >
        {value}
      </Typography>
    </Box>
  );
  return title ? <Tooltip title={title}>{body}</Tooltip> : body;
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}
