"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

import { Label, MetricNumber, StatusDot } from "@/components/atoms";
import type { Stage } from "@/domain";

/** Props for {@link BottleneckCallout}. */
export interface BottleneckCalloutProps {
  /** Bottleneck stage, or null when the pipeline is healthy. */
  stage: Stage | null;
  /** Optional WIP-days score (rendered as a small subtext when present). */
  score?: number;
}

/**
 * Pipeline bottleneck callout banner. When `stage === null` renders an
 * "all clear" idle state. Otherwise highlights the offending stage with a
 * warn-tone status dot, an ALL-CAPS `BOTTLENECK` label, and the stage name
 * in a medium-size metric.
 */
export function BottleneckCallout(props: BottleneckCalloutProps): ReactElement {
  const { stage, score } = props;
  if (stage === null) {
    return (
      <Box
        data-testid="bottleneck-callout"
        data-stage="none"
        sx={{
          display: "inline-flex",
          alignItems: "center",
          gap: 1.5,
        }}
      >
        <StatusDot tone="idle" />
        <Label tone="secondary">NO BOTTLENECK</Label>
      </Box>
    );
  }
  return (
    <Box
      data-testid="bottleneck-callout"
      data-stage={stage}
      sx={{
        display: "inline-flex",
        alignItems: "center",
        gap: 1.5,
      }}
    >
      <StatusDot tone="warn" />
      <Label>BOTTLENECK</Label>
      <MetricNumber value={stage} size="medium" font="grotesk" />
      {score !== undefined ? (
        <Label tone="secondary">{`${score.toFixed(1)} WIP-days`}</Label>
      ) : null}
    </Box>
  );
}
