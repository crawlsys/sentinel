"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

import { SLABadge } from "@/components/molecules";
import type { SLABreach } from "@/domain";

/**
 * Serializable subset of an SLA for transport across the Server → Client
 * boundary. The full domain `SLA` carries a `predicate` function that
 * React Server Components can't serialize, so the composition root maps
 * each SLA to this shape before handing it to the grid.
 */
export interface SLAGridEntry {
  readonly id: string;
  readonly name: string;
  readonly target_hours: number;
}

/** Props for {@link SLAGrid}. */
export interface SLAGridProps {
  readonly slas: readonly SLAGridEntry[];
  readonly breaches: readonly SLABreach[];
}

interface BreachSummary {
  count: number;
  maxElapsed: number;
}

function summariseBreaches(
  breaches: readonly SLABreach[],
): Map<string, BreachSummary> {
  const out = new Map<string, BreachSummary>();
  for (const b of breaches) {
    const prior = out.get(b.sla_id);
    if (prior) {
      prior.count += 1;
      prior.maxElapsed = Math.max(prior.maxElapsed, b.elapsed_hours);
    } else {
      out.set(b.sla_id, { count: 1, maxElapsed: b.elapsed_hours });
    }
  }
  return out;
}

/**
 * Grid of SLA badges. One badge per configured SLA; each goes `breached`
 * when at least one ticket has triggered it, with the worst elapsed time
 * surfaced as subtext.
 */
export function SLAGrid(props: SLAGridProps): ReactElement {
  const { slas, breaches } = props;
  const summary = summariseBreaches(breaches);
  return (
    <Box
      data-testid="sla-grid"
      sx={{
        display: "grid",
        gridTemplateColumns: { xs: "1fr", md: "repeat(2, 1fr)" },
        gap: 1.5,
      }}
    >
      {slas.map((sla) => {
        const info = summary.get(sla.id);
        const breached = info !== undefined && info.count > 0;
        return (
          <SLABadge
            key={sla.id}
            slaName={sla.name}
            breached={breached}
            {...(breached
              ? {
                  elapsedHours: info.maxElapsed,
                  targetHours: sla.target_hours,
                }
              : {})}
          />
        );
      })}
    </Box>
  );
}
