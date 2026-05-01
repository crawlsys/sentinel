// SENTINEL-22 — Work-in-progress (WIP) snapshot types.
//
// A `WipSnapshot` is a single point-in-time view of how many tickets are
// sitting in each stage, partitioned by team. The `bottleneckStage`
// helper finds the stage with the highest WIP-to-throughput ratio across
// all teams — that's the pipeline's current chokepoint.

import { STAGES, type Stage } from "./cycle-time";

/** Count of tickets per stage, for a single team. */
export type WipByStage = Record<Stage, number>;

/** Snapshot of WIP across teams at a moment in time. */
export interface WipSnapshot {
  readonly ts: Date;
  readonly by_team: Record<string, WipByStage>;
}

/** Throughput estimate (tickets/day) per stage, used to score WIP load. */
export type StageThroughput = Partial<Record<Stage, number>>;

/** Build a fully-populated WipByStage with zeros for every stage. */
export function emptyWipByStage(): WipByStage {
  const out = {} as Record<Stage, number>;
  for (const s of STAGES) {
    out[s] = 0;
  }
  return out;
}

/**
 * Identify the bottleneck stage in a snapshot.
 *
 * Method: sum WIP across all teams per stage, then divide by the
 * stage's throughput (tickets/day). The stage with the highest WIP-days
 * outstanding wins. Ties are broken by pipeline order (earlier stage
 * wins — fixing upstream first usually drains downstream too).
 *
 * Returns `null` when:
 *   - the snapshot has no teams,
 *   - every stage's WIP is zero,
 *   - the highest WIP stage has no throughput record (can't score it).
 */
export function bottleneckStage(
  snapshot: WipSnapshot,
  throughput: StageThroughput,
): Stage | null {
  const totals = emptyWipByStage();
  for (const team of Object.values(snapshot.by_team)) {
    for (const s of STAGES) {
      totals[s] += team[s] ?? 0;
    }
  }

  let best: Stage | null = null;
  let bestScore = -1;
  for (const s of STAGES) {
    const wip = totals[s];
    if (wip <= 0) continue;
    const tput = throughput[s];
    if (tput === undefined || tput <= 0) continue;
    const score = wip / tput;
    if (score > bestScore) {
      best = s;
      bestScore = score;
    }
  }
  return best;
}
