// SENTINEL-22 — SLA value types.
//
// Pure data — no engine logic. The actual SLA evaluator lives in the
// application layer (out of scope for SEN-22). This module just defines
// the shapes so other layers can talk about SLAs without pulling in any
// of the evaluator's IO dependencies.

import type { Priority, TicketIdentifier } from "./ticket";
import type { Stage } from "./cycle-time";

/**
 * Context object passed to a SLA's `predicate`. Optional fields let a
 * SLA scope itself to a subset of tickets (e.g. only `urgent` priority,
 * only tickets currently in `Code Review`).
 */
export interface SLAContext {
  readonly ticket_id: TicketIdentifier;
  readonly priority: Priority;
  readonly stage: Stage;
  readonly age_hours: number;
  readonly elapsed_in_stage_hours: number;
}

/**
 * A single Service-Level Agreement. The `predicate` is a pure function
 * over `SLAContext`; the engine evaluates it per ticket per tick and
 * fires a breach when `elapsed_in_stage_hours > target_hours`.
 */
export interface SLA {
  readonly id: string;
  readonly name: string;
  readonly target_hours: number;
  readonly predicate: (ctx: SLAContext) => boolean;
}

/** A recorded breach of one SLA by one ticket. */
export interface SLABreach {
  readonly sla_id: string;
  readonly ticket_id: TicketIdentifier;
  readonly breached_at: Date;
  readonly elapsed_hours: number;
}

/**
 * Construct an SLA. Lightweight validation: id and name non-empty,
 * target_hours positive and finite. Predicate identity is preserved.
 */
export function makeSLA(spec: {
  id: string;
  name: string;
  target_hours: number;
  predicate: (ctx: SLAContext) => boolean;
}): SLA {
  if (!spec.id.trim()) {
    throw new RangeError("SLA id must be non-empty");
  }
  if (!spec.name.trim()) {
    throw new RangeError("SLA name must be non-empty");
  }
  if (!Number.isFinite(spec.target_hours) || spec.target_hours <= 0) {
    throw new RangeError(
      `SLA target_hours must be > 0 and finite, got ${spec.target_hours}`,
    );
  }
  return Object.freeze({
    id: spec.id,
    name: spec.name,
    target_hours: spec.target_hours,
    predicate: spec.predicate,
  });
}
