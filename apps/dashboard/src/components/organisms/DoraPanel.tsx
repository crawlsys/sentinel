"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

import type { GetDoraTierResult } from "@/application";
import { MetricCard, type MetricCardTone } from "@/components/molecules";
import type { DoraTier } from "@/domain";

/** Props for {@link DoraPanel}. */
export interface DoraPanelProps {
  /** Result from `GetDoraTier.run(window)` — already computed by the page. */
  result: GetDoraTierResult;
}

function tierToTone(tier: DoraTier): MetricCardTone {
  switch (tier) {
    case "elite":
      return "success";
    case "high":
      return "primary";
    case "medium":
      return "warn";
    case "low":
      return "error";
  }
}

/** Format a number to one decimal, trimming trailing `.0`. */
function fmt1(n: number): string {
  if (!Number.isFinite(n)) return "—";
  const r = Math.round(n * 10) / 10;
  return Number.isInteger(r) ? `${r}` : r.toFixed(1);
}

/** Format a 0–1 ratio as an integer percent. */
function fmtPct(n: number): string {
  if (!Number.isFinite(n)) return "—";
  return `${Math.round(n * 100)}`;
}

/**
 * Four-card panel showing the four DORA metrics for the current window.
 * Tone of each card reflects the tier classification.
 */
export function DoraPanel(props: DoraPanelProps): ReactElement {
  const { result } = props;
  const { tiers, raw } = result;
  return (
    <Box
      data-testid="dora-panel"
      sx={{
        display: "grid",
        gridTemplateColumns: { xs: "1fr", sm: "repeat(4, 1fr)" },
        gap: 2,
      }}
    >
      <MetricCard
        label="LEAD TIME"
        value={fmt1(raw.leadTimeHours)}
        unit="h"
        tone={tierToTone(tiers.lead_time)}
      />
      <MetricCard
        label="DEPLOYS / DAY"
        value={fmt1(raw.deployFreqPerDay)}
        unit="/d"
        tone={tierToTone(tiers.deploy_freq)}
      />
      <MetricCard
        label="CHANGE FAILURE"
        value={fmtPct(raw.cfr)}
        unit="%"
        tone={tierToTone(tiers.change_failure_rate)}
      />
      <MetricCard
        label="MTTR"
        value={fmt1(raw.mttrHours)}
        unit="h"
        tone={tierToTone(tiers.mttr)}
      />
    </Box>
  );
}
