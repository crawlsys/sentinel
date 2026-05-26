// SENTINEL-25 — GetROI use case.
//
// Computes ROI = humanCost / claudeCost over a TimeRange. Story-points
// basis when at least one ticket has both a ticketed TokenSession AND a
// known estimate; otherwise falls back to days-basis using the window
// length and HUMAN_USD_PER_DAY.

import {
  compute,
  HUMAN_USD_PER_POINT,
  makeDollars,
  roiForDays,
  type Dollars,
  type ROIRatio,
} from "../domain";
import type { LinearGateway, MetricsRepository, TimeRange } from "../ports";

export interface GetROIResult {
  readonly humanCostUsd: Dollars;
  readonly claudeCostUsd: Dollars;
  readonly ratio: ROIRatio;
  readonly basis: "story_points" | "days_fallback";
}

const MS_PER_DAY = 1000 * 60 * 60 * 24;

export class GetROI {
  constructor(
    private readonly repo: MetricsRepository,
    private readonly gateway: LinearGateway,
  ) {}

  async run(window: TimeRange): Promise<GetROIResult> {
    const [sessions, tickets] = await Promise.all([
      this.repo.readTokenUsage(window),
      this.gateway.getActiveTickets(),
    ]);

    const claudeCostUsd = makeDollars(
      sessions.reduce((sum, s) => sum + s.costUsd, 0),
    );

    // Build a (ticketId → estimate) lookup from active tickets.
    const estimateByTicket = new Map<string, number>();
    for (const t of tickets) {
      if (t.estimate !== null) {
        estimateByTicket.set(String(t.id), t.estimate as number);
      }
    }

    // Sum estimates for tickets that also appear in the token-usage stream.
    let points = 0;
    let pointsCount = 0;
    for (const s of sessions) {
      if (!s.ticketId) continue;
      const est = estimateByTicket.get(String(s.ticketId));
      if (est === undefined) continue;
      points += est;
      pointsCount += 1;
    }

    if (pointsCount > 0) {
      const humanCostUsd = makeDollars(points * HUMAN_USD_PER_POINT);
      return {
        humanCostUsd,
        claudeCostUsd,
        ratio: compute(humanCostUsd, claudeCostUsd),
        basis: "story_points",
      };
    }

    // Fallback: assume the window length represents human days of effort.
    const windowMs = window.end.getTime() - window.start.getTime();
    const days = windowMs > 0 ? windowMs / MS_PER_DAY : 0;
    const ratio = roiForDays(days, claudeCostUsd);
    const humanCostUsd = makeDollars((ratio as number) * (claudeCostUsd as number));
    return {
      humanCostUsd,
      claudeCostUsd,
      ratio,
      basis: "days_fallback",
    };
  }
}
