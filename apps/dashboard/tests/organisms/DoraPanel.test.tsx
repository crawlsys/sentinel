import { describe, it, expect } from "vitest";

import { DoraPanel } from "@/components/organisms/DoraPanel";
import type { GetDoraTierResult } from "@/application";
import { renderWithTheme } from "../atoms/test-utils";

const sampleResult: GetDoraTierResult = {
  tiers: {
    lead_time: "elite",
    deploy_freq: "high",
    change_failure_rate: "medium",
    mttr: "low",
  },
  raw: {
    leadTimeHours: 4.5,
    deployFreqPerDay: 0.7,
    cfr: 0.32,
    mttrHours: 180,
  },
};

describe("DoraPanel", () => {
  it("renders all four DORA metrics in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <DoraPanel result={sampleResult} />,
      "dark",
    );
    const text = container.textContent ?? "";
    expect(text).toContain("LEAD TIME");
    expect(text).toContain("DEPLOYS / DAY");
    expect(text).toContain("CHANGE FAILURE");
    expect(text).toContain("MTTR");
    unmount();
  });

  it("maps tier → MetricCard tone correctly", () => {
    const { container, unmount } = renderWithTheme(
      <DoraPanel result={sampleResult} />,
      "dark",
    );
    const cards = container.querySelectorAll<HTMLElement>(
      '[data-testid="metric-card"]',
    );
    expect(cards.length).toBe(4);
    expect(cards[0]?.dataset["tone"]).toBe("success"); // elite
    expect(cards[1]?.dataset["tone"]).toBe("primary"); // high
    expect(cards[2]?.dataset["tone"]).toBe("warn"); // medium
    expect(cards[3]?.dataset["tone"]).toBe("error"); // low
    unmount();
  });

  it("formats CFR as a percent integer", () => {
    const { container, unmount } = renderWithTheme(
      <DoraPanel result={sampleResult} />,
      "dark",
    );
    expect(container.textContent).toContain("32");
    expect(container.textContent).toContain("%");
    unmount();
  });

  it("renders without crashing in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <DoraPanel result={sampleResult} />,
      "light",
    );
    expect(container.querySelector('[data-testid="dora-panel"]')).not.toBeNull();
    unmount();
  });

  it("matches snapshot in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <DoraPanel result={sampleResult} />,
      "dark",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });
});
