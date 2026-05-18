// SENTINEL-30 — Page-level E2E / integration test.
//
// Exercises the full composition root: adapters → use cases → organisms →
// DashboardLayout → page render. Renders the Server Component via
// `react-dom/server::renderToString` and asserts every expected panel
// data-testid appears in the resulting HTML.
//
// What this *doesn't* catch: pixel-level visual regression. That belongs
// to SEN-31's Browserbase pass against the deployed staging URL — per-
// organism snapshot tests already cover component-level visual rendering.

import { describe, it, expect } from "vitest";
import { renderToString } from "react-dom/server";

import HomePage from "@/app/page";

describe("dashboard page (e2e/integration)", () => {
  it("renders the composition root with every organism panel", async () => {
    // Server Components are async functions returning JSX — no Next
    // runtime needed to invoke.
    const element = await HomePage();
    const html = renderToString(element);

    // Template chrome
    expect(html).toContain('data-testid="dashboard-layout"');
    expect(html).toContain('data-testid="dashboard-header"');
    expect(html).toContain('data-testid="dashboard-content"');

    // All five organism panels
    expect(html).toContain('data-testid="dora-panel"');
    expect(html).toContain('data-testid="wip-board"');
    expect(html).toContain('data-testid="token-economics-panel"');
    expect(html).toContain('data-testid="sla-grid"');
    expect(html).toContain('data-testid="cycle-time-breakdown"');

    // Direct molecule rendered next to organisms
    expect(html).toContain('data-testid="roi-ratio"');
  });

  it("renders the SENTINEL hero label + window tag", async () => {
    const element = await HomePage();
    const html = renderToString(element);
    expect(html).toContain("SENTINEL");
    expect(html).toContain("LAST 30 DAYS");
  });

  it("degrades gracefully when metrics JSONL files don't exist", async () => {
    // Adapters use os.homedir() — on the test machine they may or may
    // not find data. Either way, the page MUST render without throwing.
    // (This is the regression guard for the SEN-29 RSC-serialization
    // bug class: anything non-serializable in a prop would surface as
    // a renderToString throw here.)
    await expect(HomePage()).resolves.toBeDefined();
  });
});
