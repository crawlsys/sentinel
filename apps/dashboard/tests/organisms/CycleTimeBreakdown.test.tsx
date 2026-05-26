import { describe, it, expect } from "vitest";

import { CycleTimeBreakdown } from "@/components/organisms/CycleTimeBreakdown";
import { STAGES } from "@/domain";
import { renderWithTheme } from "../atoms/test-utils";

describe("CycleTimeBreakdown", () => {
  it("renders a row per canonical stage in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <CycleTimeBreakdown byStage={{ "Code Review": 4, "QA Testing": 8 }} />,
      "dark",
    );
    const rows = container.querySelectorAll<HTMLElement>(
      'div[data-stage]',
    );
    expect(rows.length).toBe(STAGES.length);
    unmount();
  });

  it("renders the longest stage at 100% fill ratio (data-filled equals segments)", () => {
    const { container, unmount } = renderWithTheme(
      <CycleTimeBreakdown byStage={{ "Code Review": 5, "QA Testing": 10 }} />,
      "dark",
    );
    const qaRow = container.querySelector<HTMLElement>(
      'div[data-stage="QA Testing"]',
    );
    const bar = qaRow?.querySelector<HTMLElement>(
      '[data-testid="segmented-bar"]',
    );
    expect(bar?.dataset["filled"]).toBe(bar?.dataset["segments"]);
    unmount();
  });

  it("renders empty-stage rows with `—` for the time column", () => {
    const { container, unmount } = renderWithTheme(
      <CycleTimeBreakdown byStage={{}} />,
      "dark",
    );
    // All stages should print the em-dash placeholder when no data.
    expect(container.textContent?.match(/—/g)?.length ?? 0).toBeGreaterThanOrEqual(
      STAGES.length,
    );
    unmount();
  });

  it("renders in light mode without crashing", () => {
    const { container, unmount } = renderWithTheme(
      <CycleTimeBreakdown byStage={{ "In Progress": 12 }} />,
      "light",
    );
    expect(
      container.querySelector('[data-testid="cycle-time-breakdown"]'),
    ).not.toBeNull();
    unmount();
  });

  it("matches snapshot in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <CycleTimeBreakdown
        byStage={{
          "In Progress": 4,
          "Code Review": 8,
          "QA Testing": 16,
        }}
      />,
      "dark",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });
});
