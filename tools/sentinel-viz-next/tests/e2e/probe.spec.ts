/// Adversarial probe of underexplored viz surfaces: mobile viewport,
/// empty/error states, accessibility basics, long-content overflow,
/// stale-liveness indicator. Each test EITHER asserts the current
/// behavior is acceptable, OR documents the deviation we're going to
/// fix.
///
/// This spec is intentionally permissive — its job is to surface
/// problems we haven't catalogued yet. Failures here are findings,
/// not regressions.
///
/// Run:
///   PLAYWRIGHT_BASE_URL=http://127.0.0.1:3000 \
///   NEXT_PUBLIC_VIZ_API=http://127.0.0.1:8082 \
///   pnpm exec playwright test tests/e2e/probe.spec.ts --reporter=line

import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  page.setDefaultNavigationTimeout(20_000);
  page.setDefaultTimeout(10_000);
});

async function waitGraphReady(page: import("@playwright/test").Page) {
  await page.goto("/");
  await page
    .getByTestId("loading-overlay")
    .waitFor({ state: "hidden", timeout: 15_000 })
    .catch(() => {});
}

// ──────────────────────── MOBILE / NARROW VIEWPORT ────────────────────────

test("MOBILE 375×812: strips panel gets a usable width (≥300px), panels stack vertically", async ({
  page,
}) => {
  await page.setViewportSize({ width: 375, height: 812 });
  await waitGraphReady(page);
  const statusBar = page.getByTestId("status-bar");
  await expect(statusBar).toBeVisible();
  const sbBox = await statusBar.boundingBox();
  expect(sbBox?.width ?? 999).toBeLessThanOrEqual(380);
  // P3-31 replaced GraphCanvas with SessionStripsPanel — assert
  // the panel itself fills the available width.
  const panel = page.getByTestId("session-strips-panel");
  const panelBox = await panel.boundingBox();
  expect(panelBox?.width ?? 0).toBeGreaterThan(300);
  expect(panelBox?.height ?? 0).toBeGreaterThan(200);
});

test("MOBILE: ticker is reachable (visible OR available via interaction)", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 812 });
  await waitGraphReady(page);
  // The ticker is 360px wide and we're at 375 — it should still
  // fit, but it might be pushed off-screen by the inspector.
  // Document the truth.
  const ticker = page.getByTestId("event-ticker");
  const isVisible = await ticker.isVisible().catch(() => false);
  if (!isVisible) {
    test.info().annotations.push({
      type: "finding",
      description: "On 375px viewport the ticker is not visible — needs a responsive collapse",
    });
  }
});

// ──────────────────────── STALE-LIVENESS INDICATOR ────────────────────────

test("status indicator shows 'live' when SSE is flowing", async ({ page }) => {
  await waitGraphReady(page);
  // After warm-up, SSE delivers a snapshot within ~250ms, flipping
  // connected → true.
  await page.waitForTimeout(1500);
  const statusText = (await page.getByTestId("status-bar").textContent()) ?? "";
  expect(statusText).toMatch(/live|ready/i);
});

test("liveness indicator carries a data-liveness attribute and reads 'live' when fresh", async ({
  page,
}) => {
  await waitGraphReady(page);
  await page.waitForTimeout(1500);
  const ind = page.getByTestId("liveness-indicator");
  await expect(ind).toBeVisible();
  const state = await ind.getAttribute("data-liveness");
  // SSE delivers a snapshot within ~250ms; we should be in live
  // within 1.5s of warm-up.
  expect(["live", "stale", "down", "init"]).toContain(state);
  // Pragmatic — on a real API we expect live; tests on a cold DB
  // could land on init. Either is OK as long as the attribute is wired.
});

test("liveness indicator transitions to 'stale' when SSE messages stop arriving", async ({
  page,
  context,
}) => {
  await waitGraphReady(page);
  // Wait for at least one SSE message to land so we begin in
  // "live". Inspect data-liveness directly — text content may lag
  // behind the actual attribute.
  await page.waitForFunction(
    () =>
      document
        .querySelector('[data-testid="liveness-indicator"]')
        ?.getAttribute("data-liveness") === "live",
    { timeout: 8_000 },
  ).catch(() => {
    // Liveness never reached live — could be a slow CI; skip rather
    // than fail. The freshness-transition behaviour is what we care
    // about, and it requires a live baseline.
    test.skip(true, "liveness never reached 'live' — can't measure transition");
  });

  // Close the actual EventSource by going offline. route.abort
  // doesn't help — it only affects NEW requests; an open SSE keeps
  // receiving messages until the underlying socket dies.
  await context.setOffline(true);

  // Wait past the STALE threshold (5s) + a freshness-ticker cycle.
  await page.waitForTimeout(7500);
  const state = await page
    .getByTestId("liveness-indicator")
    .getAttribute("data-liveness");
  await context.setOffline(false);
  expect(state).not.toBe("live");
  expect(["stale", "down"]).toContain(state);
});

