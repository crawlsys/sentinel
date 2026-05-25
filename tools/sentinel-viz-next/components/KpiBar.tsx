"use client";

import { useEffect, useState } from "react";

import { apiBase } from "../lib/api";

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

/// Compact metrics strip — sits in the StatusBar at the right edge,
/// auto-refreshes every 5s. Cards roll up the data the operator
/// asked for: token throughput, recent activity, active sessions,
/// cost (when wired). Designed to fit on one row so it doesn't
/// crowd the status counters.
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

  return (
    <div
      data-testid="kpi-bar"
      className="flex items-center gap-3 text-[10px] font-mono"
      title="auto-refreshes every 5s — derived from the cached graph snapshot + transcript JSONLs"
    >
      <Card
        label="active"
        value={`${kpis.sessions_active} / ${kpis.sessions_total}`}
        accent="#3fb950"
      />
      <Card
        label="evt/min"
        value={kpis.events_per_min.toFixed(0)}
        accent="#58a6ff"
      />
      <Card label="5m" value={String(kpis.events_5m)} accent="#bc8cff" />
      {kpis.tokens_5m ? (
        <Card
          label="out/5m"
          value={formatTokens(kpis.tokens_5m.output)}
          accent="#d29922"
          title={`input ${formatTokens(kpis.tokens_5m.input)} · cache ${formatTokens(kpis.tokens_5m.cache_read)} read · ${formatTokens(kpis.tokens_5m.cache_creation)} write · output ${formatTokens(kpis.tokens_5m.output)}`}
        />
      ) : (
        <Card label="out/5m" value="—" accent="#484f58" title="no transcript tokens parsed in window" />
      )}
      {kpis.stuck_count > 0 ? (
        <Card
          label="stuck"
          value={String(kpis.stuck_count)}
          accent="#f85149"
          title="sessions in awaiting_user > 15min"
        />
      ) : null}
    </div>
  );
}

function Card({
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
  return (
    <div
      className="flex items-baseline gap-1 px-2 py-0.5 rounded bg-[#161b22] border border-[#30363d]"
      title={title}
    >
      <span className="text-[#6e7681] uppercase tracking-wider">{label}</span>
      <span className="font-bold" style={{ color: accent }}>
        {value}
      </span>
    </div>
  );
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}
