import { describe, it, expect } from "vitest";

import { WipBoard } from "@/components/organisms/WipBoard";
import { emptyWipByStage, type WipSnapshot } from "@/domain";
import { renderWithTheme } from "../atoms/test-utils";

function snapshot(): WipSnapshot {
  const fp = emptyWipByStage();
  fp["Code Review"] = 3;
  fp["In Progress"] = 2;
  const sen = emptyWipByStage();
  sen["QA Testing"] = 1;
  return {
    ts: new Date("2026-05-18T12:00:00Z"),
    by_team: { FPCRM: fp, SEN: sen },
  };
}

describe("WipBoard", () => {
  it("renders one row per team in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <WipBoard snapshot={snapshot()} />,
      "dark",
    );
    const teamRows = container.querySelectorAll<HTMLElement>(
      'div[data-team]',
    );
    expect(teamRows.length).toBe(2);
    expect(teamRows[0]?.dataset["team"]).toBe("FPCRM"); // alphabetical
    expect(teamRows[1]?.dataset["team"]).toBe("SEN");
    unmount();
  });

  it("renders one WipChip per non-zero stage for each team", () => {
    const { container, unmount } = renderWithTheme(
      <WipBoard snapshot={snapshot()} />,
      "dark",
    );
    const fpChips = container
      .querySelector<HTMLElement>('div[data-team="FPCRM"]')
      ?.querySelectorAll('[data-testid="wip-chip"]');
    expect(fpChips?.length).toBe(2);
    const senChips = container
      .querySelector<HTMLElement>('div[data-team="SEN"]')
      ?.querySelectorAll('[data-testid="wip-chip"]');
    expect(senChips?.length).toBe(1);
    unmount();
  });

  it("renders the bottleneck callout with stage when provided", () => {
    const { container, unmount } = renderWithTheme(
      <WipBoard snapshot={snapshot()} bottleneck="Code Review" />,
      "dark",
    );
    const callout = container.querySelector<HTMLElement>(
      '[data-testid="bottleneck-callout"]',
    );
    expect(callout?.dataset["stage"]).toBe("Code Review");
    unmount();
  });

  it("shows the idle 'NO ACTIVE TICKETS' state on an empty snapshot", () => {
    const empty: WipSnapshot = { ts: new Date(), by_team: {} };
    const { container, unmount } = renderWithTheme(
      <WipBoard snapshot={empty} />,
      "dark",
    );
    expect(container.textContent).toContain("NO ACTIVE TICKETS");
    unmount();
  });

  it("matches snapshot in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <WipBoard snapshot={snapshot()} bottleneck="Code Review" />,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });
});
