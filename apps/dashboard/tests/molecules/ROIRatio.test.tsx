import { describe, it, expect } from "vitest";
import { ROIRatio } from "@/components/molecules/ROIRatio";
import { renderWithTheme } from "../atoms/test-utils";

describe("ROIRatio", () => {
  it("renders the formatted ratio with × suffix and VS HUMAN label", () => {
    const { container, unmount } = renderWithTheme(<ROIRatio ratio={3.7} />, "dark");
    expect(container.textContent).toContain("3.7");
    expect(container.textContent).toContain("×");
    expect(container.textContent).toContain("VS HUMAN");
    unmount();
  });

  it("renders ∞ when ratio is Infinity", () => {
    const { container, unmount } = renderWithTheme(
      <ROIRatio ratio={Infinity} basis="story_points" />,
      "dark",
    );
    expect(container.textContent).toContain("∞");
    expect(container.querySelector('[data-testid="roi-ratio"]')?.getAttribute("data-tone")).toBe(
      "success",
    );
    unmount();
  });

  it("uses warn tone for 0.5 ≤ ratio < 1", () => {
    const { container, unmount } = renderWithTheme(<ROIRatio ratio={0.7} />, "dark");
    expect(container.querySelector('[data-testid="roi-ratio"]')?.getAttribute("data-tone")).toBe(
      "warn",
    );
    unmount();
  });

  it("uses error tone for ratio < 0.5", () => {
    const { container, unmount } = renderWithTheme(<ROIRatio ratio={0.1} />, "dark");
    expect(container.querySelector('[data-testid="roi-ratio"]')?.getAttribute("data-tone")).toBe(
      "error",
    );
    unmount();
  });

  it("renders the basis tag when supplied + matches snapshot", () => {
    const { container, unmount } = renderWithTheme(
      <ROIRatio ratio={5} basis="days_fallback" />,
      "light",
    );
    expect(container.textContent).toContain("DAYS");
    expect(container).toMatchSnapshot();
    unmount();
  });
});
