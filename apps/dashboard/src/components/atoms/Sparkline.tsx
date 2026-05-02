"use client";

import type { ReactElement } from "react";
import { useTheme } from "@mui/material/styles";
import Box from "@mui/material/Box";
import { LineChart } from "@mui/x-charts/LineChart";

/** Tone for {@link Sparkline} — maps to a palette token. */
export type SparklineTone = "primary" | "success" | "warn" | "error";

/** Props for {@link Sparkline}. */
export interface SparklineProps {
  /** Numeric data points (x is implicit by index). */
  points: number[];
  /** Width in pixels (defaults to 80). */
  width?: number;
  /** Height in pixels (defaults to 24). */
  height?: number;
  /** Stroke tone (defaults to `primary`). */
  tone?: SparklineTone;
}

/**
 * Tiny inline line chart — no axes, no grid, no markers, no legend, no
 * tooltips. Pure stroke. Built on `@mui/x-charts` `LineChart`.
 */
export function Sparkline(props: SparklineProps): ReactElement {
  const { points, width = 80, height = 24, tone = "primary" } = props;
  const theme = useTheme();

  const TONE_TO_HEX: Record<SparklineTone, string> = {
    primary: theme.palette.primary.main,
    success: theme.palette.success.main,
    warn: theme.palette.warning.main,
    error: theme.palette.error.main,
  };

  // x-charts requires at least one data point. Fall back to a flat zero-line
  // so callers don't have to guard.
  const safePoints = points.length > 0 ? points : [0, 0];
  const xData = safePoints.map((_, i) => i);
  const stroke = TONE_TO_HEX[tone];

  return (
    <Box
      data-testid="sparkline"
      data-tone={tone}
      sx={{
        width,
        height,
        display: "inline-block",
        lineHeight: 0,
      }}
    >
      <LineChart
        width={width}
        height={height}
        margin={{ top: 0, right: 0, bottom: 0, left: 0 }}
        xAxis={[
          { data: xData, disableLine: true, disableTicks: true, tickLabelStyle: { display: "none" } },
        ]}
        yAxis={[
          { disableLine: true, disableTicks: true, tickLabelStyle: { display: "none" } },
        ]}
        leftAxis={null}
        bottomAxis={null}
        series={[
          {
            data: safePoints,
            color: stroke,
            showMark: false,
            disableHighlight: true,
            curve: "linear",
          },
        ]}
        grid={{ vertical: false, horizontal: false }}
        slotProps={{
          legend: { hidden: true },
        }}
        skipAnimation
        disableAxisListener
      />
    </Box>
  );
}
