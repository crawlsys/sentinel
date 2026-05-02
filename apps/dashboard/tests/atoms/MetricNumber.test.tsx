import { describe, it, expect } from "vitest";
import { MetricNumber } from "@/components/atoms/MetricNumber";
import { renderWithTheme } from "./test-utils";

describe("MetricNumber", () => {
  it("renders without crashing in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <MetricNumber value={42} unit="ms" />,
      "dark",
    );
    expect(container.textContent).toContain("42");
    expect(container.textContent).toContain("ms");
    unmount();
  });

  it("renders without crashing in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <MetricNumber value={42} unit="ms" />,
      "light",
    );
    expect(container.textContent).toContain("42");
    unmount();
  });

  it("matches snapshot in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <MetricNumber value={1280} unit="ops" font="mono" size="large" />,
      "dark",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });

  it("matches snapshot in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <MetricNumber value={1280} unit="ops" font="mono" size="large" />,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });

  it("works without unit", () => {
    const { container, unmount } = renderWithTheme(
      <MetricNumber value="N/A" />,
      "dark",
    );
    expect(container.textContent).toBe("N/A");
    unmount();
  });

  it("renders display size with default Doto font", () => {
    const { container, unmount } = renderWithTheme(
      <MetricNumber value={99} size="display" />,
      "dark",
    );
    expect(container.textContent).toContain("99");
    unmount();
  });
});
