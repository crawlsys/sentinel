"use client";

import type { ReactElement, ReactNode } from "react";
import Box from "@mui/material/Box";

import { Label, Tag } from "@/components/atoms";

/** Props for {@link DashboardLayout}. */
export interface DashboardLayoutProps {
  /** Body content — typically a stack of organism panels. */
  children: ReactNode;
  /** Optional window label rendered as a tag in the header (e.g. "LAST 30 DAYS"). */
  windowLabel?: string;
}

/**
 * Master dashboard page chrome: SENTINEL hero label on the left, optional
 * window tag on the right, then a vertical stack of organism panels.
 *
 * Pure-render — composition of adapters → use cases happens at the page
 * level (the App Router server component), not inside this template.
 */
export function DashboardLayout(props: DashboardLayoutProps): ReactElement {
  const { children, windowLabel } = props;
  return (
    <Box
      component="main"
      data-testid="dashboard-layout"
      sx={{
        minHeight: "100vh",
        backgroundColor: "background.default",
        color: "text.primary",
        paddingInline: { xs: 2, md: 6 },
        paddingBlock: { xs: 4, md: 6 },
      }}
    >
      <Box
        component="header"
        data-testid="dashboard-header"
        sx={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: 2,
          marginBottom: 4,
        }}
      >
        <Box
          sx={{
            fontFamily:
              "var(--font-doto), var(--font-space-grotesk), ui-sans-serif, sans-serif",
            fontSize: "clamp(2rem, 5vw, 3.5rem)",
            letterSpacing: "-0.02em",
            lineHeight: 1,
          }}
        >
          SENTINEL
        </Box>
        <Box sx={{ display: "inline-flex", alignItems: "center", gap: 1 }}>
          <Label tone="secondary">WINDOW</Label>
          {windowLabel !== undefined ? <Tag>{windowLabel}</Tag> : null}
        </Box>
      </Box>
      <Box
        data-testid="dashboard-content"
        sx={{
          display: "grid",
          gridTemplateColumns: "1fr",
          gap: 4,
        }}
      >
        {children}
      </Box>
    </Box>
  );
}
