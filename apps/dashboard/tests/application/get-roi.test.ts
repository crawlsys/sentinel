import { describe, it, expect } from "vitest";

import { GetROI } from "@/application/get-roi";
import {
  HUMAN_USD_PER_POINT,
  HUMAN_USD_PER_DAY,
  makeTeam,
  makeTicketIdentifier,
  type StoryPoint,
  type Ticket,
} from "@/domain";
import type {
  LinearGateway,
  MetricsRepository,
  TimeRange,
  TokenSession,
} from "@/ports";

const window: TimeRange = {
  start: new Date("2026-05-01T00:00:00Z"),
  end: new Date("2026-05-31T00:00:00Z"), // 30 days
};

function fakeRepo(sessions: TokenSession[]): MetricsRepository {
  return {
    readCycleTimeEvents: async () => [],
    readDeploys: async () => [],
    readTokenUsage: async () => sessions,
    readIncidents: async () => [],
  };
}

function fakeGateway(tickets: Ticket[]): LinearGateway {
  return {
    async getActiveTickets() {
      return tickets;
    },
    async getCompletedTickets(_w: TimeRange) {
      return [];
    },
    async getTeams() {
      return [];
    },
  };
}

describe("GetROI", () => {
  it("uses story-points basis when at least one tokened ticket has an estimate", async () => {
    const sessions: TokenSession[] = [
      {
        ticketId: makeTicketIdentifier("FPCRM-1"),
        sessionId: "agg:FPCRM-1",
        totalInput: 0,
        cacheRead: 0,
        cacheCreation: 0,
        output: 0,
        costUsd: 100,
        model: "opus-4-7",
      },
    ];
    const tickets: Ticket[] = [
      {
        id: makeTicketIdentifier("FPCRM-1"),
        team: makeTeam("FPCRM"),
        priority: "medium",
        estimate: 5 as StoryPoint,
        state: "Code Review",
        created_at: new Date(0),
        completed_at: null,
      },
    ];
    const handler = new GetROI(fakeRepo(sessions), fakeGateway(tickets));
    const result = await handler.run(window);
    expect(result.basis).toBe("story_points");
    expect(result.humanCostUsd).toBe(5 * HUMAN_USD_PER_POINT);
    expect(result.claudeCostUsd).toBe(100);
    expect(result.ratio).toBeCloseTo((5 * HUMAN_USD_PER_POINT) / 100, 6);
  });

  it("falls back to days basis when no estimate match is available", async () => {
    const sessions: TokenSession[] = [
      {
        // ticketId omitted on purpose → cannot match estimate by id
        sessionId: "agg:unknown",
        totalInput: 0,
        cacheRead: 0,
        cacheCreation: 0,
        output: 0,
        costUsd: 100,
        model: "opus-4-7",
      },
    ];
    const handler = new GetROI(fakeRepo(sessions), fakeGateway([]));
    const result = await handler.run(window);
    expect(result.basis).toBe("days_fallback");
    expect(result.claudeCostUsd).toBe(100);
    // 30 days × $654/day human baseline, ratio = 30*654 / 100
    expect(result.humanCostUsd).toBeCloseTo(30 * HUMAN_USD_PER_DAY, 6);
    expect(result.ratio).toBeCloseTo((30 * HUMAN_USD_PER_DAY) / 100, 6);
  });

  it("zero claude cost yields Infinity ratio", async () => {
    const sessions: TokenSession[] = [
      {
        ticketId: makeTicketIdentifier("FPCRM-1"),
        sessionId: "agg:FPCRM-1",
        totalInput: 0,
        cacheRead: 0,
        cacheCreation: 0,
        output: 0,
        costUsd: 0,
        model: "opus-4-7",
      },
    ];
    const tickets: Ticket[] = [
      {
        id: makeTicketIdentifier("FPCRM-1"),
        team: makeTeam("FPCRM"),
        priority: "medium",
        estimate: 3 as StoryPoint,
        state: "Code Review",
        created_at: new Date(0),
        completed_at: null,
      },
    ];
    const handler = new GetROI(fakeRepo(sessions), fakeGateway(tickets));
    const result = await handler.run(window);
    expect(result.basis).toBe("story_points");
    expect(result.ratio).toBe(Infinity);
  });
});
