"use client";

import { useMemo, type ReactNode } from "react";
import CssBaseline from "@mui/material/CssBaseline";
import { ThemeProvider } from "@mui/material/styles";
import { getNothingTheme } from "./nothing-theme";

/**
 * Client-side MUI theme registry.
 *
 * Wraps the app tree in `ThemeProvider` + `CssBaseline`. Lives in a Client
 * Component because emotion / MUI's runtime requires a browser context, and
 * Next 15 RSC layouts cannot directly host context providers.
 *
 * Defaults to dark mode for now — light-mode toggle is a follow-up ticket.
 */
export function ThemeRegistry({ children }: { children: ReactNode }) {
  const theme = useMemo(() => getNothingTheme("dark"), []);
  return (
    <ThemeProvider theme={theme}>
      <CssBaseline />
      {children}
    </ThemeProvider>
  );
}
