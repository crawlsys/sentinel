/// Adversarial judge harness. Runs each Gherkin scenario from
/// `tests/specs/viz.feature` against the live :8083 viewer + :8082
/// API, captures evidence (screenshot, DOM state, network), and
/// writes a JSON report to `~/.agents/scratch/sentinel-viz-judge/`.
///
/// This file is NOT the spec — it's the runner. The spec is the
/// .feature file; the judge agent reads the spec, runs THIS file,
/// then judges whether the captured evidence satisfies each Then.
///
/// Run:
///   pnpm exec playwright test tests/e2e/judge.spec.ts --reporter=line

import { expect, test } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";

const OUT_DIR = "/home/kcrawley/.agents/scratch/sentinel-viz-judge";
fs.mkdirSync(OUT_DIR, { recursive: true });

interface Evidence {
  scenario: string;
  startedAt: string;
  shots: Record<string, string>;
  dom: Record<string, unknown>;
  network: Array<{ url: string; status: number; ms: number }>;
  consoleErrors: string[];
  perf: Record<string, number>;
  notes: string[];
}

function fresh(name: string): Evidence {
  return {
    scenario: name,
    startedAt: new Date().toISOString(),
    shots: {},
    dom: {},
    network: [],
    consoleErrors: [],
    perf: {},
    notes: [],
  };
}

function writeReport(report: Evidence) {
  const safe = report.scenario.replace(/[^a-z0-9]+/gi, "-").toLowerCase();
  fs.writeFileSync(
    path.join(OUT_DIR, `${safe}.json`),
    JSON.stringify(report, null, 2),
  );
}

test.beforeEach(async ({ page }) => {
  page.setDefaultNavigationTimeout(20_000);
  page.setDefaultTimeout(10_000);
});

// ---------- COLD LOAD ----------

test("cold-load-time", async ({ page }, testInfo) => {
  const ev = fresh("cold-load-time");
  page.on("response", (r) => {
    if (r.url().includes("/api/")) {
      ev.network.push({ url: r.url(), status: r.status(), ms: 0 });
    }
  });
  page.on("console", (m) => {
    if (m.type() === "error") ev.consoleErrors.push(m.text());
  });

  const t0 = Date.now();
  await page.goto("/");
  await page.screenshot({ path: `${OUT_DIR}/cold-load-initial.png` });
  ev.shots.initial = `${OUT_DIR}/cold-load-initial.png`;

  // Wait for the loading overlay to vanish.
  const overlay = page.getByTestId("loading-overlay");
  let overlayGoneMs: number | null = null;
  try {
    await overlay.waitFor({ state: "hidden", timeout: 15_000 });
    overlayGoneMs = Date.now() - t0;
  } catch {
    ev.notes.push("loading overlay never vanished within 15s");
  }
  ev.perf.overlayGoneMs = overlayGoneMs ?? -1;

  // Capture status bar + counts.
  ev.dom.statusBarText =
    (await page.getByTestId("status-bar").textContent())?.replace(/\s+/g, " ").trim() ?? "";
  ev.dom.svgNodeCount = await page
    .locator('svg[data-testid="graph-canvas"] g.node')
    .count();
  ev.dom.tickerRowCount = await page.getByTestId("ticker-rows").locator("li").count();

  await page.screenshot({ path: `${OUT_DIR}/cold-load-final.png` });
  ev.shots.final = `${OUT_DIR}/cold-load-final.png`;

  writeReport(ev);
  testInfo.attach("evidence", { body: JSON.stringify(ev, null, 2), contentType: "application/json" });
  await expect(page.getByTestId("graph-canvas")).toBeVisible();
});

// ---------- TICKER ----------

test("ticker-labels-and-times", async ({ page }, testInfo) => {
  const ev = fresh("ticker-labels-and-times");
  await page.goto("/");
  await page.getByTestId("loading-overlay").waitFor({ state: "hidden", timeout: 15_000 }).catch(() => {});

  const rows = await page.getByTestId("ticker-rows").locator("li").all();
  ev.dom.totalRows = rows.length;
  const sample = await Promise.all(
    rows.slice(0, 20).map(async (li) => {
      const text = (await li.textContent())?.replace(/\s+/g, " ").trim() ?? "";
      const dotColor = await li
        .locator("span.inline-block")
        .first()
        .evaluate((el) => getComputedStyle(el).backgroundColor)
        .catch(() => "(no dot)");
      return { text, dotColor };
    }),
  );
  ev.dom.sample = sample;

  // Look for blank labels (red flag).
  ev.dom.blankLabels = sample.filter((r) =>
    /\d\d:\d\d:\d\d\s+(s:|$)/.test(r.text),
  ).length;

  // Look for HH:MM:SS in each row.
  ev.dom.rowsWithTimestamp = sample.filter((r) =>
    /\d\d:\d\d:\d\d/.test(r.text),
  ).length;

  // Look for "—" dashes (failed-parse marker).
  ev.dom.rowsWithDash = sample.filter((r) => /—/.test(r.text)).length;

  await page.screenshot({ path: `${OUT_DIR}/ticker-sample.png` });
  ev.shots.sample = `${OUT_DIR}/ticker-sample.png`;
  writeReport(ev);
  testInfo.attach("evidence", { body: JSON.stringify(ev, null, 2), contentType: "application/json" });
});

