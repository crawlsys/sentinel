// SENTINEL-22 — DORA metrics domain types.
//
// Targets follow Google's published DORA tier definitions
// (https://dora.dev). All four metrics use a `low | medium | high | elite`
// classifier with hard numeric boundaries — no fuzzy bands, no rolling
// windows. Fed by the dashboard's metrics pipeline.

import type { Hours } from "./cycle-time";

export type LeadTime = Hours;

/** Deploy frequency expressed as deploys per day. */
export type DeployFrequency = number & { readonly __brand: "DeployFrequency" };

/** Change failure rate in [0, 1]. */
export type ChangeFailureRate = number & { readonly __brand: "ChangeFailureRate" };

/** Mean time to recover, in hours. */
export type MTTR = Hours;

export type DoraTier = "elite" | "high" | "medium" | "low";

export type DoraMetric = "lead_time" | "deploy_freq" | "change_failure_rate" | "mttr";

/** All DORA tiers in best-to-worst order. */
export const DORA_TIERS: readonly DoraTier[] = ["elite", "high", "medium", "low"];

/** All supported DORA metric ids. */
export const DORA_METRICS: readonly DoraMetric[] = [
  "lead_time",
  "deploy_freq",
  "change_failure_rate",
  "mttr",
];

export function makeDeployFrequency(value: number): DeployFrequency {
  if (!Number.isFinite(value)) {
    throw new TypeError(`DeployFrequency must be finite, got ${value}`);
  }
  if (value < 0) {
    throw new RangeError(`DeployFrequency must be >= 0, got ${value}`);
  }
  return value as DeployFrequency;
}

export function makeChangeFailureRate(value: number): ChangeFailureRate {
  if (!Number.isFinite(value)) {
    throw new TypeError(`ChangeFailureRate must be finite, got ${value}`);
  }
  if (value < 0 || value > 1) {
    throw new RangeError(`ChangeFailureRate must be in [0, 1], got ${value}`);
  }
  return value as ChangeFailureRate;
}

/**
 * Classify a metric value into a DORA tier.
 *
 * Boundaries (mirrors the spec in the SEN-22 ticket):
 *   - lead_time (hours): elite < 24, high < 168, medium < 720, low > 720
 *   - deploy_freq (per day): elite > 1, high 1/day–1/wk (>= 1/7), medium >= 1/30, low < 1/30
 *   - change_failure_rate (0-1): elite <= 0.15, high <= 0.30, medium <= 0.45, low > 0.45
 *   - mttr (hours): elite < 1, high < 24, medium < 168, low > 168
 *
 * Throws on unknown metric ids — caller bug, not a runtime data issue.
 */
export function tierFor(metric: DoraMetric, value: number): DoraTier {
  if (!Number.isFinite(value)) {
    throw new TypeError(`tierFor(${metric}) requires a finite number, got ${value}`);
  }
  switch (metric) {
    case "lead_time":
      if (value < 24) return "elite";
      if (value < 168) return "high";
      if (value < 720) return "medium";
      return "low";
    case "deploy_freq":
      if (value > 1) return "elite";
      if (value >= 1 / 7) return "high";
      if (value >= 1 / 30) return "medium";
      return "low";
    case "change_failure_rate":
      if (value < 0 || value > 1) {
        throw new RangeError(
          `change_failure_rate must be in [0, 1], got ${value}`,
        );
      }
      if (value <= 0.15) return "elite";
      if (value <= 0.3) return "high";
      if (value <= 0.45) return "medium";
      return "low";
    case "mttr":
      if (value < 1) return "elite";
      if (value < 24) return "high";
      if (value < 168) return "medium";
      return "low";
  }
}
