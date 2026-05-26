// SENTINEL-25 — GetSLABreaches use case.
//
// Evaluates a set of SLA rules against the current set of active tickets.
// One breach record per ticket × SLA combination where (predicate matches
// AND elapsed-in-stage > target_hours). `elapsed-in-stage` is approximated
// as `age_hours` for now — a richer per-stage timestamp source (SEN-17)
// can refine this later.

import type { SLA, SLABreach } from "../domain";
import type { Clock, LinearGateway } from "../ports";

const MS_PER_HOUR = 1000 * 60 * 60;

export class GetSLABreaches {
  constructor(
    private readonly gateway: LinearGateway,
    private readonly clock: Clock,
  ) {}

  async run(slas: readonly SLA[]): Promise<SLABreach[]> {
    const tickets = await this.gateway.getActiveTickets();
    const now = this.clock.now();
    const out: SLABreach[] = [];
    for (const ticket of tickets) {
      const ageHours =
        (now.getTime() - ticket.created_at.getTime()) / MS_PER_HOUR;
      // No per-stage history → approximate elapsed-in-stage as ticket age.
      // SEN-17 will replace this with the real stage-entry timestamp.
      const elapsedInStageHours = ageHours;
      for (const sla of slas) {
        const matches = sla.predicate({
          ticket_id: ticket.id,
          priority: ticket.priority,
          stage: ticket.state,
          age_hours: ageHours,
          elapsed_in_stage_hours: elapsedInStageHours,
        });
        if (!matches) continue;
        if (elapsedInStageHours <= sla.target_hours) continue;
        out.push({
          sla_id: sla.id,
          ticket_id: ticket.id,
          breached_at: now,
          elapsed_hours: elapsedInStageHours,
        });
      }
    }
    return out;
  }
}
