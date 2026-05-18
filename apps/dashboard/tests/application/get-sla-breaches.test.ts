import { describe, it, expect } from "vitest";

import { GetSLABreaches } from "@/application/get-sla-breaches";
import {
  makeSLA,
  makeTeam,
  makeTicketIdentifier,
  type SLA,
  type Ticket,
} from "@/domain";
import type { Clock, LinearGateway, TimeRange } from "@/ports";

const FIXED_NOW = new Date("2026-05-18T12:00:00Z");

function makeTicket(args: {
  id: string;
  team: string;
  priority?: Ticket["priority"];
  state?: Ticket["state"];
  ageHours: number;
}): Ticket {
  return {
    id: makeTicketIdentifier(args.id),
    team: makeTeam(args.team),
    priority: args.priority ?? "medium",
    estimate: null,
    state: args.state ?? "Code Review",
    created_at: new Date(FIXED_NOW.getTime() - args.ageHours * 3600 * 1000),
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

const fixedClock: Clock = { now: () => FIXED_NOW };

describe("GetSLABreaches", () => {
  it("emits a breach when predicate matches AND elapsed > target", async () => {
    const tickets = [makeTicket({ id: "FPCRM-1", team: "FPCRM", ageHours: 48 })];
    const sla: SLA = makeSLA({
      id: "review-24h",
      name: "Code Review within 24h",
      target_hours: 24,
      predicate: (ctx) => ctx.stage === "Code Review",
    });
    const handler = new GetSLABreaches(fakeGateway(tickets), fixedClock);
    const breaches = await handler.run([sla]);
    expect(breaches).toHaveLength(1);
    expect(breaches[0]).toMatchObject({
      sla_id: "review-24h",
      ticket_id: "FPCRM-1",
      elapsed_hours: 48,
    });
    expect(breaches[0]?.breached_at).toEqual(FIXED_NOW);
  });

  it("skips a ticket whose predicate is false", async () => {
    const tickets = [
      makeTicket({
        id: "FPCRM-1",
        team: "FPCRM",
        ageHours: 100,
        state: "Backlog",
      }),
    ];
    const sla = makeSLA({
      id: "review-24h",
      name: "Code Review within 24h",
      target_hours: 24,
      predicate: (ctx) => ctx.stage === "Code Review",
    });
    const handler = new GetSLABreaches(fakeGateway(tickets), fixedClock);
    expect(await handler.run([sla])).toEqual([]);
  });

  it("skips a ticket whose elapsed is within target", async () => {
    const tickets = [
      makeTicket({ id: "FPCRM-1", team: "FPCRM", ageHours: 12 }),
    ];
    const sla = makeSLA({
      id: "review-24h",
      name: "Code Review within 24h",
      target_hours: 24,
      predicate: (ctx) => ctx.stage === "Code Review",
    });
    const handler = new GetSLABreaches(fakeGateway(tickets), fixedClock);
    expect(await handler.run([sla])).toEqual([]);
  });

  it("emits one breach per matching (ticket, sla) pair", async () => {
    const tickets = [
      makeTicket({ id: "FPCRM-1", team: "FPCRM", ageHours: 100 }),
      makeTicket({ id: "FPCRM-2", team: "FPCRM", ageHours: 50 }),
    ];
    const slas = [
      makeSLA({
        id: "a",
        name: "A",
        target_hours: 24,
        predicate: () => true,
      }),
      makeSLA({
        id: "b",
        name: "B",
        target_hours: 48,
        predicate: () => true,
      }),
    ];
    const handler = new GetSLABreaches(fakeGateway(tickets), fixedClock);
    const breaches = await handler.run(slas);
    // FPCRM-1 (100h) breaches both; FPCRM-2 (50h) breaches a (>24) + b (>48)
    expect(breaches).toHaveLength(4);
  });

  it("returns [] when no tickets are active", async () => {
    const handler = new GetSLABreaches(fakeGateway([]), fixedClock);
    expect(
      await handler.run([
        makeSLA({
          id: "x",
          name: "x",
          target_hours: 1,
          predicate: () => true,
        }),
      ]),
    ).toEqual([]);
  });
});
