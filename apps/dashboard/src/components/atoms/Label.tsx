"use client";

import type { ReactElement, ReactNode } from "react";
import Box from "@mui/material/Box";

/** Color tone for a {@link Label}. Maps to the theme's text palette. */
export type LabelTone = "primary" | "secondary" | "disabled";

/** Props for {@link Label}. */
export interface LabelProps {
  /** Label content — typically a short ALL CAPS string. */
  children: ReactNode;
  /** Tone (defaults to `primary`). */
  tone?: LabelTone;
}

const TONE_TO_COLOR: Record<LabelTone, string> = {
  primary: "text.primary",
  secondary: "text.secondary",
  disabled: "text.disabled",
};

/**
 * Small ALL-CAPS Space Mono label with wide letter-tracking.
 *
 * Used for section captions, axis labels, table headers, and other
 * "nameplate" text in the Nothing aesthetic.
 */
export function Label(props: LabelProps): ReactElement {
  const { children, tone = "primary" } = props;

  return (
    <Box
      component="span"
      sx={{
        fontFamily:
          "var(--font-space-mono), ui-monospace, SFMono-Regular, monospace",
        fontSize: "0.7rem",
        fontWeight: 700,
        letterSpacing: "0.1em",
        textTransform: "uppercase",
        color: TONE_TO_COLOR[tone],
        lineHeight: 1.4,
      }}
    >
      {children}
    </Box>
  );
}