test("ticker-grouped-rows-expand", async ({ page }, testInfo) => {
  const ev = fresh("ticker-grouped-rows-expand");
  await page.goto("/");
  await page.getByTestId("loading-overlay").waitFor({ state: "hidden", timeout: 15_000 }).catch(() => {});

  const badge = page.locator('button:has-text("×")').first();
  ev.dom.groupedRowsPresent = await badge.count();
  if (ev.dom.groupedRowsPresent === 0) {
    ev.notes.push("no ×N grouped rows currently in the ticker");
  } else {
    await badge.click();
    await page.waitForTimeout(300);
    await page.screenshot({ path: `${OUT_DIR}/ticker-group-expanded.png` });
    ev.shots.expanded = `${OUT_DIR}/ticker-group-expanded.png`;
    // Count expanded members
    ev.dom.expandedMembers = await page
      .locator('ul[class*="border-dashed"] li')
      .count();
  }
  writeReport(ev);
  testInfo.attach("evidence", { body: JSON.stringify(ev, null, 2), contentType: "application/json" });
});

// ---------- GRAPH ----------

test("graph-default-hides-hooks", async ({ page }, testInfo) => {
  const ev = fresh("graph-default-hides-hooks");
  await page.goto("/");
  await page.getByTestId("loading-overlay").waitFor({ state: "hidden", timeout: 15_000 }).catch(() => {});

  const totalNodes = await page.locator('svg[data-testid="graph-canvas"] g.node').count();
  const hookNodes = await page
    .locator('svg[data-testid="graph-canvas"] g.node[data-node-id^="SentinelHookInvocation"]')
    .count();
  ev.dom.totalNodes = totalNodes;
  ev.dom.hookNodes = hookNodes;
  await page.screenshot({ path: `${OUT_DIR}/graph-default.png` });
  ev.shots.default = `${OUT_DIR}/graph-default.png`;
  writeReport(ev);
  testInfo.attach("evidence", { body: JSON.stringify(ev, null, 2), contentType: "application/json" });
});

test("graph-chain-edges-render", async ({ page }, testInfo) => {
  const ev = fresh("graph-chain-edges-render");
  await page.goto("/");
  await page.getByTestId("loading-overlay").waitFor({ state: "hidden", timeout: 15_000 }).catch(() => {});

  // Edges are drawn as <line class="edge"> — stroke colour encodes the kind.
  const sample = await page.evaluate(() => {
    const lines = Array.from(document.querySelectorAll('svg[data-testid="graph-canvas"] line.edge'));
    const tally: Record<string, number> = {};
    for (const ln of lines) {
      const stroke = (ln.getAttribute("stroke") ?? "").toLowerCase();
      tally[stroke] = (tally[stroke] ?? 0) + 1;
    }
    return { total: lines.length, byStroke: tally };
  });
  ev.dom.edges = sample;
  writeReport(ev);
  testInfo.attach("evidence", { body: JSON.stringify(ev, null, 2), contentType: "application/json" });
});

// ---------- INSPECTOR ----------

test("inspector-friendly-title", async ({ page }, testInfo) => {
  const ev = fresh("inspector-friendly-title");
  await page.goto("/");
  await page.getByTestId("loading-overlay").waitFor({ state: "hidden", timeout: 15_000 }).catch(() => {});

  // Click the first ticker row.
  const firstRow = page.getByTestId("ticker-rows").locator("li").first();
  if (await firstRow.count()) {
    await firstRow.locator(".cursor-pointer").first().click();
    await page.waitForTimeout(800);
    const h3 = await page.locator('[data-testid="panel-inspector"] h3').textContent();
    ev.dom.h3 = h3 ?? "";
    ev.dom.h3HasSentinelPrefix = (h3 ?? "").includes("Sentinel");
  }
  await page.screenshot({ path: `${OUT_DIR}/inspector-clicked.png` });
  ev.shots.clicked = `${OUT_DIR}/inspector-clicked.png`;
  writeReport(ev);
  testInfo.attach("evidence", { body: JSON.stringify(ev, null, 2), contentType: "application/json" });
});

