// SENTINEL-25 — GetDoraTier use case.
//
// Composes MetricsRepository reads with domain.tierFor() to produce the
// four DORA classifications plus the raw values they came from. Pure
// composition — every IO call delegates to a port.

import { tierFor, type DoraTier } from "../domain";
import type { MetricsRepository, TimeRange } from "../ports";

export interface GetDoraTierResult {
  readonly tiers: {
    readonly lead_time: DoraTier;
    readonly deploy_freq: DoraTier;
    readonly change_failure_rate: DoraTier;
    readonly mttr: DoraTier;
  };
  readonly raw: {
    readonly leadTimeHours: number;
    readonly deployFreqPerDay: number;
    readonly cfr: number;
    readonly mttrHours: number;
  };
}

const MS_PER_HOUR = 1000 * 60 * 60;
const MS_PER_DAY = MS_PER_HOUR * 24;

export class GetDoraTier {
  constructor(private readonly repo: MetricsRepository) {}

  async run(window: TimeRange): Promise<GetDoraTierResult> {
    const [transitions, deploys, incidents] = await Promise.all([
      this.repo.readCycleTimeEvents(window),
      this.repo.readDeploys(window),
      this.repo.readIncidents(window),
    ]);

    // Lead time: mean StageTransition.hours over transitions into Completed.
    const intoCompleted = transitions.filter((t) => t.to === "Completed");
    const leadTimeHours =
      intoCompleted.length === 0
        ? 0
        : intoCompleted.reduce((sum, t) => sum + (t.hours as number), 0) /
          intoCompleted.length;

    // Deploy freq: deploys per day over the window.
    const windowMs = window.end.getTime() - window.start.getTime();
    const windowDays = windowMs > 0 ? windowMs / MS_PER_DAY : 0;
    const deployFreqPerDay = windowDays > 0 ? deploys.length / windowDays : 0;

    // CFR: incidents / deploys.
    const cfr = deploys.length === 0 ? 0 : incidents.length / deploys.length;

    // MTTR: mean recovery time across incidents with completedAt set.
    const recovered = incidents.filter((i) => i.completedAt !== undefined);
    const mttrHours =
      recovered.length === 0
        ? 0
        : recovered.reduce((sum, i) => {
            const completedAt = i.completedAt as Date;
            return sum + (completedAt.getTime() - i.createdAt.getTime()) / MS_PER_HOUR;
          }, 0) / recovered.length;

    return {
      tiers: {
        lead_time: tierFor("lead_time", leadTimeHours),
        deploy_freq: tierFor("deploy_freq", deployFreqPerDay),
        // tierFor("change_failure_rate", x) requires x in [0, 1] — clamp
        // defensively in case adapter math produced a tiny float overshoot.
        change_failure_rate: tierFor(
          "change_failure_rate",
          Math.max(0, Math.min(1, cfr)),
        ),
        mttr: tierFor("mttr", mttrHours),
      },
      raw: { leadTimeHours, deployFreqPerDay, cfr, mttrHours },
    };
  }
}
