"use client";

import type { ReactElement, ReactNode } from "react";
import Box from "@mui/material/Box";

/** Tone for a {@link Tag}. */
export type TagTone = "default" | "success" | "warn" | "error";

/** Props for {@link Tag}. */
export interface TagProps {
  /** Tag content (typically short text). */
  children: ReactNode;
  /** Tone (defaults to `default`). */
  tone?: TagTone;
  /** Whether to uppercase the children (defaults to `true`). */
  uppercase?: boolean;
}

const TONE_TO_COLOR: Record<TagTone, string> = {
  default: "text.primary",
  success: "success.main",
  warn: "warning.main",
  error: "error.main",
};

/**
 * Outlined pill tag in Space Mono. Border + text both pull from the palette
 * token mapped to the chosen tone, so dark/light mode works automatically.
 */
export function Tag(props: TagProps): ReactElement {
  const { children, tone = "default", uppercase = true } = props;
  const color = TONE_TO_COLOR[tone];

  return (
    <Box
      component="span"
      data-testid="tag"
      data-tone={tone}
      sx={{
        display: "inline-flex",
        alignItems: "center",
        height: 22,
        paddingInline: "0.6em",
        borderRadius: 999,
        border: "1px solid",
        borderColor: color,
        color,
        fontFamily:
          "var(--font-space-mono), ui-monospace, SFMono-Regular, monospace",
        fontWeight: 700,
        fontSize: "0.7rem",
        letterSpacing: "0.08em",
        textTransform: uppercase ? "uppercase" : "none",
        lineHeight: 1,
        backgroundColor: "transparent",
        whiteSpace: "nowrap",
      }}
    >
      {children}
    </Box>
  );
}
