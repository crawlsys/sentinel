// SENTINEL-23 — Metrics repository port.
//
// Read-only access to historical metrics data (cycle-time events, deploys,
// token usage, incidents) over a time window. Adapters implement this
// against whatever backing store the deployment uses (sentinel state
// store, JSONL files, postgres, etc.).
//
// Pure interface declarations only — zero implementations, zero IO.

import type { StageTransition } from "../domain";
import type { DeployEvent } from "./deploy-event-stream";
import type { TokenSession, Incident } from "./types";
import type { TimeRange } from "./time-range";

export interface MetricsRepository {
  readCycleTimeEvents(window: TimeRange): Promise<StageTransition[]>;
  readDeploys(window: TimeRange): Promise<DeployEvent[]>;
  readTokenUsage(window: TimeRange): Promise<TokenSession[]>;
  readIncidents(window: TimeRange): Promise<Incident[]>;
}
