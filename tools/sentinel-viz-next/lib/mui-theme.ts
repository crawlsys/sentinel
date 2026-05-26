"use client";

import { createTheme, type Theme } from "@mui/material/styles";

/// Nothing-themed MUI theme. Maps every Nothing token from
/// globals.css into the MUI palette + typography + shape so that
/// MUI primitives (Button, Card, Modal, Chip, IconButton, etc.)
/// render with the same OLED-black + monospace-label aesthetic as
/// the Tailwind components.
///
/// References:
///   - ~/.claude/skills/nothing-design/references/tokens.md
///   - ~/.claude/skills/nothing-design/references/components.md
///
/// Reads CSS vars defined in app/globals.css so a future light-
/// mode swap (or live re-themeing) is a single CSS edit.
export function buildNothingMuiTheme(): Theme {
  return createTheme({
    cssVariables: { cssVarPrefix: "nd" },
    palette: {
      mode: "dark",
      common: { black: "#000000", white: "#FFFFFF" },
      background: {
        default: "#000000", // --black, OLED
        paper: "#111111",   // --surface
      },
      text: {
        primary: "#E8E8E8",   // --text-primary
        secondary: "#999999", // --text-secondary
        disabled: "#666666",  // --text-disabled
      },
      primary: {
        // Primary = the "display white" — used for inverted buttons
        // and active states. Per Nothing components.md §2.
        main: "#FFFFFF",
        contrastText: "#000000",
      },
      error: {
        // The SOLE accent moment. Used for stuck callouts and
        // destructive actions. Never decorative.
        main: "#D71921", // --accent
        contrastText: "#FFFFFF",
      },
      warning: {
        main: "#D4A843", // --warning
        contrastText: "#000000",
      },
      success: {
        main: "#4A9E5C", // --success
        contrastText: "#000000",
      },
      info: {
        main: "#5B9BF6", // --info (interactive, links)
        contrastText: "#000000",
      },
      divider: "#222222", // --border
    },
    typography: {
      // Two font families per Nothing §2.2. Doto reserved for
      // single hero moments — applied per-component via className,
      // not as a global Typography variant.
      fontFamily: 'var(--font-grotesk), "DM Sans", system-ui, sans-serif',
      // MUI's variant scale collapsed to three sizes per the
      // three-layer rule §2.1.
      h1: { fontSize: 48, lineHeight: 1.05, letterSpacing: "-0.02em", fontWeight: 400 },
      h2: { fontSize: 36, lineHeight: 1.1, letterSpacing: "-0.02em", fontWeight: 400 },
      h3: { fontSize: 24, lineHeight: 1.2, letterSpacing: "-0.01em", fontWeight: 500 },
      subtitle1: { fontSize: 18, lineHeight: 1.3, fontWeight: 400 },
      body1: { fontSize: 14, lineHeight: 1.5, fontWeight: 400 },
      body2: { fontSize: 12, lineHeight: 1.4, fontWeight: 400 },
      // "Instrument panel" label — ALL CAPS Space Mono, per §2.2.
      // Used by MUI Chip, FormLabel, etc. via theme.typography.overline.
      overline: {
        fontFamily: 'var(--font-space-mono), "JetBrains Mono", monospace',
        fontSize: 11,
        lineHeight: 1.2,
        letterSpacing: "0.08em",
        textTransform: "uppercase",
        color: "#999999",
        fontWeight: 400,
      },
      caption: {
        fontFamily: 'var(--font-space-mono), "JetBrains Mono", monospace',
        fontSize: 12,
        lineHeight: 1.4,
        letterSpacing: "0.04em",
      },
      button: {
        // All MUI buttons render as ALL CAPS Space Mono per
        // components.md §2.
        fontFamily: 'var(--font-space-mono), "JetBrains Mono", monospace',
        fontSize: 13,
        letterSpacing: "0.06em",
        fontWeight: 400,
        textTransform: "uppercase",
      },
    },
    shape: {
      // Cards 8px (technical), Buttons pill 999px override per-
      // component below.
      borderRadius: 8,
    },
    components: {
      MuiCssBaseline: {
        styleOverrides: {
          body: {
            backgroundColor: "#000000",
            color: "#E8E8E8",
          },
        },
      },
      MuiButton: {
        defaultProps: {
          disableElevation: true,
          disableRipple: false,
        },
        styleOverrides: {
          root: {
            borderRadius: 999, // pill per §2 components.md
            padding: "8px 16px",
            minHeight: 36,
            boxShadow: "none",
            "&:hover": { boxShadow: "none" },
          },
          outlined: {
            borderColor: "#333333",
            color: "#E8E8E8",
            "&:hover": {
              borderColor: "#E8E8E8",
              backgroundColor: "transparent",
            },
          },
          contained: {
            backgroundColor: "#FFFFFF",
            color: "#000000",
            "&:hover": { backgroundColor: "#E8E8E8" },
          },
        },
      },
      MuiIconButton: {
        defaultProps: { disableRipple: false },
        styleOverrides: {
          root: {
            color: "#999999",
            borderRadius: 4,
            padding: 8,
            "&:hover": { color: "#E8E8E8", backgroundColor: "#1A1A1A" },
          },
        },
      },
      MuiChip: {
        defaultProps: { size: "small", variant: "outlined" },
        styleOverrides: {
          root: {
            fontFamily: 'var(--font-space-mono), monospace',
            fontSize: 11,
            letterSpacing: "0.06em",
            textTransform: "uppercase",
            borderColor: "#333333",
            color: "#E8E8E8",
            borderRadius: 999,
            height: 22,
          },
          outlined: {
            backgroundColor: "transparent",
          },
        },
      },
      MuiCard: {
        defaultProps: { variant: "outlined" },
        styleOverrides: {
          root: {
            backgroundColor: "#111111",
            borderColor: "#222222",
            borderRadius: 8,
            backgroundImage: "none",
          },
        },
      },
      MuiPaper: {
        defaultProps: { elevation: 0 },
        styleOverrides: {
          root: { backgroundImage: "none" },
        },
      },
      MuiDialog: {
        styleOverrides: {
          paper: {
            backgroundColor: "#111111",
            border: "1px solid #222222",
            borderRadius: 8,
          },
        },
      },
      MuiDrawer: {
        styleOverrides: {
          paper: {
            backgroundColor: "#000000",
            borderColor: "#222222",
            backgroundImage: "none",
          },
        },
      },
      MuiModal: {
        styleOverrides: {
          backdrop: {
            backgroundColor: "rgba(0, 0, 0, 0.7)",
          },
        },
      },
      MuiBackdrop: {
        styleOverrides: {
          root: { backgroundColor: "rgba(0, 0, 0, 0.7)" },
        },
      },
      MuiTooltip: {
        styleOverrides: {
          tooltip: {
            backgroundColor: "#1A1A1A",
            border: "1px solid #333333",
            color: "#E8E8E8",
            fontSize: 11,
            fontFamily: 'var(--font-space-mono), monospace',
            letterSpacing: "0.04em",
            padding: "6px 10px",
            borderRadius: 4,
          },
        },
      },
      MuiSelect: {
        styleOverrides: {
          select: {
            backgroundColor: "#000000",
            color: "#E8E8E8",
            fontFamily: 'var(--font-space-mono), monospace',
            fontSize: 12,
          },
        },
      },
      MuiOutlinedInput: {
        styleOverrides: {
          notchedOutline: { borderColor: "#222222" },
          root: {
            "&:hover .MuiOutlinedInput-notchedOutline": { borderColor: "#333333" },
            "&.Mui-focused .MuiOutlinedInput-notchedOutline": { borderColor: "#E8E8E8" },
          },
        },
      },
      MuiLinearProgress: {
        // Used by the segmented bar treatment on KpiBar — but the
        // signature segmented look is hand-rolled via a custom
        // <SegmentedBar /> component (per components.md §11).
        styleOverrides: {
          root: { backgroundColor: "#222222", height: 8 },
          bar: { backgroundColor: "#FFFFFF" },
        },
      },
    },
  });
}
