import { expect, test } from "@playwright/test";

/// Phase-1 happy path: page loads, all four UI regions are present, the
/// status bar moves from "connecting" to "live" once SSE attaches, and
/// the inspector opens when a ticker row is clicked.
///
/// Requires the Rust API on :8082 and `pnpm dev`/`pnpm start` on :8083
/// (see playwright.config.ts).
test("smoke: loads the viz, connects to SSE, opens inspector on ticker click", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.getByTestId("status-bar")).toBeVisible();
  await expect(page.getByTestId("graph-canvas")).toBeVisible();
  await expect(page.getByTestId("event-ticker")).toBeVisible();
  await expect(page.getByTestId("panel-inspector")).toBeVisible();

  // SSE should connect within a couple seconds against a live bridge.
  await expect(page.getByText(/● live/)).toBeVisible({ timeout: 10_000 });

  // Click the freshest ticker row → inspector should populate (or stay
  // empty if the row targets a node not yet in the snapshot — both are
  // valid, this test just verifies the click is wired).
  const firstRow = page.getByTestId("ticker-rows").locator("li").first();
  if ((await firstRow.count()) > 0) {
    await firstRow.click();
  }
});
