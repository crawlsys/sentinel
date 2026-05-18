import { describe, it, expect } from "vitest";
import { MetricCard } from "@/components/molecules/MetricCard";
import { renderWithTheme } from "../atoms/test-utils";

describe("MetricCard", () => {
  it("renders label + value + unit in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <MetricCard label="DEPLOYS / DAY" value={1.5} unit="/d" />,
      "dark",
    );
    expect(container.textContent).toContain("DEPLOYS / DAY");
    expect(container.textContent).toContain("1.5");
    expect(container.textContent).toContain("/d");
    unmount();
  });

  it("renders without trend when none supplied", () => {
    const { container, unmount } = renderWithTheme(
      <MetricCard label="X" value={0} />,
      "dark",
    );
    expect(container.querySelector('[data-testid="sparkline"]')).toBeNull();
    unmount();
  });

  it("renders trend sparkline when points are provided", () => {
    const { container, unmount } = renderWithTheme(
      <MetricCard label="X" value={0} trend={[1, 2, 3, 4]} />,
      "dark",
    );
    expect(container.querySelector('[data-testid="sparkline"]')).not.toBeNull();
    unmount();
  });

  it("forwards tone to the data attribute and the sparkline", () => {
    const { container, unmount } = renderWithTheme(
      <MetricCard label="X" value={1} tone="error" trend={[1, 2, 3]} />,
      "dark",
    );
    expect(
      container.querySelector('[data-testid="metric-card"]')?.getAttribute("data-tone"),
    ).toBe("error");
    expect(
      container.querySelector('[data-testid="sparkline"]')?.getAttribute("data-tone"),
    ).toBe("error");
    unmount();
  });

  it("matches snapshot in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <MetricCard label="DEPLOYS / DAY" value={1.5} unit="/d" />,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });
});
