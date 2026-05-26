// SENTINEL-25 — GetTokenEconomics use case.
//
// Summarises the token-usage stream into total spend, cache hit rate, and
// a per-ticket breakdown. The byTicket array is the raw TokenSession[]
// the repository returned — callers can re-aggregate as needed.

import { makeDollars, type Dollars } from "../domain";
import type { MetricsRepository, TimeRange, TokenSession } from "../ports";

export interface GetTokenEconomicsResult {
  readonly totalCostUsd: Dollars;
  readonly cacheHitRate: number;
  readonly byTicket: readonly TokenSession[];
}

export class GetTokenEconomics {
  constructor(private readonly repo: MetricsRepository) {}

  async run(window: TimeRange): Promise<GetTokenEconomicsResult> {
    const sessions = await this.repo.readTokenUsage(window);
    let totalCost = 0;
    let cacheRead = 0;
    let cacheCreation = 0;
    let inputTotal = 0;
    for (const s of sessions) {
      totalCost += s.costUsd;
      cacheRead += s.cacheRead;
      cacheCreation += s.cacheCreation;
      inputTotal += s.totalInput;
    }
    const denom = cacheRead + cacheCreation + inputTotal;
    const cacheHitRate = denom > 0 ? cacheRead / denom : 0;
    return {
      totalCostUsd: makeDollars(totalCost),
      cacheHitRate,
      byTicket: sessions,
    };
  }
}