test("inspector-activity-populates", async ({ page }, testInfo) => {
  const ev = fresh("inspector-activity-populates");
  await page.goto("/");
  await page.getByTestId("loading-overlay").waitFor({ state: "hidden", timeout: 15_000 }).catch(() => {});

  const firstRow = page.getByTestId("ticker-rows").locator("li").first();
  if (await firstRow.count()) {
    await firstRow.locator(".cursor-pointer").first().click();
    // Wait up to 4s for segments to appear.
    let segs = 0;
    for (let i = 0; i < 40; i++) {
      segs = await page.locator('[data-testid="activity-segments"] li').count();
      if (segs > 0) break;
      await page.waitForTimeout(100);
    }
    ev.dom.segmentCount = segs;
    ev.perf.segmentsResolvedMs = segs > 0 ? 100 * (segs > 0 ? 1 : 0) : -1;
  }
  await page.screenshot({ path: `${OUT_DIR}/inspector-activity.png` });
  ev.shots.activity = `${OUT_DIR}/inspector-activity.png`;
  writeReport(ev);
  testInfo.attach("evidence", { body: JSON.stringify(ev, null, 2), contentType: "application/json" });
});

test("inspector-at-ts-changes-with-row-click", async ({ page }, testInfo) => {
  const ev = fresh("inspector-at-ts-changes-with-row-click");
  await page.goto("/");
  await page.getByTestId("loading-overlay").waitFor({ state: "hidden", timeout: 15_000 }).catch(() => {});

  const rows = await page.getByTestId("ticker-rows").locator("li").all();
  if (rows.length < 3) {
    ev.notes.push("not enough ticker rows to test");
  } else {
    await rows[0].locator(".cursor-pointer").first().click();
    await page.waitForTimeout(900);
    const headerA = await page.locator('[data-testid="panel-inspector"] h4').first().textContent();
    ev.dom.firstClickHeader = (headerA ?? "").replace(/\s+/g, " ").trim();
    await rows[2].locator(".cursor-pointer").first().click();
    await page.waitForTimeout(900);
    const headerB = await page.locator('[data-testid="panel-inspector"] h4').first().textContent();
    ev.dom.secondClickHeader = (headerB ?? "").replace(/\s+/g, " ").trim();
    ev.dom.headersDiffered = ev.dom.firstClickHeader !== ev.dom.secondClickHeader;
  }
  writeReport(ev);
  testInfo.attach("evidence", { body: JSON.stringify(ev, null, 2), contentType: "application/json" });
});

// ---------- LIVE / RESILIENCE ----------

test("graph-stable-during-sse", async ({ page }, testInfo) => {
  const ev = fresh("graph-stable-during-sse");
  await page.goto("/");
  await page.getByTestId("loading-overlay").waitFor({ state: "hidden", timeout: 15_000 }).catch(() => {});
  // Let the sim settle for 3s, then sample positions.
  await page.waitForTimeout(3000);
  const before = await page.evaluate(() => {
    return Array.from(document.querySelectorAll('svg[data-testid="graph-canvas"] g.node')).map((g) => {
      const t = (g as SVGGElement).getAttribute("transform") ?? "";
      const m = /translate\(\s*([-\d.]+)\s*,\s*([-\d.]+)\s*\)/.exec(t);
      return {
        id: (g as SVGGElement).getAttribute("data-node-id") ?? "",
        x: m ? parseFloat(m[1]) : 0,
        y: m ? parseFloat(m[2]) : 0,
      };
    });
  });
  await page.waitForTimeout(5000);
  const after = await page.evaluate(() => {
    return Array.from(document.querySelectorAll('svg[data-testid="graph-canvas"] g.node')).map((g) => {
      const t = (g as SVGGElement).getAttribute("transform") ?? "";
      const m = /translate\(\s*([-\d.]+)\s*,\s*([-\d.]+)\s*\)/.exec(t);
      return {
        id: (g as SVGGElement).getAttribute("data-node-id") ?? "",
        x: m ? parseFloat(m[1]) : 0,
        y: m ? parseFloat(m[2]) : 0,
      };
    });
  });
  const byId = new Map(before.map((b) => [b.id, b]));
  const deltas = after.map((a) => {
    const b = byId.get(a.id);
    return b ? Math.hypot(a.x - b.x, a.y - b.y) : -1;
  });
  ev.dom.maxDelta = Math.max(...deltas.filter((d) => d >= 0));
  ev.dom.meanDelta = deltas.length ? deltas.filter((d) => d >= 0).reduce((s, d) => s + d, 0) / deltas.length : 0;
  ev.dom.beforeCount = before.length;
  ev.dom.afterCount = after.length;
  writeReport(ev);
  testInfo.attach("evidence", { body: JSON.stringify(ev, null, 2), contentType: "application/json" });
});

