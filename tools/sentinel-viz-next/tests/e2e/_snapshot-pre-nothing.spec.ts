import { test } from "@playwright/test";

const OUT = "/home/kcrawley/.agents/scratch/viz-snapshot-pre-nothing";

const VIEWS = [
  { name: "desktop-1600", w: 1600, h: 900 },
  { name: "desktop-1280", w: 1280, h: 800 },
  { name: "tablet-768",   w: 768,  h: 1024 },
  { name: "mobile-414",   w: 414,  h: 896 },
  { name: "mobile-375",   w: 375,  h: 812 },
];

for (const v of VIEWS) {
  test(`snapshot ${v.name}`, async ({ page }, info) => {
    await page.setViewportSize({ width: v.w, height: v.h });
    await page.goto("http://172.16.100.22:3000/");
    await page.waitForTimeout(8000); // let AI summaries land
    await page.screenshot({ path: `${OUT}/${v.name}-idle.png`, fullPage: false });

    // Tap a strip to capture modal/inspector state
    const strip = page.locator('[data-testid="session-strip"]').first();
    if (await strip.count() > 0) {
      await strip.click();
      await page.waitForTimeout(800);
      await page.screenshot({ path: `${OUT}/${v.name}-inspector.png`, fullPage: false });
    }

    info.attach("output-dir", { body: OUT, contentType: "text/plain" });
  });
}
