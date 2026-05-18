// SENTINEL-25 — GetWipByStage use case.
//
// Reads the active ticket set, groups by (team, stage), returns a
// WipSnapshot stamped with clock.now(). Teams with zero tickets are
// omitted from `by_team`; stages within a present team always carry a
// fully-populated zero map (via emptyWipByStage()).

import { emptyWipByStage, type WipSnapshot } from "../domain";
import type { Clock, LinearGateway } from "../ports";

export class GetWipByStage {
  constructor(
    private readonly gateway: LinearGateway,
    private readonly clock: Clock,
  ) {}

  async run(): Promise<WipSnapshot> {
    const tickets = await this.gateway.getActiveTickets();
    const byTeam: Record<string, ReturnType<typeof emptyWipByStage>> = {};
    for (const ticket of tickets) {
      const teamKey = String(ticket.team);
      if (!byTeam[teamKey]) {
        byTeam[teamKey] = emptyWipByStage();
      }
      byTeam[teamKey][ticket.state] += 1;
    }
    return {
      ts: this.clock.now(),
      by_team: byTeam,
    };
  }
}
