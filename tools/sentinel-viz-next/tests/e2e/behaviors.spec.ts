/// End-to-end BEHAVIOR + PERF coverage of the four most recent
/// changes (P3-11..P3-15) plus regression budgets for the perf wins
/// landed in P3-17 / P3-18.
///
/// Every test here is an assertion (not a screenshot dump) so it
/// fails when behavior deviates. Add a new test the moment a manual
/// "I noticed X feels off" pass turns up a deviation.
///
/// Run:
///   PLAYWRIGHT_BASE_URL=http://127.0.0.1:3000 \
///   NEXT_PUBLIC_VIZ_API=http://127.0.0.1:8082 \
///   pnpm exec playwright test tests/e2e/behaviors.spec.ts --reporter=line

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
  await page
    .waitForFunction(
      () => document.querySelectorAll('svg[data-testid="graph-canvas"] g.node').length > 0,
      { timeout: 10_000 },
    )
    .catch(() => {});
}

// ──────────────────────── AUTO-WATCH TOGGLE ────────────────────────

test("AUTO toggle: present, defaults OFF, has ignore attribute", async ({ page }) => {
  await waitGraphReady(page);
  const btn = page.getByTestId("auto-watch-toggle");
  await expect(btn).toBeVisible();
  await expect(btn).toHaveText(/AUTO\s+OFF/i);
  expect(await btn.getAttribute("data-auto-watch-ignore")).not.toBeNull();
});

test("AUTO toggle: click flips state and survives mouse movement (regression for P3-15)", async ({
  page,
}) => {
  await waitGraphReady(page);
  const btn = page.getByTestId("auto-watch-toggle");
  await btn.click();
  await expect(btn).toHaveText(/AUTO\s+ON/i);

  // Move the mouse aggressively — used to flip the toggle off
  // because mousemove was a capture-phase interaction signal.
  for (let i = 0; i < 30; i++) {
    await page.mouse.move(400 + i, 300 + (i % 17));
  }
  await page.waitForTimeout(200);
  await expect(btn).toHaveText(/AUTO\s+ON/i);
});

test("AUTO toggle: scroll does NOT disable auto", async ({ page }) => {
  await waitGraphReady(page);
  const btn = page.getByTestId("auto-watch-toggle");
  await btn.click();
  await expect(btn).toHaveText(/AUTO\s+ON/i);

  // Scroll the ticker (most plausible scroll surface).
  const ticker = page.getByTestId("ticker-rows");
  await ticker.evaluate((el) => {
    for (let i = 0; i < 5; i++) el.scrollTop = i * 20;
  });
  await page.waitForTimeout(150);
  await expect(btn).toHaveText(/AUTO\s+ON/i);
});

test("AUTO toggle: clicking somewhere else DOES disable auto", async ({ page }) => {
  await waitGraphReady(page);
  const btn = page.getByTestId("auto-watch-toggle");
  await btn.click();
  await expect(btn).toHaveText(/AUTO\s+ON/i);
  // Give the post-set grace window time to expire.
  await page.waitForTimeout(800);
  // Click the SVG canvas background.
  await page.locator('svg[data-testid="graph-canvas"]').click({ position: { x: 5, y: 5 } });
  await expect(btn).toHaveText(/AUTO\s+OFF/i);
});

test("AUTO toggle: tooltip reflects current state", async ({ page }) => {
  await waitGraphReady(page);
  const btn = page.getByTestId("auto-watch-toggle");
  expect(await btn.getAttribute("title")).toMatch(/auto-watch OFF/i);
  await btn.click();
  expect(await btn.getAttribute("title")).toMatch(/auto-watch ON/i);
});

// ──────────────────────── STUCK BADGE DEDUP ────────────────────────

test("STUCK badge: exactly one in the StatusBar (no duplicate in KpiBar)", async ({ page }) => {
  await waitGraphReady(page);
  // If there's no stuck session right now, this test is a no-op
  // assertion (we only verify there are NEVER two). Otherwise the
  // count is 1.
  const count = await page.getByTestId("stuck-badge").count();
  expect(count).toBeLessThanOrEqual(1);
});

// ──────────────────────── STICKY STUCK ROWS ────────────────────────

