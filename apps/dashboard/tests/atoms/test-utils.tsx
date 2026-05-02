import type { ReactElement } from "react";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { ThemeProvider } from "@mui/material/styles";
import { getNothingTheme } from "@/theme/nothing-theme";

// React 19 looks at this global flag to decide whether to honor `act(...)`.
// Vitest's jsdom env doesn't set it, so without this React logs noisy
// "testing environment is not configured to support act(...)" warnings.
declare global {
  // eslint-disable-next-line no-var
  var IS_REACT_ACT_ENVIRONMENT: boolean;
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true;

/**
 * Render a React element into a fresh `<div>` wrapped in the Nothing theme
 * provider for the requested mode. Returns the host container plus an
 * `unmount` callback.
 *
 * Tests use `react-dom/client` directly (no @testing-library/react in this
 * project) inside vitest's jsdom environment. Renders are wrapped in `act`
 * for React 19 strict-mode compatibility.
 */
export interface RenderResult {
  container: HTMLDivElement;
  root: Root;
  unmount: () => void;
}

export function renderWithTheme(
  ui: ReactElement,
  mode: "dark" | "light" = "dark",
): RenderResult {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);

  const theme = getNothingTheme(mode);

  act(() => {
    root.render(<ThemeProvider theme={theme}>{ui}</ThemeProvider>);
  });

  return {
    container,
    root,
    unmount: () => {
      act(() => {
        root.unmount();
      });
      container.remove();
    },
  };
}
