import { describe, it, expect } from "vitest";

import { SLAGrid, type SLAGridEntry } from "@/components/organisms/SLAGrid";
import { makeTicketIdentifier, type SLABreach } from "@/domain";
import { renderWithTheme } from "../atoms/test-utils";

const slas: SLAGridEntry[] = [
  { id: "review-24h", name: "Code Review < 24h", target_hours: 24 },
  { id: "qa-48h", name: "QA < 48h", target_hours: 48 },
];

const breaches: SLABreach[] = [
  {
    sla_id: "review-24h",
    ticket_id: makeTicketIdentifier("FPCRM-1"),
    breached_at: new Date(),
    elapsed_hours: 36,
  },
  {
    sla_id: "review-24h",
    ticket_id: makeTicketIdentifier("FPCRM-2"),
    breached_at: new Date(),
    elapsed_hours: 50, // larger → should win the max
  },
];

describe("SLAGrid", () => {
  it("renders one badge per SLA in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <SLAGrid slas={slas} breaches={[]} />,
      "dark",
    );
    expect(
      container.querySelectorAll('[data-testid="sla-badge"]').length,
    ).toBe(2);
    unmount();
  });

  it("marks a SLA as breached when at least one breach matches its id", () => {
    const { container, unmount } = renderWithTheme(
      <SLAGrid slas={slas} breaches={breaches} />,
      "dark",
    );
    const badges = container.querySelectorAll<HTMLElement>(
      '[data-testid="sla-badge"]',
    );
    expect(badges[0]?.dataset["breached"]).toBe("true");
    expect(badges[1]?.dataset["breached"]).toBe("false");
    unmount();
  });

  it("renders elapsed/target subtext using the worst elapsed for the SLA", () => {
    const { container, unmount } = renderWithTheme(
      <SLAGrid slas={slas} breaches={breaches} />,
      "dark",
    );
    // Worst elapsed is 50h, target 24h.
    expect(container.textContent).toContain("50h / 24h");
    unmount();
  });

  it("renders no badges when no SLAs are configured", () => {
    const { container, unmount } = renderWithTheme(
      <SLAGrid slas={[]} breaches={[]} />,
      "dark",
    );
    expect(
      container.querySelectorAll('[data-testid="sla-badge"]').length,
    ).toBe(0);
    unmount();
  });

  it("matches snapshot in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <SLAGrid slas={slas} breaches={breaches} />,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });
});
