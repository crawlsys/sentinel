"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

/** Tone for {@link SegmentedBar}. Maps to a palette token. */
export type SegmentedBarTone = "primary" | "success" | "warn" | "error";

/** Props for {@link SegmentedBar}. */
export interface SegmentedBarProps {
  /** Progress value clamped to `[0, 1]`. */
  value: number;
  /** Number of segments (defaults to 20). */
  segments?: number;
  /** Tone for filled segments (defaults to `primary`). */
  tone?: SegmentedBarTone;
}

const TONE_TO_COLOR: Record<SegmentedBarTone, string> = {
  primary: "primary.main",
  success: "success.main",
  warn: "warning.main",
  error: "error.main",
};

const EMPTY_OPACITY = 0.18;

function clamp01(n: number): number {
  if (Number.isNaN(n)) return 0;
  if (n < 0) return 0;
  if (n > 1) return 1;
  return n;
}

/**
 * Nothing-motif segmented progress bar. Splits the bar into N equal-width
 * segments; fills the leftmost `round(value * segments)` in `tone`, leaves
 * the rest at low-opacity.
 */
export function SegmentedBar(props: SegmentedBarProps): ReactElement {
  const { value, segments = 20, tone = "primary" } = props;

  const safeSegments = Math.max(1, Math.floor(segments));
  const clamped = clamp01(value);
  const filled = Math.round(clamped * safeSegments);

  const color = TONE_TO_COLOR[tone];

  return (
    <Box
      data-testid="segmented-bar"
      data-segments={safeSegments}
      data-filled={filled}
      role="progressbar"
      aria-valuemin={0}
      aria-valuemax={1}
      aria-valuenow={clamped}
      sx={{
        display: "inline-flex",
        gap: "2px",
        width: "100%",
        height: 8,
      }}
    >
      {Array.from({ length: safeSegments }, (_, i) => {
        const isFilled = i < filled;
        return (
          <Box
            key={i}
            data-filled={isFilled ? "true" : "false"}
            sx={{
              flex: 1,
              height: "100%",
              backgroundColor: color,
              opacity: isFilled ? 1 : EMPTY_OPACITY,
              borderRadius: 0,
            }}
          />
        );
      })}
    </Box>
  );
}
