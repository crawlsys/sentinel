// SENTINEL-22 — Ticket value type.
//
// Branded primitives for ticket identifier (`PREFIX-NUMBER`) and team
// name. The full Ticket aggregate ties together identifier, team,
// priority, optional estimate, current stage, and timestamps.

import type { StoryPoint } from "./story-point";
import type { Stage } from "./cycle-time";

/** A Linear-style ticket id, e.g. `FPCRM-123`, `SEN-22`. */
export type TicketIdentifier = string & { readonly __brand: "TicketIdentifier" };

/** A team identifier (string slug). */
export type Team = string & { readonly __brand: "Team" };

export type Priority = "urgent" | "high" | "medium" | "low";

export const PRIORITIES: readonly Priority[] = ["urgent", "high", "medium", "low"];

const TICKET_ID_RE = /^[A-Z]+-\d+$/;
const TICKET_ID_RE_DESC = "PREFIX-NUMBER (uppercase prefix, e.g. SEN-22)";

/**
 * Build a validated TicketIdentifier. Throws on values that don't match
 * `^[A-Z]+-\d+$` — i.e. lowercase prefix, missing dash, missing digits,
 * or any other shape Linear wouldn't issue.
 */
export function makeTicketIdentifier(value: string): TicketIdentifier {
  if (typeof value !== "string" || !TICKET_ID_RE.test(value)) {
    throw new RangeError(
      `TicketIdentifier must match ${TICKET_ID_RE_DESC}, got ${JSON.stringify(value)}`,
    );
  }
  return value as TicketIdentifier;
}

/** Build a Team. Throws on empty / whitespace-only input. */
export function makeTeam(value: string): Team {
  if (typeof value !== "string" || !value.trim()) {
    throw new RangeError(
      `Team must be a non-empty string, got ${JSON.stringify(value)}`,
    );
  }
  return value as Team;
}

/** Validate a Priority value. Useful when reading from untyped JSON. */
export function makePriority(value: string): Priority {
  if ((PRIORITIES as readonly string[]).includes(value)) {
    return value as Priority;
  }
  throw new RangeError(
    `Priority must be one of ${PRIORITIES.join(", ")}, got ${JSON.stringify(value)}`,
  );
}

/** Full ticket aggregate. */
export interface Ticket {
  readonly id: TicketIdentifier;
  readonly team: Team;
  readonly priority: Priority;
  readonly estimate: StoryPoint | null;
  readonly state: Stage;
  readonly created_at: Date;
  readonly completed_at: Date | null;
}
