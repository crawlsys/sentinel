"use client";

import type { ReactElement } from "react";
import Box from "@mui/material/Box";

import { Label } from "@/components/atoms";
import { BottleneckCallout, WipChip } from "@/components/molecules";
import { STAGES, type Stage, type WipSnapshot } from "@/domain";

/** Props for {@link WipBoard}. */
export interface WipBoardProps {
  readonly snapshot: WipSnapshot;
  /** Optional bottleneck stage. `null` renders the idle callout. */
  readonly bottleneck?: Stage | null;
  /** Optional per-stage threshold used by `WipChip` to colour itself. */
  readonly threshold?: number;
}

/**
 * Per-team WIP board: a `BottleneckCallout` banner up top, then one row
 * per team showing a `WipChip` for every non-zero stage. Teams render in
 * deterministic alphabetical order.
 */
export function WipBoard(props: WipBoardProps): ReactElement {
  const { snapshot, bottleneck, threshold } = props;
  const teamNames = Object.keys(snapshot.by_team).sort();
  return (
    <Box
      data-testid="wip-board"
      sx={{
        display: "flex",
        flexDirection: "column",
        gap: 2,
      }}
    >
      <BottleneckCallout stage={bottleneck ?? null} />
      {teamNames.length === 0 ? (
        <Label tone="secondary">NO ACTIVE TICKETS</Label>
      ) : (
        teamNames.map((team) => {
          const teamMap = snapshot.by_team[team];
          if (!teamMap) return null;
          const presentStages: Stage[] = STAGES.filter(
            (s) => (teamMap[s] ?? 0) > 0,
          );
          return (
            <Box
              key={team}
              data-team={team}
              sx={{
                display: "flex",
                flexDirection: "row",
                alignItems: "center",
                flexWrap: "wrap",
                gap: 1,
              }}
            >
              <Label>{team}</Label>
              {presentStages.length === 0 ? (
                <Label tone="secondary">IDLE</Label>
              ) : (
                presentStages.map((stage) => (
                  <WipChip
                    key={stage}
                    stage={stage}
                    count={teamMap[stage] ?? 0}
                    {...(threshold !== undefined ? { threshold } : {})}
                  />
                ))
              )}
            </Box>
          );
        })
      )}
    </Box>
  );
}