test("tab-visibility-no-jump", async ({ page }, testInfo) => {
  const ev = fresh("tab-visibility-no-jump");
  await page.goto("/");
  await page.getByTestId("loading-overlay").waitFor({ state: "hidden", timeout: 15_000 }).catch(() => {});
  await page.waitForTimeout(2500);

  const sample = async () =>
    page.evaluate(() => {
      return Array.from(document.querySelectorAll('svg[data-testid="graph-canvas"] g.node')).map((g) => {
        const t = (g as SVGGElement).getAttribute("transform") ?? "";
        const m = /translate\(\s*([-\d.]+)\s*,\s*([-\d.]+)\s*\)/.exec(t);
        return {
          id: (g as SVGGElement).getAttribute("data-node-id") ?? "",
          x: m ? parseFloat(m[1]) : 0,
          y: m ? parseFloat(m[2]) : 0,
        };
      });
    });

  const before = await sample();

  // Simulate tab hidden.
  await page.evaluate(() => {
    Object.defineProperty(document, "visibilityState", { value: "hidden", configurable: true });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await page.waitForTimeout(5000);
  await page.evaluate(() => {
    Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await page.waitForTimeout(500);

  const after = await sample();
  const byId = new Map(before.map((b) => [b.id, b]));
  const deltas = after.map((a) => {
    const b = byId.get(a.id);
    return b ? Math.hypot(a.x - b.x, a.y - b.y) : -1;
  });
  ev.dom.maxDelta = deltas.length ? Math.max(...deltas.filter((d) => d >= 0)) : 0;
  writeReport(ev);
  testInfo.attach("evidence", { body: JSON.stringify(ev, null, 2), contentType: "application/json" });
});

// ---------- PERF ----------

test("api-graph-warm-latency", async ({ request }, testInfo) => {
  const ev = fresh("api-graph-warm-latency");
  // Warm
  await request.get("http://127.0.0.1:8082/api/graph");
  // Sample 3 warm hits.
  const samples: number[] = [];
  for (let i = 0; i < 3; i++) {
    const t = Date.now();
    const r = await request.get("http://127.0.0.1:8082/api/graph");
    expect(r.ok()).toBeTruthy();
    samples.push(Date.now() - t);
  }
  ev.perf.warmSamplesMs = Object.fromEntries(samples.map((v, i) => [i, v])) as Record<string, number>;
  ev.perf.maxWarmMs = Math.max(...samples);
  writeReport(ev);
  testInfo.attach("evidence", { body: JSON.stringify(ev, null, 2), contentType: "application/json" });
});

test("api-activity-warm-latency", async ({ request }, testInfo) => {
  const ev = fresh("api-activity-warm-latency");
  const g = await request.get("http://127.0.0.1:8082/api/graph");
  const body = (await g.json()) as { nodes: Array<{ type: string; data: { session_id?: string } }> };
  const sid = body.nodes.find((n) => n.type === "SentinelSession" && n.data?.session_id)?.data?.session_id;
  if (!sid) {
    ev.notes.push("no session id available to probe");
    writeReport(ev);
    return;
  }
  await request.get(`http://127.0.0.1:8082/api/activity/${sid}`); // warm
  const samples: number[] = [];
  for (let i = 0; i < 3; i++) {
    const t = Date.now();
    const r = await request.get(`http://127.0.0.1:8082/api/activity/${sid}`);
    expect(r.ok()).toBeTruthy();
    samples.push(Date.now() - t);
  }
  ev.perf.warmSamplesMs = Object.fromEntries(samples.map((v, i) => [i, v])) as Record<string, number>;
  ev.perf.maxWarmMs = Math.max(...samples);
  writeReport(ev);
  testInfo.attach("evidence", { body: JSON.stringify(ev, null, 2), contentType: "application/json" });
});
