// SENTINEL-29 — Master dashboard composition root.
//
// Server Component (no "use client"): instantiates concrete adapters,
// awaits each application use case, and pipes the resolved results into
// the organism panels rendered inside DashboardLayout. Adapters degrade
// to empty arrays when their source files don't exist (ENOENT), so the
// page renders an "empty" dashboard cleanly on a fresh machine.

import os from "node:os";

import {
  CachedLinearGateway,
  JsonlMetricsRepository,
  SystemClock,
} from "@/adapters";
import {
  GetDoraTier,
  GetROI,
  GetSLABreaches,
  GetTokenEconomics,
  GetWipByStage,
} from "@/application";
import { ROIRatio } from "@/components/molecules";
import {
  CycleTimeBreakdown,
  DoraPanel,
  SLAGrid,
  TokenEconomicsPanel,
  WipBoard,
} from "@/components/organisms";
import { DashboardLayout } from "@/components/templates";
import {
  bottleneckStage,
  makeSLA,
  STAGES,
  type SLA,
  type Stage,
  type StageThroughput,
  type StageTransition,
} from "@/domain";

const MS_PER_DAY = 1000 * 60 * 60 * 24;
const WINDOW_DAYS = 30;

// Default SLA set — hardcoded for SEN-29's first cut; SEN-19 / a future
// config loader can lift this to TOML.
const DEFAULT_SLAS: readonly SLA[] = [
  makeSLA({
    id: "review-24h",
    name: "Code Review < 24h",
    target_hours: 24,
    predicate: (ctx) => ctx.stage === "Code Review",
  }),
  makeSLA({
    id: "qa-48h",
    name: "QA Testing < 48h",
    target_hours: 48,
    predicate: (ctx) => ctx.stage === "QA Testing",
  }),
  makeSLA({
    id: "in-progress-72h",
    name: "Active work < 72h",
    target_hours: 72,
    predicate: (ctx) => ctx.stage === "In Progress",
  }),
];

/** Mean cycle hours into each stage. Missing stages omitted. */
function aggregateByStage(
  transitions: readonly StageTransition[],
): Partial<Record<Stage, number>> {
  const sums = new Map<Stage, number>();
  const counts = new Map<Stage, number>();
  for (const t of transitions) {
    const stage = t.to;
    sums.set(stage, (sums.get(stage) ?? 0) + (t.hours as number));
    counts.set(stage, (counts.get(stage) ?? 0) + 1);
  }
  const out: Partial<Record<Stage, number>> = {};
  for (const stage of STAGES) {
    const n = counts.get(stage) ?? 0;
    if (n === 0) continue;
    out[stage] = (sums.get(stage) ?? 0) / n;
  }
  return out;
}

/** Count of transitions into each stage. Used as a rough throughput proxy. */
function throughputFromTransitions(
  transitions: readonly StageTransition[],
  days: number,
): StageThroughput {
  const counts: Partial<Record<Stage, number>> = {};
  for (const t of transitions) {
    counts[t.to] = (counts[t.to] ?? 0) + 1;
  }
  const out: StageThroughput = {};
  for (const stage of STAGES) {
    const n = counts[stage] ?? 0;
    if (n === 0 || days <= 0) continue;
    out[stage] = n / days;
  }
  return out;
}

export default async function HomePage() {
  const clock = new SystemClock();
  const home = os.homedir();
  const repo = new JsonlMetricsRepository(home);
  const linear = new CachedLinearGateway(home, "firefly-pro");

  const now = clock.now();
  const start = new Date(now.getTime() - WINDOW_DAYS * MS_PER_DAY);
  const window = { start, end: now };

  const [dora, wip, tokens, roi, transitions, breaches] = await Promise.all([
    new GetDoraTier(repo).run(window),
    new GetWipByStage(linear, clock).run(),
    new GetTokenEconomics(repo).run(window),
    new GetROI(repo, linear).run(window),
    repo.readCycleTimeEvents(window),
    new GetSLABreaches(linear, clock).run(DEFAULT_SLAS),
  ]);

  const byStage = aggregateByStage(transitions);
  const throughput = throughputFromTransitions(transitions, WINDOW_DAYS);
  const bottleneck = bottleneckStage(wip, throughput);

  return (
    <DashboardLayout windowLabel={`LAST ${WINDOW_DAYS} DAYS`}>
      <DoraPanel result={dora} />
      <WipBoard snapshot={wip} bottleneck={bottleneck} />
      <TokenEconomicsPanel result={tokens} />
      <ROIRatio ratio={roi.ratio as number} basis={roi.basis} />
      <SLAGrid
        slas={DEFAULT_SLAS.map((s) => ({
          id: s.id,
          name: s.name,
          target_hours: s.target_hours,
        }))}
        breaches={breaches}
      />
      <CycleTimeBreakdown byStage={byStage} />
    </DashboardLayout>
  );
}
