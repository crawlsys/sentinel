/**
 * Nothing-aesthetic MUI v6 theme overrides.
 *
 * Design philosophy: technical, percussive, no decoration. Flat surfaces
 * (zero elevation), monospaced typography for labels/code, dot-matrix display
 * font for hero text, no spring transitions, no toasts, no shimmer skeletons.
 *
 * The shadows array is force-flattened to 25 `'none'` strings so MUI's
 * elevation system effectively no-ops everywhere — Card/Paper/Menu/Tooltip
 * all render flat.
 */
import { createTheme, type Theme } from "@mui/material/styles";
import type { Shadows } from "@mui/material/styles";

/** Nothing brand red — used as MUI error palette and accent. */
const NOTHING_RED = "#D71921";

/** Pure OLED black for dark-mode background. */
const OLED_BLACK = "#000000";

/** Slightly-raised paper surface in dark mode (still effectively black). */
const NEAR_BLACK = "#0a0a0a";

/** Warm off-white background for light mode (avoids pure-white glare). */
const WARM_WHITE = "#fafaf7";

/** Percussive easing curve — instant snap, no bounce, no spring. */
const PERCUSSIVE_EASING = "cubic-bezier(0.2, 0, 0, 1)";

/** Font-family CSS variable references — loaded via next/font in app/layout.tsx. */
const FONT_GROTESK = "var(--font-space-grotesk), ui-sans-serif, system-ui, sans-serif";
const FONT_MONO = "var(--font-space-mono), ui-monospace, SFMono-Regular, monospace";
const FONT_DOTO = "var(--font-doto), var(--font-space-grotesk), ui-sans-serif, sans-serif";

/**
 * Build a fully-configured Nothing-aesthetic MUI theme.
 *
 * @param mode - Color scheme: `'dark'` (default) is OLED-black; `'light'` is warm off-white.
 */
