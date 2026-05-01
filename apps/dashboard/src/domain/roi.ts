// SENTINEL-22 — ROI computations.
//
// `ROIRatio` = human cost / claude cost. Anything > 1.0 means Claude is
// cheaper than the human-baseline equivalent. Constants below are Linear-
// internal benchmarks for Firefly-Pro / Centurion engineering teams; if
// SEN-15 lands a canonical source these can be sourced from there, but
// for now they're hardcoded so SEN-22 doesn't block on SEN-15 ordering.

import { makeDollars, type Dollars } from "./dollars";

export type ROIRatio = number & { readonly __brand: "ROIRatio" };

/** Internal human-baseline cost per story point, USD. */
export const HUMAN_USD_PER_POINT = 327;

/** Internal human-baseline cost per engineering day, USD. */
export const HUMAN_USD_PER_DAY = 654;

/**
 * Compute the ROI ratio of a human-baseline cost over a Claude cost.
 *
 * `claudeCost == 0` is treated as `Infinity` — there is no finite ratio
 * for "Claude was free". Negative inputs throw — those are programming
 * errors, not legitimate data.
 */
export function compute(humanCost: Dollars, claudeCost: Dollars): ROIRatio {
  if (!Number.isFinite(humanCost) || !Number.isFinite(claudeCost)) {
    throw new TypeError(
      `compute requires finite Dollars, got humanCost=${humanCost}, claudeCost=${claudeCost}`,
    );
  }
  if (humanCost < 0 || claudeCost < 0) {
    throw new RangeError(
      `compute requires non-negative Dollars, got humanCost=${humanCost}, claudeCost=${claudeCost}`,
    );
  }
  if (claudeCost === 0) {
    return Infinity as ROIRatio;
  }
  return (humanCost / claudeCost) as ROIRatio;
}

/** Convenience: ROI for a story-point estimate using the standard human rate. */
export function roiForPoints(points: number, claudeCost: Dollars): ROIRatio {
  if (!Number.isFinite(points) || points < 0) {
    throw new RangeError(`roiForPoints requires points >= 0, got ${points}`);
  }
  return compute(makeDollars(points * HUMAN_USD_PER_POINT), claudeCost);
}

/** Convenience: ROI for an N-day human effort vs. a Claude run cost. */
export function roiForDays(days: number, claudeCost: Dollars): ROIRatio {
  if (!Number.isFinite(days) || days < 0) {
    throw new RangeError(`roiForDays requires days >= 0, got ${days}`);
  }
  return compute(makeDollars(days * HUMAN_USD_PER_DAY), claudeCost);
}