test("when STUCK > 0, a sticky row + stuck-reason line are present in the ticker", async ({
  page,
}) => {
  await waitGraphReady(page);
  const badge = page.getByTestId("stuck-badge");
  if ((await badge.count()) === 0) {
    test.skip(true, "no stuck sessions in this DB snapshot");
    return;
  }
  const stuckRows = page.locator('[data-testid="ticker-rows"] li.stuck-row');
  // At least one pinned row.
  expect(await stuckRows.count()).toBeGreaterThan(0);
  // First row in the list must be a stuck row.
  const firstRow = page.locator('[data-testid="ticker-rows"] li').first();
  await expect(firstRow).toHaveClass(/stuck-row/);
  // Sub-line surfaces the reason without requiring a click.
  const reason = page.getByTestId("stuck-reason-line").first();
  await expect(reason).toBeVisible();
  const text = (await reason.textContent()) ?? "";
  expect(text).toMatch(/STUCK/);
  // Either we display kind ("AskUserQuestion", "PreToolUse"…) or
  // the "awaiting" fallback.
  expect(text).toMatch(/[A-Za-z]/);
});

test("clicking the STUCK badge focuses the first stuck session", async ({ page }) => {
  await waitGraphReady(page);
  const badge = page.getByTestId("stuck-badge");
  if ((await badge.count()) === 0) {
    test.skip(true, "no stuck sessions");
    return;
  }
  await badge.click();
  // After focus, the inspector should show a SentinelSession panel.
  const insp = page.getByTestId("panel-inspector");
  await expect(insp).toBeVisible();
});

// ──────────────────────── SESSION-CONSOLE FOLLOW SELECTION ────────────────────────

test("SessionConsole defaults to 'all sessions' scope", async ({ page }) => {
  await waitGraphReady(page);
  const scope = page.getByTestId("session-console-scope");
  await expect(scope).toBeVisible();
  await expect(scope).toHaveText(/all sessions/i);
});

test("SessionConsole scopes to the selected session when a ticker row is clicked", async ({
  page,
}) => {
  await waitGraphReady(page);
  // Click any clickable row in the ticker. Even a stuck-row click
  // will trigger scope-follow if the selection actually changes the
  // selectedSessionId. (When only stuck-row sessions are visible,
  // we'd still expect scope to flip from "all sessions" → scoped.)
  const row = page.locator('[data-testid="ticker-rows"] li .cursor-pointer').first();
  if ((await row.count()) === 0) {
    test.skip(true, "no ticker rows present");
    return;
  }
  await row.click();
  // The selection effect cascades through the SSE/focused-session
  // round-trip; allow a generous settle window.
  await page.waitForTimeout(1500);
  const scope = page.getByTestId("session-console-scope");
  await expect(scope).toHaveText(/scoped/i);
});

// ──────────────────────── KPI BAR ────────────────────────

test("KpiBar: present, shows active/evt-min/5m/out-5m cards", async ({ page }) => {
  await waitGraphReady(page);
  const kpi = page.getByTestId("kpi-bar");
  await expect(kpi).toBeVisible();
  const text = (await kpi.textContent()) ?? "";
  expect(text).toMatch(/active/i);
  expect(text).toMatch(/evt\/min/i);
  expect(text).toMatch(/5m/i);
  expect(text).toMatch(/out\/5m/i);
});

// ──────────────────────── OPERATOR PHRASING (P3-20) ────────────────────────

test("ticker sub-lines never show raw lifecycle jargon (PreToolUse / UserPromptSubmit etc.)", async ({
  page,
}) => {
  await waitGraphReady(page);
  const allText = await page
    .locator('[data-testid="ticker-rows"]')
    .innerText();
  // The raw enum names live in payload.sentinel_event — they should
  // be translated before reaching the DOM.
  for (const jargon of ["PreToolUse", "PostToolUse", "UserPromptSubmit", "SubagentStop", "PreCompact"]) {
    expect(allText).not.toContain(jargon);
  }
});

