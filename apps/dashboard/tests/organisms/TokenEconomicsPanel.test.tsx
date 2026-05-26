import { describe, it, expect } from "vitest";

import { TokenEconomicsPanel } from "@/components/organisms/TokenEconomicsPanel";
import { makeDollars } from "@/domain";
import type { GetTokenEconomicsResult } from "@/application";
import { renderWithTheme } from "../atoms/test-utils";

function fixture(
  overrides: Partial<GetTokenEconomicsResult> = {},
): GetTokenEconomicsResult {
  return {
    totalCostUsd: makeDollars(1234),
    cacheHitRate: 0.65,
    byTicket: [],
    ...overrides,
  };
}

describe("TokenEconomicsPanel", () => {
  it("renders both cards + the cache-hit bar in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <TokenEconomicsPanel result={fixture()} />,
      "dark",
    );
    expect(container.textContent).toContain("TOTAL SPEND");
    expect(container.textContent).toContain("CACHE HIT");
    expect(container.textContent).toContain("$1,234");
    expect(container.textContent).toContain("65");
    expect(
      container.querySelector('[data-testid="segmented-bar"]'),
    ).not.toBeNull();
    unmount();
  });

  it("uses success tone when cache hit ≥ 60%", () => {
    const { container, unmount } = renderWithTheme(
      <TokenEconomicsPanel result={fixture({ cacheHitRate: 0.6 })} />,
      "dark",
    );
    const cards = container.querySelectorAll<HTMLElement>(
      '[data-testid="metric-card"]',
    );
    // [0] = TOTAL SPEND (primary always), [1] = CACHE HIT
    expect(cards[1]?.dataset["tone"]).toBe("success");
    unmount();
  });

  it("uses warn tone when 30% ≤ cache hit < 60%", () => {
    const { container, unmount } = renderWithTheme(
      <TokenEconomicsPanel result={fixture({ cacheHitRate: 0.4 })} />,
      "dark",
    );
    const cards = container.querySelectorAll<HTMLElement>(
      '[data-testid="metric-card"]',
    );
    expect(cards[1]?.dataset["tone"]).toBe("warn");
    unmount();
  });

  it("uses error tone when cache hit < 30%", () => {
    const { container, unmount } = renderWithTheme(
      <TokenEconomicsPanel result={fixture({ cacheHitRate: 0.1 })} />,
      "dark",
    );
    const cards = container.querySelectorAll<HTMLElement>(
      '[data-testid="metric-card"]',
    );
    expect(cards[1]?.dataset["tone"]).toBe("error");
    unmount();
  });

  it("matches snapshot in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <TokenEconomicsPanel result={fixture()} />,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });
});