export function getNothingTheme(mode: "dark" | "light"): Theme {
  const isDark = mode === "dark";

  return createTheme({
    // No elevation, anywhere. All 25 entries → 'none'.
    shadows: Array(25).fill("none") as unknown as Shadows,

    palette: {
      mode,
      primary: {
        main: isDark ? "#ffffff" : "#000000",
        contrastText: isDark ? "#000000" : "#ffffff",
      },
      secondary: {
        main: isDark ? "rgba(255, 255, 255, 0.6)" : "rgba(0, 0, 0, 0.6)",
      },
      error: {
        main: NOTHING_RED,
        contrastText: "#ffffff",
      },
      background: {
        default: isDark ? OLED_BLACK : WARM_WHITE,
        paper: isDark ? NEAR_BLACK : "#ffffff",
      },
      text: {
        primary: isDark ? "#ffffff" : "#0a0a0a",
        secondary: isDark
          ? "rgba(255, 255, 255, 0.6)"
          : "rgba(10, 10, 10, 0.6)",
        disabled: isDark
          ? "rgba(255, 255, 255, 0.4)"
          : "rgba(10, 10, 10, 0.4)",
      },
      divider: isDark
        ? "rgba(255, 255, 255, 0.12)"
        : "rgba(10, 10, 10, 0.12)",
    },

    typography: {
      fontFamily: FONT_GROTESK,
      // Hero display — Doto dot-matrix only here.
      h1: {
        fontFamily: FONT_DOTO,
        fontWeight: 400,
        fontSize: "clamp(3rem, 8vw, 6rem)",
        letterSpacing: "0.05em",
        lineHeight: 1.1,
      },
      h2: {
        fontFamily: FONT_GROTESK,
        fontWeight: 500,
        letterSpacing: "-0.01em",
      },
      h3: {
        fontFamily: FONT_GROTESK,
        fontWeight: 500,
      },
      h4: {
        fontFamily: FONT_GROTESK,
        fontWeight: 500,
      },
      h5: {
        fontFamily: FONT_GROTESK,
        fontWeight: 500,
      },
      h6: {
        fontFamily: FONT_GROTESK,
        fontWeight: 500,
      },
      body1: { fontFamily: FONT_GROTESK },
      body2: { fontFamily: FONT_GROTESK },
      // Labels / code / chips — Space Mono.
      button: {
        fontFamily: FONT_MONO,
        fontWeight: 700,
        textTransform: "uppercase",
        letterSpacing: "0.08em",
      },
      caption: {
        fontFamily: FONT_MONO,
        letterSpacing: "0.05em",
      },
      // Overline — ALL CAPS, mono, wide tracking.
      overline: {
        fontFamily: FONT_MONO,
        textTransform: "uppercase",
        letterSpacing: "0.1em",
        fontWeight: 700,
        fontSize: "0.7rem",
      },
    },

    shape: {
      // Technical default: 4px square-ish corners.
      // Buttons override to pill via component overrides below.
      borderRadius: 4,
    },

    transitions: {
      easing: {
        easeIn: PERCUSSIVE_EASING,
        easeOut: PERCUSSIVE_EASING,
        easeInOut: PERCUSSIVE_EASING,
        sharp: PERCUSSIVE_EASING,
      },
      duration: {
        shortest: 100,
        shorter: 150,
        short: 180,
        standard: 200,
        complex: 240,
        enteringScreen: 180,
        leavingScreen: 140,
      },
    },

    components: {
      MuiCssBaseline: {
        styleOverrides: {
          body: {
            backgroundColor: isDark ? OLED_BLACK : WARM_WHITE,
            color: isDark ? "#ffffff" : "#0a0a0a",
            fontFamily: FONT_GROTESK,
          },
          "*::-webkit-scrollbar": {
            width: 6,
            height: 6,
          },
          "*::-webkit-scrollbar-thumb": {
            background: isDark
              ? "rgba(255, 255, 255, 0.2)"
              : "rgba(0, 0, 0, 0.2)",
            borderRadius: 0,
          },
        },
      },

      MuiButtonBase: {
        defaultProps: {
          // Nothing forbids ripple — instant feedback only.
          disableRipple: true,
        },
      },

      MuiButton: {
        defaultProps: {
          variant: "outlined",
          disableElevation: true,
        },
        styleOverrides: {
          root: {
            borderRadius: 999, // pill
            fontFamily: FONT_MONO,
            fontWeight: 700,
            textTransform: "uppercase",
            letterSpacing: "0.08em",
            paddingInline: "1.25rem",
            boxShadow: "none",
            "&:hover": {
              boxShadow: "none",
            },
          },
        },
      },

      MuiPaper: {
        defaultProps: {
          elevation: 0,
        },
        styleOverrides: {
          root: {
            // MUI v6 dark mode adds a backgroundImage gradient for elevation.
            // Nothing aesthetic forbids it — flat surfaces only.
            backgroundImage: "none",
          },
        },
      },

      MuiCard: {
        defaultProps: {
          variant: "outlined",
          elevation: 0,
        },
        styleOverrides: {
          root: {
            backgroundImage: "none",
            boxShadow: "none",
          },
        },
      },

      MuiAppBar: {
        defaultProps: {
          elevation: 0,
          color: "transparent",
        },
        styleOverrides: {
          root: {
            backgroundImage: "none",
            borderBottom: `1px solid ${
              isDark ? "rgba(255,255,255,0.12)" : "rgba(0,0,0,0.12)"
            }`,
          },
        },
      },

      MuiChip: {
        styleOverrides: {
          root: {
            fontFamily: FONT_MONO,
            fontWeight: 700,
            letterSpacing: "0.05em",
            textTransform: "uppercase",
            borderRadius: 999,
          },
        },
      },

      MuiTextField: {
        defaultProps: {
          variant: "outlined",
        },
      },

      MuiOutlinedInput: {
        styleOverrides: {
          root: {
            borderRadius: 4,
            fontFamily: FONT_GROTESK,
          },
        },
      },

      MuiTab: {
        styleOverrides: {
          root: {
            fontFamily: FONT_MONO,
            fontWeight: 700,
            textTransform: "uppercase",
            letterSpacing: "0.08em",
          },
        },
      },

      MuiTabs: {
        styleOverrides: {
          indicator: {
            // Sharp 2px line, no rounded ends.
            height: 2,
            borderRadius: 0,
          },
        },
      },

      // Nothing forbids toasts — Snackbar is hidden globally.
      MuiSnackbar: {
        styleOverrides: {
          root: {
            display: "none",
          },
        },
      },

      // Replace pulsing skeleton rectangles with [LOADING...] text.
      // The actual MuiSkeleton wave/pulse animation is suppressed via 'animation: none'
      // and the visible content is injected via a ::before pseudo-element.
      MuiSkeleton: {
        defaultProps: {
          animation: false,
        },
        styleOverrides: {
          root: {
            backgroundColor: "transparent",
            transform: "none",
            width: "auto",
            height: "auto",
            animation: "none",
            display: "inline-block",
            color: isDark
              ? "rgba(255, 255, 255, 0.6)"
              : "rgba(0, 0, 0, 0.6)",
            fontFamily: FONT_MONO,
            fontWeight: 700,
            letterSpacing: "0.08em",
            textTransform: "uppercase",
            "&::before": {
              content: '"[LOADING...]"',
            },
            "&::after": {
              display: "none",
            },
          },
        },
      },
    },
  });
}