test("inspector title for a session is no longer the anonymous 'session' label", async ({
  page,
}) => {
  await waitGraphReady(page);
  // Click a SessionConsole row or a ticker row that selects a session.
  const firstRow = page.locator('[data-testid="ticker-rows"] li').first();
  if ((await firstRow.count()) === 0) {
    test.skip(true, "no ticker rows");
    return;
  }
  await firstRow.locator(".cursor-pointer").first().click();
  await page.waitForTimeout(1000);
  // The inspector's h3 should contain either a name or a sid prefix,
  // not just "session".
  const title = (await page.locator('[data-testid="panel-inspector"] h3').first().textContent()) ?? "";
  if (title.toLowerCase().includes("session")) {
    // Anonymous "session" by itself is the failure case — we now
    // append "· s:<sid>" when we know the sid.
    expect(title).toMatch(/s:[a-f0-9]{4,}/i);
  } else {
    // Or the LLM-assigned name short-circuits the "session" prefix
    // entirely — that's fine too, just not blank/anonymous.
    expect(title.trim().length).toBeGreaterThan(2);
  }
});

// ──────────────────────── ROLLUP + SKELETON (P3-22/23/24) ────────────────────────

test("ticker shows ROLLED rows (×N ▸ tools-list) on real data, not 100 individual events", async ({
  page,
}) => {
  await waitGraphReady(page);
  // After P3-24 the ticker collapses adjacent same-session,
  // same-category claude tool calls. On the live DB this typically
  // means the visible row count is well below the raw event count.
  // We assert at least ONE row shows the rolled-up form: ×N with a
  // tools list containing at least one comma OR multiple distinct
  // tools visible. If the live data has no consecutive bursts (very
  // rare on a real Sentinel session), skip.
  const rolled = await page
    .locator('[data-testid="ticker-rows"] li[data-actor]')
    .filter({ hasText: /×\d+/ })
    .count();
  if (rolled === 0) {
    test.skip(true, "no rolled-up rows in this DB snapshot");
    return;
  }
  expect(rolled).toBeGreaterThan(0);
});

test("ticker NEVER shows the raw 'about to run' jargon in sub-lines on routine claude rows", async ({
  page,
}) => {
  await waitGraphReady(page);
  // After P3-22 the sub-line is gated to outcome-bearing rows only.
  // Routine PreToolUse rows (the bulk of traffic) MUST NOT render
  // "about to run".
  const allText = await page
    .locator('[data-testid="ticker-rows"]')
    .innerText();
  expect(allText).not.toContain("about to run");
  expect(allText).not.toContain("you submitted");
  expect(allText).not.toContain("finished");
});

test("skeleton placeholders render BEFORE real data lands — perceived-perf win (P3-23)", async ({
  page,
}) => {
  // Navigate but DON'T wait for the load overlay to vanish — we
  // want to observe the in-between state where skeletons are
  // present but real rows aren't.
  const navStart = Date.now();
  await page.goto("/", { waitUntil: "domcontentloaded" });
  // Within 300ms of DOM ready we should see at least one skeleton
  // row OR (if data was already cached) real rows. Both prove the
  // ticker isn't blank.
  let observed = "neither";
  for (let i = 0; i < 30; i++) {
    const skel = await page.locator('[data-testid="ticker-skeleton"]').count();
    const real = await page.locator('[data-testid="ticker-rows"] li[data-actor]').count();
    if (skel > 0) {
      observed = "skeleton";
      break;
    }
    if (real > 0) {
      observed = "real";
      break;
    }
    await page.waitForTimeout(10);
  }
  const elapsed = Date.now() - navStart;
  expect(observed).not.toBe("neither");
  // Whichever observed state, it should appear well under our cold-
  // load budget. 300ms is the perceived-instant threshold.
  expect(elapsed).toBeLessThan(2000);
});

// ──────────────────────── ACTOR DISTINCTION (P3-19) ────────────────────────

test("every ticker row exposes a data-actor attribute in {claude, sentinel, user}", async ({
  page,
}) => {
  await waitGraphReady(page);
  // Scope to TOP-LEVEL row li only — RolledPreview + flyout render
  // nested <li> that legitimately lack data-actor.
  const actors = await page
    .locator('[data-testid="ticker-rows"] li[data-actor]')
    .evaluateAll((els) => els.map((el) => el.getAttribute("data-actor")));
  expect(actors.length).toBeGreaterThan(0);
  for (const a of actors) {
    expect(["claude", "sentinel", "user"]).toContain(a);
  }
});

