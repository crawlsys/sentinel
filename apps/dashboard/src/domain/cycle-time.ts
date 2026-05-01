// SENTINEL-22 — Cycle time domain types.
//
// Pure TS, zero IO. Mirrors the Linear pipeline used by Firefly Pro and any
// project that adopts the same convention:
// Backlog -> Todo -> In Progress -> Code Review -> QA Testing -> QA Failed -> Completed.

export type Hours = number & { readonly __brand: "Hours" };

/** A duration in hours, branded so it can't be mixed with raw numbers. */
export type CycleTime = Hours;

/** Linear workflow states tracked by the dashboard. */
export type Stage =
  | "Backlog"
  | "Todo"
  | "In Progress"
  | "Code Review"
  | "QA Testing"
  | "QA Failed"
  | "Completed";

/** A single transition between two workflow states. */
export interface StageTransition {
  readonly from: Stage;
  readonly to: Stage;
  readonly ts: Date;
  readonly hours: Hours;
}

/** Coarse cycle-time bucket for histogram-style reporting. */
export type TimeBucket = "< 1h" | "1-4h" | "4-24h" | "1-3d" | "> 3d";

/**
 * Construct a `Hours` value. Throws on negative or non-finite input — cycle
 * time is always >= 0 by definition.
 */
export function makeHours(value: number): Hours {
  if (!Number.isFinite(value)) {
    throw new TypeError(`Hours must be finite, got ${value}`);
  }
  if (value < 0) {
    throw new RangeError(`Hours must be >= 0, got ${value}`);
  }
  return value as Hours;
}

/**
 * Bucket a duration in hours into one of five canonical TimeBucket values.
 *
 * Boundaries are right-exclusive: 1h goes into `1-4h`, 4h into `4-24h`,
 * 24h into `1-3d`, 72h into `> 3d`.
 */
export function bucketize(hours: number): TimeBucket {
  if (!Number.isFinite(hours) || hours < 0) {
    throw new RangeError(`bucketize requires a non-negative finite number, got ${hours}`);
  }
  if (hours < 1) return "< 1h";
  if (hours < 4) return "1-4h";
  if (hours < 24) return "4-24h";
  if (hours < 72) return "1-3d";
  return "> 3d";
}

/** All Stage values in their canonical pipeline order. */
export const STAGES: readonly Stage[] = [
  "Backlog",
  "Todo",
  "In Progress",
  "Code Review",
  "QA Testing",
  "QA Failed",
  "Completed",
];

/** All TimeBucket values in ascending duration order. */
export const TIME_BUCKETS: readonly TimeBucket[] = [
  "< 1h",
  "1-4h",
  "4-24h",
  "1-3d",
  "> 3d",
];
