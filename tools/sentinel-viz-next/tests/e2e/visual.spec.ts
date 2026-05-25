import { expect, test } from "@playwright/test";

/// Visual QA. Takes screenshots of the main viz at multiple states.
/// Outputs go to ~/.agents/scratch/sentinel-viz-shots/.
test("visual QA: home page + node selection", async ({ page }, testInfo) => {
  const shotsDir = "/home/kcrawley/.agents/scratch/sentinel-viz-shots";

  await page.setViewportSize({ width: 1600, height: 900 });

  page.on("console", (msg) => console.log(`[browser ${msg.type()}]`, msg.text()));
  page.on("pageerror", (err) => console.log("[browser ERROR]", err.message));

  await page.goto("/");

  // Initial paint
  await page.screenshot({ path: `${shotsDir}/01-initial.png`, fullPage: false });

  // Wait for the graph to populate (either via initial fetch or SSE).
  await page
    .waitForFunction(() => {
      const svg = document.querySelector('svg[data-testid="graph-canvas"]');
      return !!svg && svg.querySelectorAll("g.node").length > 0;
    }, { timeout: 15_000 })
    .catch(() => {
      console.log("graph never populated within 15s");
    });

  await page.screenshot({ path: `${shotsDir}/02-graph-populated.png`, fullPage: false });

  // Click first ticker row (events the ticker renders).
  const firstRow = page.getByTestId("ticker-rows").locator("li").first();
  if ((await firstRow.count()) > 0) {
    await firstRow.locator(".cursor-pointer").first().click();
    await page.waitForTimeout(700); // let pan transition settle
    await page.screenshot({ path: `${shotsDir}/03-after-ticker-click.png`, fullPage: false });
  }

  // Try expanding a grouped (×N) row, if any.
  const groupBadge = page.locator('button:has-text("×")').first();
  if ((await groupBadge.count()) > 0) {
    await groupBadge.click();
    await page.screenshot({ path: `${shotsDir}/04-group-expanded.png`, fullPage: false });
  }

  // Click a session node directly in the graph (if any are visible).
  const firstNode = page.locator("svg[data-testid='graph-canvas'] g.node").first();
  if ((await firstNode.count()) > 0) {
    await firstNode.click();
    await page.waitForTimeout(700);
    await page.screenshot({ path: `${shotsDir}/05-node-clicked.png`, fullPage: false });
  }

  // Always succeed — this is a screenshot-only spec, not an assertion.
  expect(true).toBe(true);
  testInfo.attach("shots-dir", { body: shotsDir, contentType: "text/plain" });
});