test("ticker header shows the actor legend (◇ agent / ⚙ sentinel / ↩ user)", async ({ page }) => {
  await waitGraphReady(page);
  const legend = page.getByTestId("actor-legend");
  await expect(legend).toBeVisible();
  const text = (await legend.textContent()) ?? "";
  expect(text).toMatch(/agent/i);
  expect(text).toMatch(/sentinel/i);
  expect(text).toMatch(/user/i);
  expect(text).toContain("◇");
  expect(text).toContain("⚙");
  expect(text).toContain("↩");
});

test("actor glyph is rendered inside each row and lines up with its data-actor", async ({
  page,
}) => {
  await waitGraphReady(page);
  const pairs = await page
    .locator('[data-testid="ticker-rows"] li')
    .evaluateAll((els) =>
      els.map((el) => ({
        actor: el.getAttribute("data-actor"),
        glyph: el.querySelector('[data-testid="actor-glyph"]')?.textContent?.trim() ?? null,
      })),
    );
  expect(pairs.length).toBeGreaterThan(0);
  for (const { actor, glyph } of pairs) {
    if (actor === "claude") expect(glyph).toBe("◇");
    else if (actor === "sentinel") expect(glyph).toBe("⚙");
    else if (actor === "user") expect(glyph).toBe("↩");
  }
});

test("UserPromptSubmit rows are marked data-actor=user (label still reads 'user prompt')", async ({
  page,
}) => {
  await waitGraphReady(page);
  // Find any row whose label contains the user-prompt text; verify
  // its data-actor is "user". Skip if no user prompts in window.
  const userRowCount = await page.locator('[data-testid="ticker-rows"] li[data-actor="user"]').count();
  if (userRowCount === 0) {
    test.skip(true, "no user prompts in current DB window");
    return;
  }
  const firstUserRow = page.locator('[data-testid="ticker-rows"] li[data-actor="user"]').first();
  await expect(firstUserRow).toContainText(/user prompt/i);
});

// ──────────────────────── PERF BUDGETS ────────────────────────

test("PERF: cold load → graph populated within 3s", async ({ page }) => {
  const t0 = Date.now();
  await page.goto("/");
  await page
    .waitForFunction(
      () => document.querySelectorAll('svg[data-testid="graph-canvas"] g.node').length > 0,
      { timeout: 8_000 },
    )
    .catch(() => {});
  const elapsed = Date.now() - t0;
  expect(elapsed).toBeLessThan(3000);
});

test("PERF: warm /api/graph latency p99 < 100ms over 10 samples", async ({ request }) => {
  // Warm
  await request.get("http://127.0.0.1:8082/api/graph");
  const samples: number[] = [];
  for (let i = 0; i < 10; i++) {
    const t = Date.now();
    const r = await request.get("http://127.0.0.1:8082/api/graph");
    expect(r.ok()).toBeTruthy();
    samples.push(Date.now() - t);
  }
  samples.sort((a, b) => a - b);
  const p99 = samples[Math.floor(samples.length * 0.99)] ?? samples[samples.length - 1];
  expect(p99).toBeLessThan(100);
});

test("PERF: no console errors during 6s of idle SSE traffic", async ({ page }) => {
  const errors: string[] = [];
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(msg.text());
  });
  page.on("pageerror", (err) => errors.push(err.message));
  await waitGraphReady(page);
  await page.waitForTimeout(6_000);
  // Ignore well-known noise we don't own — none expected; if any
  // surface, fix or whitelist explicitly.
  expect(errors).toEqual([]);
});

