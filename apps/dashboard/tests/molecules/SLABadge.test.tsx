import { describe, it, expect } from "vitest";
import { SLABadge } from "@/components/molecules/SLABadge";
import { renderWithTheme } from "../atoms/test-utils";

describe("SLABadge", () => {
  it("renders the SLA name in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <SLABadge slaName="Review < 24h" breached={false} />,
      "dark",
    );
    expect(container.textContent).toContain("Review < 24h");
    unmount();
  });

  it("marks breached=false via data attribute and uses success tone", () => {
    const { container, unmount } = renderWithTheme(
      <SLABadge slaName="A" breached={false} />,
      "dark",
    );
    const badge = container.querySelector('[data-testid="sla-badge"]');
    expect(badge?.getAttribute("data-breached")).toBe("false");
    const dot = container.querySelector('[data-testid="status-dot"]');
    expect(dot?.getAttribute("data-tone")).toBe("success");
    unmount();
  });

  it("marks breached=true via data attribute and uses error tone", () => {
    const { container, unmount } = renderWithTheme(
      <SLABadge slaName="A" breached={true} />,
      "dark",
    );
    const badge = container.querySelector('[data-testid="sla-badge"]');
    expect(badge?.getAttribute("data-breached")).toBe("true");
    const dot = container.querySelector('[data-testid="status-dot"]');
    expect(dot?.getAttribute("data-tone")).toBe("error");
    unmount();
  });

  it("shows elapsed/target subtext when both are provided", () => {
    const { container, unmount } = renderWithTheme(
      <SLABadge
        slaName="A"
        breached={true}
        elapsedHours={48}
        targetHours={24}
      />,
      "dark",
    );
    expect(container.textContent).toContain("48h / 24h");
    unmount();
  });

  it("matches snapshot in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <SLABadge
        slaName="Review < 24h"
        breached={true}
        elapsedHours={36}
        targetHours={24}
      />,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });
});
