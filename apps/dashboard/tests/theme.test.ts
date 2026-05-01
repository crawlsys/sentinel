import { describe, expect, it } from "vitest";
import { getNothingTheme } from "@/theme/nothing-theme";

describe("getNothingTheme — Nothing-aesthetic MUI overrides", () => {
  it("flattens shadows: every entry is 'none'", () => {
    const theme = getNothingTheme("dark");
    expect(theme.shadows.every((s) => s === "none")).toBe(true);
  });

  it("dark mode reports palette.mode === 'dark'", () => {
    const theme = getNothingTheme("dark");
    expect(theme.palette.mode).toBe("dark");
  });

  it("light mode reports palette.mode === 'light'", () => {
    const theme = getNothingTheme("light");
    expect(theme.palette.mode).toBe("light");
  });

  it("MuiButtonBase.defaultProps.disableRipple is true (Nothing forbids ripple)", () => {
    const theme = getNothingTheme("dark");
    const buttonBase = theme.components?.MuiButtonBase;
    expect(buttonBase?.defaultProps?.disableRipple).toBe(true);
  });

  it("error palette uses Nothing accent red #D71921", () => {
    const theme = getNothingTheme("dark");
    expect(theme.palette.error.main).toBe("#D71921");
  });
});
