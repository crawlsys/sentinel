import { describe, it, expect } from "vitest";

import { GetWipByStage } from "@/application/get-wip-by-stage";
import { makeTeam, makeTicketIdentifier, STAGES, type Ticket } from "@/domain";
import type { Clock, LinearGateway, TimeRange } from "@/ports";

const FIXED_NOW = new Date("2026-05-18T12:00:00Z");
const fixedClock: Clock = { now: () => FIXED_NOW };

function makeTicket(id: string, team: string, state: Ticket["state"]): Ticket {
  return {
    id: makeTicketIdentifier(id),
    team: makeTeam(team),
    priority: "medium",
    estimate: null,
    state,
    created_at: new Date(0),
    completed_at: null,
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

describe("GetWipByStage", () => {
  it("returns an empty by_team map when no tickets exist", async () => {
    const handler = new GetWipByStage(fakeGateway([]), fixedClock);
    const snap = await handler.run();
    expect(snap.ts).toEqual(FIXED_NOW);
    expect(snap.by_team).toEqual({});
  });

  it("groups counts by (team, stage)", async () => {
    const tickets = [
      makeTicket("FPCRM-1", "FPCRM", "Code Review"),
      makeTicket("FPCRM-2", "FPCRM", "Code Review"),
      makeTicket("FPCRM-3", "FPCRM", "In Progress"),
      makeTicket("SEN-1", "SEN", "In Progress"),
    ];
    const handler = new GetWipByStage(fakeGateway(tickets), fixedClock);
    const snap = await handler.run();
    expect(snap.by_team["FPCRM"]?.["Code Review"]).toBe(2);
    expect(snap.by_team["FPCRM"]?.["In Progress"]).toBe(1);
    expect(snap.by_team["SEN"]?.["In Progress"]).toBe(1);
    // Stages not used still present as 0 (emptyWipByStage seeds them).
    for (const stage of STAGES) {
      expect(typeof snap.by_team["FPCRM"]?.[stage]).toBe("number");
    }
  });

  it("uses clock.now() for the snapshot timestamp", async () => {
    const t = new Date("2030-01-01T00:00:00Z");
    const handler = new GetWipByStage(fakeGateway([]), { now: () => t });
    const snap = await handler.run();
    expect(snap.ts).toEqual(t);
  });
});