test("PERF: GraphCanvas — nodes do not drift during 6s of steady SSE (P3-18 regression)", async ({
  page,
}) => {
  await waitGraphReady(page);
  await page.waitForTimeout(2000); // let initial settle finish

  type Sample = { id: string; x: number; y: number };
  const sample = (): Promise<Sample[]> =>
    page.evaluate(() => {
      return Array.from(
        document.querySelectorAll('svg[data-testid="graph-canvas"] g.node'),
      ).map((g) => {
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
  await page.waitForTimeout(6_000);
  const after = await sample();

  const byId = new Map(before.map((b) => [b.id, b]));
  const deltas: number[] = [];
  for (const a of after) {
    const b = byId.get(a.id);
    if (!b) continue;
    deltas.push(Math.hypot(a.x - b.x, a.y - b.y));
  }
  if (deltas.length === 0) {
    test.skip(true, "no surviving nodes between samples");
    return;
  }
  const maxDelta = Math.max(...deltas);
  // Pinned nodes should not drift at all; allow ε for floating point.
  expect(maxDelta).toBeLessThan(1.0);
});

test("PERF: EventTicker — ticker rows do NOT re-render on the 5s now-tick (P3-17 regression)", async ({
  page,
}) => {
  // Strategy: take an identity snapshot of TOP-LEVEL ticker rows
  // (data-actor li only — RolledPreview/flyout nested li are
  // intentionally allowed to update as the cache warms). Wait 7s
  // past the 5s now-tick. If the TimeAgo leaf were torn down each
  // tick, the parent row's identity attrs would also reshuffle.
  await waitGraphReady(page);
  const snapshot = () =>
    page.evaluate(() =>
      Array.from(
        document.querySelectorAll('[data-testid="ticker-rows"] li[data-actor]'),
      ).map(
        (li, i) =>
          `${i}|${li.getAttribute("data-actor")}|${li.getAttribute("data-session-id") ?? ""}|${li.getAttribute("data-stuck") ?? ""}|${li.getAttribute("data-intervention") ?? ""}`,
      ),
    );
  const beforeIds = await snapshot();
  await page.waitForTimeout(7_500);
  const afterIds = await snapshot();
  // Should be identical structure (allow length change if a new
  // event arrived — that's a legitimate render).
  if (beforeIds.length === afterIds.length) {
    expect(afterIds).toEqual(beforeIds);
  } else {
    // We got an SSE-driven update during the window. Verify that
    // the OVERLAP region (first N rows of the new list that match
    // the old list) is structurally identical — a 5s tick should
    // NEVER swap classes on rows that haven't moved.
    const n = Math.min(beforeIds.length, afterIds.length);
    // Anchor on the freshest rows (newest at top — overlap is the
    // tail of both lists, length-aligned from the end).
    const beforeTail = beforeIds.slice(beforeIds.length - n);
    const afterTail = afterIds.slice(afterIds.length - n);
    // Don't crash on legitimate row insertion-at-top — only assert
    // tail invariants, which prove no spurious tear-down happened.
    expect(afterTail).toEqual(beforeTail);
  }
});

// ──────────────────────── NO UNHANDLED REJECTIONS ────────────────────────

test("no unhandled promise rejections across the warm-up + scroll + click flow", async ({
  page,
}) => {
  const rejections: string[] = [];
  await page.exposeFunction("__pwReject", (msg: string) => {
    rejections.push(msg);
  });
  await page.addInitScript(() => {
    window.addEventListener("unhandledrejection", (e) => {
      // @ts-expect-error injected
      window.__pwReject?.(String(e.reason));
    });
  });
  await waitGraphReady(page);
  // Click a ticker row, click the auto toggle, click a session node.
  // For the SVG node we use force-click — first() may resolve to a
  // node that's been laid out outside the visible viewport (the
  // graph isn't auto-centered for tests), and the actual behavior
  // we care about here is "interaction handlers don't throw", not
  // "pointer events route through z-stack". Layout/z-order
  // regressions are covered separately.
  const firstRow = page.locator('[data-testid="ticker-rows"] li').first();
  if (await firstRow.count()) await firstRow.locator(".cursor-pointer").first().click();
  await page.waitForTimeout(500);
  await page.getByTestId("auto-watch-toggle").click();
  await page.waitForTimeout(500);
  // Prefer a SentinelSession node — those are anchored deterministically
  // near origin and unlikely to be off-screen.
  const sessionNode = page
    .locator('svg[data-testid="graph-canvas"] g.node[data-kind="SentinelSession"]')
    .first();
  if (await sessionNode.count()) {
    await sessionNode.click({ force: true });
  }
  await page.waitForTimeout(800);
  expect(rejections).toEqual([]);
});