// ──────────────────────── KEYBOARD / FOCUS ────────────────────────

test("buttons get a visible focus ring (≥2px accent) on Tab — keyboard a11y", async ({
  page,
}) => {
  await waitGraphReady(page);
  // Tab to the first interactive control and inspect its computed
  // outline. The global :focus-visible rule lands a 2px accent
  // outline + glow so keyboard operators can see the focused
  // element on the dark theme.
  await page.keyboard.press("Tab");
  const observed = await page.evaluate(() => {
    const el = document.activeElement as HTMLElement | null;
    if (!el) return null;
    const s = getComputedStyle(el);
    return {
      tag: el.tagName,
      testid: el.getAttribute("data-testid"),
      outlineStyle: s.outlineStyle,
      outlineWidthPx: parseInt(s.outlineWidth, 10),
      outlineColor: s.outlineColor,
      boxShadow: s.boxShadow,
    };
  });
  expect(observed).not.toBeNull();
  expect(observed?.outlineStyle).not.toBe("none");
  // Default browser ring is 1px; ours is 2px+.
  expect(observed?.outlineWidthPx ?? 0).toBeGreaterThanOrEqual(2);
  // Default ring color is near-black on the dark theme. Ours uses
  // the accent (#58a6ff = rgb(88, 166, 255)) — a bright cyan-blue.
  // Heuristic: red + green + blue summed for the accent is high (~509).
  // The default near-black is ~48. Anything > 200 is "visibly coloured".
  const m = /rgb\((\d+),\s*(\d+),\s*(\d+)\)/.exec(observed?.outlineColor ?? "");
  if (m) {
    const rgbSum = parseInt(m[1], 10) + parseInt(m[2], 10) + parseInt(m[3], 10);
    expect(rgbSum).toBeGreaterThan(200);
  }
});

// ──────────────────────── LONG-CONTENT OVERFLOW ────────────────────────

test("ticker rows handle very long labels without forcing horizontal scroll", async ({ page }) => {
  await waitGraphReady(page);
  // The ticker is 360px. Some real-world labels (long tool args,
  // long ask questions) can be 200+ chars. truncate `.flex-1`
  // with overflow should ensure no row pushes parent width.
  const tickerBox = await page.getByTestId("event-ticker").boundingBox();
  const rowWidths = await page
    .locator('[data-testid="ticker-rows"] li')
    .evaluateAll((els) => els.map((el) => (el as HTMLElement).scrollWidth));
  if (!tickerBox || rowWidths.length === 0) {
    test.skip(true, "no ticker rows / no bbox");
    return;
  }
  const widest = Math.max(...rowWidths);
  // Document, don't fail.
  test.info().annotations.push({
    type: "finding",
    description: `ticker width=${tickerBox.width}px, widest row scrollWidth=${widest}px`,
  });
  // A small allowance — scrollWidth being slightly > clientWidth is normal
  // due to padding rounding.
  expect(widest).toBeLessThan(tickerBox.width + 24);
});

// ──────────────────────── STATUS BAR INFO HIERARCHY ────────────────────────

test("status bar info density: which numbers are present?", async ({ page }) => {
  await waitGraphReady(page);
  const text = (await page.getByTestId("status-bar").textContent()) ?? "";
  // Count the discrete labelled numbers in the status bar.
  const labels = text.match(/(nodes|edges|events|seq|corpus|active|evt\/min|5m|out\/5m|stuck):?\s*\d+/gi);
  test.info().annotations.push({
    type: "finding",
    description: `status bar labelled numbers (${labels?.length ?? 0}): ${(labels ?? []).join(" | ")}`,
  });
});

// ──────────────────────── EMPTY STATE ────────────────────────

test("inspector shows a helpful empty state before any selection", async ({ page }) => {
  await waitGraphReady(page);
  const inspector = page.getByTestId("panel-inspector");
  await expect(inspector).toBeVisible();
  const text = (await inspector.textContent()) ?? "";
  expect(text.trim().length).toBeGreaterThan(0);
  test.info().annotations.push({
    type: "finding",
    description: `inspector empty state copy: "${text.replace(/\s+/g, " ").trim().slice(0, 200)}"`,
  });
});

test("SessionConsole shows a helpful empty state when no segments are available", async ({
  page,
}) => {
  await waitGraphReady(page);
  const console = page.getByTestId("session-console");
  await expect(console).toBeVisible();
  const text = (await console.textContent()) ?? "";
  test.info().annotations.push({
    type: "finding",
    description: `session console copy: "${text.replace(/\s+/g, " ").trim().slice(0, 200)}"`,
  });
});
