"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

/**
 * Font family options for MetricNumber.
 *
 * - `doto`  — dot-matrix Doto display font (Nothing hero aesthetic)
 * - `mono`  — Space Mono (technical, default)
 * - `grotesk` — Space Grotesk (sans-serif, soft)
 */
export type MetricFont = "doto" | "mono" | "grotesk";

/** Display-size variants for MetricNumber. */
export type MetricSize = "display" | "large" | "medium";

/** Props for {@link MetricNumber}. */
export interface MetricNumberProps {
  /** The numeric value (or pre-formatted string) to display. */
  value: number | string;
  /** Optional unit (e.g. `"ms"`, `"%"`). Rendered at 0.5× scale. */
  unit?: string;
  /**
   * Font family. Defaults to `mono`. When the explicit prop is omitted *and*
   * `size === "display"`, this falls back to `doto`.
   */
  font?: MetricFont;
  /** Visual size tier. */
  size?: MetricSize;
}

const FONT_VAR: Record<MetricFont, string> = {
  doto: "var(--font-doto), var(--font-space-grotesk), ui-sans-serif, sans-serif",
  mono: "var(--font-space-mono), ui-monospace, SFMono-Regular, monospace",
  grotesk: "var(--font-space-grotesk), ui-sans-serif, system-ui, sans-serif",
};

const SIZE_REM: Record<MetricSize, string> = {
  display: "clamp(3rem, 8vw, 6rem)",
  large: "2.5rem",
  medium: "1.5rem",
};

/**
 * Large numeric display with optional unit suffix.
 *
 * Renders the value at the chosen size with tight letter-spacing; the unit (if
 * provided) renders at 0.5× the value's font size. Uses Doto by default for
 * `display` size, otherwise Space Mono.
 */
export function MetricNumber(props: MetricNumberProps): ReactElement {
  const { value, unit, font, size = "large" } = props;

  // Auto-pick Doto for display size unless caller specified a font explicitly.
  const resolvedFont: MetricFont =
    font ?? (size === "display" ? "doto" : "mono");

  const fontFamily = FONT_VAR[resolvedFont];
  const fontSize = SIZE_REM[size];

  return (
    <Box
      component="span"
      sx={{
        display: "inline-flex",
        alignItems: "baseline",
        color: "text.primary",
        fontFamily,
        fontSize,
        fontWeight: 400,
        letterSpacing: "-0.02em",
        lineHeight: 1.1,
      }}
    >
      <Box component="span" sx={{ fontFamily, fontSize, lineHeight: 1.1 }}>
        {value}
      </Box>
      {unit ? (
        <Box
          component="span"
          sx={{
            fontFamily,
            fontSize: `calc(${fontSize} * 0.5)`,
            marginLeft: "0.25em",
            letterSpacing: "0.02em",
            color: "text.secondary",
          }}
        >
          {unit}
        </Box>
      ) : null}
    </Box>
  );
}
