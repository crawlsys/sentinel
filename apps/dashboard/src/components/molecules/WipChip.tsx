"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

import { StatusDot, Tag, type TagTone } from "@/components/atoms";
import type { Stage } from "@/domain";

/** Props for {@link WipChip}. */
export interface WipChipProps {
  /** Pipeline stage this chip represents. */
  stage: Stage;
  /** Number of tickets currently in this stage. */
  count: number;
  /** Optional threshold above which the chip turns warn/error. */
  threshold?: number;
}

function pickTone(count: number, threshold: number | undefined): TagTone {
  if (threshold === undefined) return "success";
  if (count >= threshold) return "error";
  if (count >= threshold * 0.75) return "warn";
  return "success";
}

/**
 * Stage + count chip with an at-a-glance status dot. Goes warn / error
 * when `count` approaches / exceeds `threshold`.
 */
export function WipChip(props: WipChipProps): ReactElement {
  const { stage, count, threshold } = props;
  const tone = pickTone(count, threshold);
  // Tag's tones are `default | success | warn | error`; StatusDot's are
  // `success | warn | error | idle`. Map the shared subset directly.
  const dotTone = tone === "default" ? "idle" : tone;
  return (
    <Box
      data-testid="wip-chip"
      data-stage={stage}
      data-count={count}
      sx={{
        display: "inline-flex",
        alignItems: "center",
        gap: 1,
      }}
    >
      <StatusDot tone={dotTone} />
      <Tag tone={tone}>{`${stage}: ${count}`}</Tag>
    </Box>
  );
}
