import { describe, it, expect } from "vitest";

import {
  PRIORITIES,
  makePriority,
  makeTeam,
  makeTicketIdentifier,
} from "@/domain/ticket";

describe("makeTicketIdentifier", () => {
  it("accepts well-formed Linear ids", () => {
    expect(makeTicketIdentifier("SEN-22")).toBe("SEN-22");
    expect(makeTicketIdentifier("FPCRM-1234")).toBe("FPCRM-1234");
    expect(makeTicketIdentifier("A-1")).toBe("A-1");
  });

  it("rejects lowercase prefix", () => {
    expect(() => makeTicketIdentifier("lowercase-22")).toThrow(RangeError);
    expect(() => makeTicketIdentifier("Sen-22")).toThrow(RangeError);
  });

  it("rejects missing dash", () => {
    expect(() => makeTicketIdentifier("SEN22")).toThrow(RangeError);
  });

  it("rejects non-numeric tail", () => {
    expect(() => makeTicketIdentifier("SEN-abc")).toThrow(RangeError);
  });

  it("rejects empty / whitespace", () => {
    expect(() => makeTicketIdentifier("")).toThrow(RangeError);
    expect(() => makeTicketIdentifier(" SEN-22")).toThrow(RangeError);
  });

  it("rejects non-string at runtime", () => {
    expect(() => makeTicketIdentifier(123 as unknown as string)).toThrow(RangeError);
  });
});

describe("makePriority", () => {
  it("accepts the four canonical priorities", () => {
    for (const p of PRIORITIES) {
      expect(makePriority(p)).toBe(p);
    }
  });

  it("rejects unknown values", () => {
    expect(() => makePriority("URGENT")).toThrow(RangeError);
    expect(() => makePriority("")).toThrow(RangeError);
    expect(() => makePriority("critical")).toThrow(RangeError);
  });
});

describe("makeTeam", () => {
  it("accepts non-empty strings", () => {
    expect(makeTeam("frontend")).toBe("frontend");
    expect(makeTeam("ops-team")).toBe("ops-team");
  });

  it("rejects empty / whitespace", () => {
    expect(() => makeTeam("")).toThrow(RangeError);
    expect(() => makeTeam("   ")).toThrow(RangeError);
  });
});
