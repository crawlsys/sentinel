"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

import { Label, SegmentedBar } from "@/components/atoms";
import { STAGES, type Stage } from "@/domain";

/** Props for {@link CycleTimeBreakdown}. */
export interface CycleTimeBreakdownProps {
  /** Average cycle hours per stage (missing stages render at 0). */
  readonly byStage: Partial<Record<Stage, number>>;
}

/**
 * Per-stage cycle-time bars. Each stage gets a row with its label on the
 * left and a `SegmentedBar` filling the remaining width. Bars scale to
 * the worst stage in the data so the rest are visually comparable.
 */
export function CycleTimeBreakdown(props: CycleTimeBreakdownProps): ReactElement {
  const { byStage } = props;
  const values = STAGES.map((s) => byStage[s] ?? 0);
  const max = Math.max(0, ...values);
  return (
    <Box
      data-testid="cycle-time-breakdown"
      sx={{
        display: "flex",
        flexDirection: "column",
        gap: 1,
      }}
    >
      {STAGES.map((stage, idx) => {
        const hours = values[idx] ?? 0;
        const ratio = max > 0 ? hours / max : 0;
        return (
          <Box
            key={stage}
            data-stage={stage}
            sx={{
              display: "grid",
              gridTemplateColumns: "minmax(120px, 1fr) 3fr auto",
              alignItems: "center",
              gap: 2,
            }}
          >
            <Label tone="secondary">{stage}</Label>
            <SegmentedBar value={ratio} tone="primary" />
            <Label tone="secondary">
              {hours > 0 ? `${Math.round(hours * 10) / 10}h` : "—"}
            </Label>
          </Box>
        );
      })}
    </Box>
  );
}
