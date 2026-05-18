import { promises as fs } from "node:fs";
import os from "node:os";
import path from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { JsonlMetricsRepository } from "@/adapters/jsonl-metrics-repository";

const window = {
  start: new Date("2026-01-01T00:00:00Z"),
  end: new Date("2027-01-01T00:00:00Z"),
};

async function setupHome(): Promise<{ home: string; metricsDir: string }> {
  const home = await fs.mkdtemp(path.join(os.tmpdir(), "sen24-"));
  const metricsDir = path.join(home, ".claude", "sentinel", "metrics");
  await fs.mkdir(metricsDir, { recursive: true });
  return { home, metricsDir };
}

describe("JsonlMetricsRepository", () => {
  let home: string;
  let metricsDir: string;

  beforeEach(async () => {
    const setup = await setupHome();
    home = setup.home;
    metricsDir = setup.metricsDir;
  });

  afterEach(async () => {
    await fs.rm(home, { recursive: true, force: true });
  });

  describe("readCycleTimeEvents", () => {
    it("returns [] when cycle-time.jsonl is missing", async () => {
      const repo = new JsonlMetricsRepository(home);
      const got = await repo.readCycleTimeEvents(window);
      expect(got).toEqual([]);
    });

    it("derives hours between adjacent transitions per issue", async () => {
      const rows = [
        {
          issue_id: "FPCRM-1",
          team: "FPCRM",
          from_state: "In Progress",
          to_state: "Code Review",
          timestamp: "2026-05-01T10:00:00Z",
        },
        {
          issue_id: "FPCRM-1",
          team: "FPCRM",
          from_state: "Code Review",
          to_state: "QA Testing",
          timestamp: "2026-05-01T13:00:00Z",
        },
      ];
      await fs.writeFile(
        path.join(metricsDir, "cycle-time.jsonl"),
        rows.map((r) => JSON.stringify(r)).join("\n"),
      );
      const repo = new JsonlMetricsRepository(home);
      const got = await repo.readCycleTimeEvents(window);
      expect(got).toHaveLength(2);
      // First transition for an issue: no prior ts -> hours=0.
      expect(got[0]?.hours).toBe(0);
      expect(got[0]?.from).toBe("In Progress");
      expect(got[0]?.to).toBe("Code Review");
      // Second transition: 3 hours later.
      expect(got[1]?.hours).toBe(3);
      expect(got[1]?.from).toBe("Code Review");
      expect(got[1]?.to).toBe("QA Testing");
    });

    it("drops rows whose stage names aren't in STAGES", async () => {
      const rows = [
        {
          issue_id: "FPCRM-1",
          from_state: "Bogus",
          to_state: "Code Review",
          timestamp: "2026-05-01T10:00:00Z",
        },
        {
          issue_id: "FPCRM-1",
          from_state: "Code Review",
          to_state: "Made-up",
          timestamp: "2026-05-01T11:00:00Z",
        },
      ];
      await fs.writeFile(
        path.join(metricsDir, "cycle-time.jsonl"),
        rows.map((r) => JSON.stringify(r)).join("\n"),
      );
      const repo = new JsonlMetricsRepository(home);
      const got = await repo.readCycleTimeEvents(window);
      expect(got).toEqual([]);
    });

    it("drops rows whose from_state is null", async () => {
      await fs.writeFile(
        path.join(metricsDir, "cycle-time.jsonl"),
        JSON.stringify({
          issue_id: "FPCRM-1",
          to_state: "Code Review",
          timestamp: "2026-05-01T10:00:00Z",
        }),
      );
      const repo = new JsonlMetricsRepository(home);
      const got = await repo.readCycleTimeEvents(window);
      expect(got).toEqual([]);
    });

    it("filters by window end exclusively", async () => {
      const rows = [
        {
          issue_id: "FPCRM-1",
          from_state: "In Progress",
          to_state: "Code Review",
          timestamp: "2025-12-31T23:59:59Z", // before window
        },
        {
          issue_id: "FPCRM-2",
          from_state: "In Progress",
          to_state: "Code Review",
          timestamp: "2026-06-01T10:00:00Z", // inside window
        },
        {
          issue_id: "FPCRM-3",
          from_state: "In Progress",
          to_state: "Code Review",
          timestamp: "2027-01-01T00:00:00Z", // equals end → excluded
        },
      ];
      await fs.writeFile(
        path.join(metricsDir, "cycle-time.jsonl"),
        rows.map((r) => JSON.stringify(r)).join("\n"),
      );
      const repo = new JsonlMetricsRepository(home);
      const got = await repo.readCycleTimeEvents(window);
      expect(got).toHaveLength(1);
      expect(got[0]?.ts.toISOString()).toBe("2026-06-01T10:00:00.000Z");
    });

    it("skips malformed lines", async () => {
      const lines = [
        JSON.stringify({
          issue_id: "FPCRM-1",
          from_state: "In Progress",
          to_state: "Code Review",
          timestamp: "2026-05-01T10:00:00Z",
        }),
        "not-json",
        "{not-balanced",
      ];
      await fs.writeFile(path.join(metricsDir, "cycle-time.jsonl"), lines.join("\n"));
      const repo = new JsonlMetricsRepository(home);
      const got = await repo.readCycleTimeEvents(window);
      expect(got).toHaveLength(1);
    });
  });

  describe("readDeploys", () => {
    it("returns [] when deploys.jsonl is missing", async () => {
      const repo = new JsonlMetricsRepository(home);
      expect(await repo.readDeploys(window)).toEqual([]);
    });

    it("filters by window and maps fields", async () => {
      const rows = [
        {
          timestamp: "2026-05-01T10:00:00Z",
          repo: "sentinel",
          env: "prod",
          commit: "abc",
          duration_s: 30,
        },
        {
          timestamp: "2025-01-01T00:00:00Z", // outside
          repo: "sentinel",
          env: "prod",
          commit: "old",
        },
      ];
      await fs.writeFile(
        path.join(metricsDir, "deploys.jsonl"),
        rows.map((r) => JSON.stringify(r)).join("\n"),
      );
      const repo = new JsonlMetricsRepository(home);
      const got = await repo.readDeploys(window);
      expect(got).toHaveLength(1);
      expect(got[0]).toMatchObject({
        repo: "sentinel",
        env: "prod",
        commit: "abc",
        durationS: 30,
      });
    });
  });

  describe("readTokenUsage", () => {
    it("returns [] when tokens-per-ticket.jsonl is missing", async () => {
      const repo = new JsonlMetricsRepository(home);
      expect(await repo.readTokenUsage(window)).toEqual([]);
    });

    it("maps ticket aggregates to TokenSession[] with synthetic session id", async () => {
      const rows = [
        {
          ticket: "FPCRM-413",
          sessions: 3,
          total_input: 100,
          cache_read: 200,
          cache_creation: 50,
          output: 75,
          cost_usd: 1.23,
          models: { "opus-4-7": 2, "opus-4-6": 1 },
        },
      ];
      await fs.writeFile(
        path.join(metricsDir, "tokens-per-ticket.jsonl"),
        rows.map((r) => JSON.stringify(r)).join("\n"),
      );
      const repo = new JsonlMetricsRepository(home);
      const got = await repo.readTokenUsage(window);
      expect(got).toHaveLength(1);
      expect(got[0]).toMatchObject({
        sessionId: "agg:FPCRM-413",
        totalInput: 100,
        cacheRead: 200,
        cacheCreation: 50,
        output: 75,
        costUsd: 1.23,
        model: "opus-4-7",
      });
      expect(got[0]?.ticketId).toBe("FPCRM-413");
    });

    it("omits ticketId on non-matching ticket strings", async () => {
      await fs.writeFile(
        path.join(metricsDir, "tokens-per-ticket.jsonl"),
        JSON.stringify({
          ticket: "no-prefix",
          sessions: 1,
          total_input: 0,
          cache_read: 0,
          cache_creation: 0,
          output: 0,
          cost_usd: 0,
        }),
      );
      const repo = new JsonlMetricsRepository(home);
      const got = await repo.readTokenUsage(window);
      expect(got).toHaveLength(1);
      expect(got[0]?.ticketId).toBeUndefined();
    });
  });

  describe("readIncidents", () => {
    it("always returns [] (no source yet)", async () => {
      const repo = new JsonlMetricsRepository(home);
      expect(await repo.readIncidents(window)).toEqual([]);
    });
  });
});
