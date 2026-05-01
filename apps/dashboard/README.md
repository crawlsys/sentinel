# Sentinel Dashboard

Next.js 15 + TypeScript + MUI v6 + Tailwind v4 dashboard for the sentinel hook
engine. Surfaces proofs, workflows, hook execution stats, skill telemetry, and
the tokens-per-ticket aggregator (SENTINEL-7) consumed from the sentinel
daemon REST API.

## Quick start

```bash
cd apps/dashboard
pnpm install
cp .env.local.example .env.local   # then point NEXT_PUBLIC_SENTINEL_API_URL
pnpm dev                            # http://localhost:3001
```

Other scripts:

```bash
pnpm tsc       # type-check (no emit)
pnpm lint      # next lint
pnpm build     # production build
pnpm test      # vitest unit tests
```

## Architecture

DDD / Hexagonal layout under `src/` with atomic-design components:

| Path | Owner | Filled by |
|------|-------|-----------|
| `app/` | Next.js App Router (routes, layout) | this PR (SENTINEL-20) |
| `src/theme/` | MUI theme + Tailwind layer | SENTINEL-21 |
| `src/domain/` | Pure types: proofs, sessions, metrics | SENTINEL-22 |
| `src/ports/` | Repository interfaces | SENTINEL-23 |
| `src/adapters/` | Sentinel daemon REST client | SENTINEL-24 |
| `src/application/` | Use cases / view-model logic | SENTINEL-25 |
| `src/components/atoms/` | Primitive UI (button, badge) | SENTINEL-26 |
| `src/components/molecules/` | Composed UI (stat card, row) | SENTINEL-27 |
| `src/components/organisms/` | Whole panels (proof table, charts) | SENTINEL-28 |
| `src/components/templates/` | Page layouts | SENTINEL-29 |
| `tests/` | Vitest specs | SENTINEL-30 |

Tailwind v4 is layout-only — MUI owns components. Google Fonts (Space Grotesk,
Space Mono, Doto) are loaded via `next/font/google` in `app/layout.tsx` and
exposed as CSS variables (`--font-sans`, `--font-mono`, `--font-display`).

## Sentinel daemon

The dashboard talks to the sentinel daemon REST API (default `http://localhost:3002`
once SENTINEL-31 lands; before that the daemon defaults to 3001 and conflicts
with `pnpm dev`). Override via `NEXT_PUBLIC_SENTINEL_API_URL` in `.env.local`.
