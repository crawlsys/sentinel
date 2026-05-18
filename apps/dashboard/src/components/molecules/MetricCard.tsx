"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

import {
  Label,
  MetricNumber,
  Sparkline,
  type SparklineTone,
} from "@/components/atoms";

/** Tone for {@link MetricCard}. */
export type MetricCardTone = "primary" | "success" | "warn" | "error";

/** Props for {@link MetricCard}. */
export interface MetricCardProps {
  /** ALL-CAPS caption rendered above the value. */
  label: string;
  /** The headline number (or pre-formatted string). */
  value: number | string;
  /** Optional unit suffix (e.g. `"ms"`, `"%"`). */
  unit?: string;
  /** Optional trend sparkline data. */
  trend?: number[];
  /** Tone — drives the sparkline stroke + the border colour. */
  tone?: MetricCardTone;
}

const TONE_TO_BORDER: Record<MetricCardTone, string> = {
  primary: "divider",
  success: "success.main",
  warn: "warning.main",
  error: "error.main",
};

const TONE_TO_SPARK: Record<MetricCardTone, SparklineTone> = {
  primary: "primary",
  success: "success",
  warn: "warn",
  error: "error",
};

/**
 * Captioned numeric tile: ALL-CAPS label, headline metric, optional trend
 * sparkline. The fundamental unit on a dashboard row.
 */
export function MetricCard(props: MetricCardProps): ReactElement {
  const { label, value, unit, trend, tone = "primary" } = props;
  return (
    <Box
      data-testid="metric-card"
      data-tone={tone}
      sx={{
        display: "flex",
        flexDirection: "column",
        gap: 1,
        padding: 2,
        border: "1px solid",
        borderColor: TONE_TO_BORDER[tone],
        borderRadius: 0,
        backgroundColor: "background.paper",
        minWidth: 120,
      }}
    >
      <Label tone="secondary">{label}</Label>
      <MetricNumber value={value} {...(unit !== undefined ? { unit } : {})} size="large" />
      {trend !== undefined && trend.length > 0 ? (
        <Box sx={{ marginTop: 1 }}>
          <Sparkline points={trend} tone={TONE_TO_SPARK[tone]} width={140} height={28} />
        </Box>
      ) : null}
    </Box>
  );
}
