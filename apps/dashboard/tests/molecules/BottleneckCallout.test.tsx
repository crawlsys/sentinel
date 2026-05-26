import { describe, it, expect } from "vitest";
import { BottleneckCallout } from "@/components/molecules/BottleneckCallout";
import { renderWithTheme } from "../atoms/test-utils";

describe("BottleneckCallout", () => {
  it("renders the stage name when one is present", () => {
    const { container, unmount } = renderWithTheme(
      <BottleneckCallout stage="Code Review" />,
      "dark",
    );
    expect(container.textContent).toContain("Code Review");
    expect(container.textContent).toContain("BOTTLENECK");
    expect(
      container.querySelector('[data-testid="status-dot"]')?.getAttribute("data-tone"),
    ).toBe("warn");
    unmount();
  });

  it("renders the idle 'NO BOTTLENECK' state when stage is null", () => {
    const { container, unmount } = renderWithTheme(
      <BottleneckCallout stage={null} />,
      "dark",
    );
    expect(container.textContent).toContain("NO BOTTLENECK");
    expect(
      container.querySelector('[data-testid="status-dot"]')?.getAttribute("data-tone"),
    ).toBe("idle");
    unmount();
  });

  it("shows the WIP-days score when supplied", () => {
    const { container, unmount } = renderWithTheme(
      <BottleneckCallout stage="QA Testing" score={3.4} />,
      "dark",
    );
    expect(container.textContent).toContain("3.4 WIP-days");
    unmount();
  });

  it("renders in light mode without crashing", () => {
    const { container, unmount } = renderWithTheme(
      <BottleneckCallout stage="In Progress" score={2.0} />,
      "light",
    );
    expect(container.textContent).toContain("In Progress");
    unmount();
  });

  it("matches snapshot in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <BottleneckCallout stage="Code Review" score={2.7} />,
      "dark",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });
});
