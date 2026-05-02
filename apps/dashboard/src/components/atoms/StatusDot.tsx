"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

/** Status tone for a {@link StatusDot}. */
export type StatusTone = "success" | "warn" | "error" | "idle";

/** Props for {@link StatusDot}. */
export interface StatusDotProps {
  /** Which palette token to draw the dot in. */
  tone: StatusTone;
}

const TONE_TO_COLOR: Record<StatusTone, string> = {
  success: "success.main",
  warn: "warning.main",
  error: "error.main",
  idle: "text.disabled",
};

/**
 * 8px circular status indicator. Maps tone → theme palette so it adapts
 * automatically across dark/light modes.
 */
export function StatusDot(props: StatusDotProps): ReactElement {
  const { tone } = props;
  return (
    <Box
      data-testid="status-dot"
      data-tone={tone}
      role="presentation"
      sx={{
        display: "inline-block",
        width: 8,
        height: 8,
        borderRadius: "50%",
        backgroundColor: TONE_TO_COLOR[tone],
        flexShrink: 0,
      }}
    />
  );
}
