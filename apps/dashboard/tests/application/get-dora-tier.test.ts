import { describe, it, expect } from "vitest";

import { GetDoraTier } from "@/application/get-dora-tier";
import { makeHours, type StageTransition } from "@/domain";
import type {
  DeployEvent,
  Incident,
  MetricsRepository,
  TimeRange,
  TokenSession,
} from "@/ports";
import { makeTicketIdentifier } from "@/domain";

const oneDay = 24 * 60 * 60 * 1000;
const window: TimeRange = {
  start: new Date("2026-05-01T00:00:00Z"),
  end: new Date("2026-05-31T00:00:00Z"), // 30 days
};

function fakeRepo(overrides: Partial<MetricsRepository>): MetricsRepository {
  return {
    readCycleTimeEvents: async () => [],
    readDeploys: async () => [],
    readTokenUsage: async () => [],
    readIncidents: async () => [],
    ...overrides,
  };
}

describe("GetDoraTier", () => {
  it("returns elite-class metrics on an empty data set (degenerate path)", async () => {
    const handler = new GetDoraTier(fakeRepo({}));
    const result = await handler.run(window);
    expect(result.raw).toEqual({
      leadTimeHours: 0,
      deployFreqPerDay: 0,
      cfr: 0,
      mttrHours: 0,
    });
    // 0 lead time → elite per tierFor; 0 deploy_freq → low; 0 CFR → elite;
    // 0 MTTR → elite. These are documented degenerate-data outputs.
    expect(result.tiers.lead_time).toBe("elite");
    expect(result.tiers.deploy_freq).toBe("low");
    expect(result.tiers.change_failure_rate).toBe("elite");
    expect(result.tiers.mttr).toBe("elite");
  });

  it("averages lead-time across transitions into Completed", async () => {
    const transitions: StageTransition[] = [
      {
        from: "QA Testing",
        to: "Completed",
        ts: new Date("2026-05-15T00:00:00Z"),
        hours: makeHours(2),
      },
      {
        from: "QA Testing",
        to: "Completed",
        ts: new Date("2026-05-16T00:00:00Z"),
        hours: makeHours(6),
      },
      {
        from: "In Progress",
        to: "Code Review",
        ts: new Date("2026-05-17T00:00:00Z"),
        hours: makeHours(99),
      },
    ];
    const handler = new GetDoraTier(
      fakeRepo({ readCycleTimeEvents: async () => transitions }),
    );
    const result = await handler.run(window);
    expect(result.raw.leadTimeHours).toBe(4); // (2 + 6) / 2
    expect(result.tiers.lead_time).toBe("elite"); // 4 < 24
  });

  it("computes deploy_freq across the window day-count", async () => {
    // 30 deploys over a 30-day window = 1.0/day → high (>= 1/7).
    // 31 deploys would be > 1 → elite. We use 30 to land exactly on the
    // high/elite boundary (deploy_freq > 1 is the elite cut).
    const deploys: DeployEvent[] = Array.from({ length: 30 }, (_, i) => ({
      timestamp: new Date(window.start.getTime() + i * oneDay),
      repo: "x",
      env: "prod",
      commit: `c${i}`,
      durationS: 1,
    }));
    const handler = new GetDoraTier(
      fakeRepo({ readDeploys: async () => deploys }),
    );
    const result = await handler.run(window);
    expect(result.raw.deployFreqPerDay).toBeCloseTo(1.0, 6);
    expect(result.tiers.deploy_freq).toBe("high");
  });

  it("CFR with zero deploys returns 0 (no false-positive failures)", async () => {
    const incident: Incident = {
      ticketId: makeTicketIdentifier("FPCRM-1"),
      createdAt: new Date("2026-05-10T00:00:00Z"),
      severity: "sev2",
    };
    const handler = new GetDoraTier(
      fakeRepo({ readIncidents: async () => [incident] }),
    );
    const result = await handler.run(window);
    expect(result.raw.cfr).toBe(0);
    expect(result.tiers.change_failure_rate).toBe("elite");
  });

  it("MTTR excludes incidents missing completedAt; averages the rest", async () => {
    const incidents: Incident[] = [
      {
        ticketId: makeTicketIdentifier("FPCRM-1"),
        createdAt: new Date("2026-05-10T00:00:00Z"),
        completedAt: new Date("2026-05-10T02:00:00Z"), // 2h
        severity: "sev2",
      },
      {
        ticketId: makeTicketIdentifier("FPCRM-2"),
        createdAt: new Date("2026-05-12T00:00:00Z"),
        completedAt: new Date("2026-05-12T04:00:00Z"), // 4h
        severity: "sev2",
      },
      {
        ticketId: makeTicketIdentifier("FPCRM-3"),
        createdAt: new Date("2026-05-15T00:00:00Z"),
        // unrecovered — excluded from MTTR average
        severity: "sev3",
      },
    ];
    const handler = new GetDoraTier(
      fakeRepo({ readIncidents: async () => incidents }),
    );
    const result = await handler.run(window);
    expect(result.raw.mttrHours).toBe(3); // (2 + 4) / 2
    expect(result.tiers.mttr).toBe("high"); // < 24
  });

  it("readTokenUsage is not consulted by GetDoraTier", async () => {
    // Regression guard: if a future refactor accidentally pulled token data
    // into the DORA calc, this test would catch it.
    let tokenCalls = 0;
    const handler = new GetDoraTier(
      fakeRepo({
        readTokenUsage: async () => {
          tokenCalls += 1;
          return [] as TokenSession[];
        },
      }),
    );
    await handler.run(window);
    expect(tokenCalls).toBe(0);
  });
});
