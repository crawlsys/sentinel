import { describe, it, expect } from "vitest";

import { makeSLA, type SLAContext } from "@/domain/sla";
import { makeTicketIdentifier } from "@/domain/ticket";

describe("makeSLA", () => {
  const baseCtx: SLAContext = {
    ticket_id: makeTicketIdentifier("SEN-22"),
    priority: "urgent",
    stage: "Code Review",
    age_hours: 10,
    elapsed_in_stage_hours: 4,
  };

  it("constructs a frozen SLA with the given fields", () => {
    const sla = makeSLA({
      id: "review-urgent-4h",
      name: "Urgent Code Review",
      target_hours: 4,
      predicate: (c) => c.priority === "urgent" && c.stage === "Code Review",
    });
    expect(sla.id).toBe("review-urgent-4h");
    expect(sla.target_hours).toBe(4);
    expect(sla.predicate(baseCtx)).toBe(true);
  });

  it("rejects empty id / name", () => {
    expect(() =>
      makeSLA({ id: "", name: "x", target_hours: 1, predicate: () => true }),
    ).toThrow(RangeError);
    expect(() =>
      makeSLA({ id: "x", name: "  ", target_hours: 1, predicate: () => true }),
    ).toThrow(RangeError);
  });

  it("rejects non-positive or non-finite target_hours", () => {
    expect(() =>
      makeSLA({ id: "x", name: "x", target_hours: 0, predicate: () => true }),
    ).toThrow(RangeError);
    expect(() =>
      makeSLA({ id: "x", name: "x", target_hours: -1, predicate: () => true }),
    ).toThrow(RangeError);
    expect(() =>
      makeSLA({ id: "x", name: "x", target_hours: Number.NaN, predicate: () => true }),
    ).toThrow(RangeError);
  });
});
