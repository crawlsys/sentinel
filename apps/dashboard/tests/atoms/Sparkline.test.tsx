import { describe, it, expect } from "vitest";
import { Sparkline } from "@/components/atoms/Sparkline";
import { renderWithTheme } from "./test-utils";

describe("Sparkline", () => {
  it("renders without crashing in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <Sparkline points={[1, 2, 3, 2, 1]} />,
      "dark",
    );
    expect(container.querySelector('[data-testid="sparkline"]')).not.toBeNull();
    // Inner SVG from x-charts should be present.
    expect(container.querySelector("svg")).not.toBeNull();
    unmount();
  });

  it("renders without crashing in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <Sparkline points={[1, 2, 3, 2, 1]} />,
      "light",
    );
    expect(container.querySelector('[data-testid="sparkline"]')).not.toBeNull();
    unmount();
  });

  it("matches snapshot in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <Sparkline points={[1, 4, 2, 8, 5]} width={80} height={24} />,
      "dark",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });

  it("matches snapshot in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <Sparkline points={[1, 4, 2, 8, 5]} width={80} height={24} />,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });

  it("handles empty points gracefully", () => {
    const { container, unmount } = renderWithTheme(
      <Sparkline points={[]} />,
      "dark",
    );
    // Should still render without throwing.
    expect(container.querySelector('[data-testid="sparkline"]')).not.toBeNull();
    unmount();
  });
});
