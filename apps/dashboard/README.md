# Sentinel Dashboard

Next.js 15 + TypeScript + MUI v6 dashboard for the sentinel hook engine.
Surfaces DORA tiers, SLA breaches, per-stage WIP, token economics, ROI vs
human baseline, and per-stage cycle time — all read from the local sentinel
metrics JSONL files under `~/.claude/sentinel/metrics/` plus the per-project
Linear-assigned cache.

Built with the Nothing aesthetic (Doto display font, Space Mono / Space
Grotesk body, dot-grid background, square edges, palette via the MUI theme
in `src/theme/nothing-theme.ts`).

> **No Tailwind.** MUI owns all components and layout via the `sx` prop and
> theme tokens.

## Quick start (local-only)

```bash
cd apps/dashboard
pnpm install
pnpm dev            # http://localhost:3001
```

That's it — no env file needed, no external services, no API keys. Adapters
read directly from `~/.claude/sentinel/metrics/*.jsonl` and the per-project
Linear cache file on the local filesystem; missing files degrade to empty
panels.

### Other scripts

```bash
pnpm tsc            # type-check (no emit)
pnpm lint           # next lint
pnpm build          # production build
pnpm test           # vitest unit + integration tests
pnpm start          # serve the production build (after `pnpm build`)
```

## Architecture (DDD / Hexagonal + atomic design)

| Path | Owner | Status |
|------|-------|--------|
| `app/` | Next.js App Router (layout, master page) | ✅ SEN-20, SEN-29 |
| `src/theme/` | MUI theme + Nothing tokens | ✅ SEN-21 |
| `src/domain/` | Pure value types + rules (no IO, no React) | ✅ SEN-22 |
| `src/ports/` | Repository + gateway interfaces (no IO) | ✅ SEN-23 |
| `src/adapters/` | JSONL repo + Linear cache + gh-cli gateway | ✅ SEN-24 |
| `src/application/` | Use cases / query handlers | ✅ SEN-25 |
| `src/components/atoms/` | MetricNumber, Label, StatusDot, … | ✅ SEN-26 |
| `src/components/molecules/` | MetricCard, SLABadge, WipChip, … | ✅ SEN-27 |
| `src/components/organisms/` | DoraPanel, WipBoard, TokenEconomicsPanel, … | ✅ SEN-28 |
| `src/components/templates/` | DashboardLayout | ✅ SEN-29 |
| `tests/` | Vitest + page-level integration spec | ✅ SEN-26..SEN-30 |

ESLint enforces the atomic-design boundaries via `no-restricted-imports`
overrides per tier. Run `pnpm lint` to verify.

Google Fonts (Doto, Space Grotesk, Space Mono) load via `next/font/google`
in `app/layout.tsx` and surface as CSS vars (`--font-doto`,
`--font-space-grotesk`, `--font-space-mono`).

## Composition root

`app/page.tsx` is a Server Component that wires the concrete adapters into
the application use cases over a 30-day window, then pipes the resolved
results into the organism panels rendered inside `DashboardLayout`:

```
SystemClock + JsonlMetricsRepository + CachedLinearGateway
        │
        ▼
GetDoraTier · GetWipByStage · GetTokenEconomics · GetROI · GetSLABreaches
        │
        ▼
DoraPanel · WipBoard · TokenEconomicsPanel · ROIRatio · SLAGrid · CycleTimeBreakdown
        │
        ▼
DashboardLayout (template) → <main>
```

## Data sources (read at render time)

| File | Owner module | Used for |
|------|-------------|---------|
| `~/.claude/sentinel/metrics/cycle-time.jsonl` | SEN-1 | DORA lead time, per-stage breakdown |
| `~/.claude/sentinel/metrics/deploys.jsonl` | SEN-9 | DORA deploy frequency |
| `~/.claude/sentinel/metrics/tokens-per-ticket.jsonl` | SEN-7 | Token economics + ROI |
| `~/.claude/sentinel/linear-assigned-{project}.json` | Linear refresh cron | WIP board, ROI estimates |

Adapters return empty arrays on `ENOENT`, so the dashboard renders cleanly
even on a fresh machine with no collected data — every panel just shows
its empty/idle state.

## Deployment

**Currently local-only.** Vercel deploy (SEN-31 in `tasks.md`) is deferred
pending a decision on which Vercel team hosts the project. See
`docs/dashboard-local-run.md` for the local-run notes; the deploy
scaffolding (vercel.json, GitHub Action) is intentionally absent until
that decision is made.

If you want to share the running dashboard with someone temporarily, use
`cloudflared tunnel --url http://localhost:3001` to expose `pnpm dev`
publicly without committing to a deploy target.
