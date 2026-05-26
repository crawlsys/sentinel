"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

import { Label, StatusDot, Tag } from "@/components/atoms";

/** Props for {@link SLABadge}. */
export interface SLABadgeProps {
  /** Display name of the SLA. */
  slaName: string;
  /** Whether the SLA is currently breached. */
  breached: boolean;
  /** Optional elapsed-in-stage hours. */
  elapsedHours?: number;
  /** Optional target threshold hours. */
  targetHours?: number;
}

/**
 * SLA-status pill. Shows a status dot + tag in `error` tone when breached,
 * `success` otherwise. When both `elapsedHours` and `targetHours` are
 * provided, renders an `N / M h` subtext.
 */
export function SLABadge(props: SLABadgeProps): ReactElement {
  const { slaName, breached, elapsedHours, targetHours } = props;
  const tone = breached ? "error" : "success";
  const subtext =
    elapsedHours !== undefined && targetHours !== undefined
      ? `${Math.round(elapsedHours)}h / ${targetHours}h`
      : null;
  return (
    <Box
      data-testid="sla-badge"
      data-breached={breached ? "true" : "false"}
      sx={{
        display: "inline-flex",
        alignItems: "center",
        gap: 1,
      }}
    >
      <StatusDot tone={tone} />
      <Tag tone={tone}>{slaName}</Tag>
      {subtext !== null ? (
        <Label tone="secondary">{subtext}</Label>
      ) : null}
    </Box>
  );
}
