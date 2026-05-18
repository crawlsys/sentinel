"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

import { Label, MetricNumber, Tag } from "@/components/atoms";

/** Props for {@link ROIRatio}. */
export interface ROIRatioProps {
  /** Ratio = human cost / claude cost. `Infinity` is rendered as `∞`. */
  ratio: number;
  /** Optional basis tag (story points vs days fallback). */
  basis?: "story_points" | "days_fallback";
}

function basisLabel(basis: ROIRatioProps["basis"]): string {
  switch (basis) {
    case "story_points":
      return "STORY POINTS";
    case "days_fallback":
      return "DAYS";
    default:
      return "";
  }
}

function formatRatio(ratio: number): string {
  if (ratio === Infinity) return "∞";
  if (!Number.isFinite(ratio)) return "—";
  // Round to 1 decimal; drop the decimal if it's `.0`.
  const rounded = Math.round(ratio * 10) / 10;
  return Number.isInteger(rounded) ? `${rounded}` : rounded.toFixed(1);
}

/**
 * ROI multiplier display: e.g. `5.4 ×` with a `VS HUMAN` label and an
 * optional basis tag. Tone-coded: success > 1, warn ≥ 0.5, else error.
 * Infinity (zero-claude-cost) renders as `∞`.
 */
export function ROIRatio(props: ROIRatioProps): ReactElement {
  const { ratio, basis } = props;
  const display = formatRatio(ratio);
  // Tone choice intentionally omits Infinity from the boundary checks so
  // ratio=Infinity ends up `success` (which is the desired UX: free
  // Claude is *very* good ROI).
  const tone = ratio >= 1 ? "success" : ratio >= 0.5 ? "warn" : "error";

  return (
    <Box
      data-testid="roi-ratio"
      data-ratio={display}
      data-tone={tone}
      sx={{
        display: "inline-flex",
        flexDirection: "column",
        gap: 0.5,
      }}
    >
      <Box sx={{ display: "inline-flex", alignItems: "baseline", gap: 0.5 }}>
        <MetricNumber value={display} unit="×" size="large" />
      </Box>
      <Box sx={{ display: "inline-flex", alignItems: "center", gap: 1 }}>
        <Label tone="secondary">VS HUMAN</Label>
        {basis !== undefined ? <Tag tone={tone}>{basisLabel(basis)}</Tag> : null}
      </Box>
    </Box>
  );
}
