# Sentinel Dashboard — Local Run Notes

The sentinel dashboard runs locally only as of 2026-05-18. There is no
hosted staging or production environment; the deploy scaffolding is
intentionally deferred (see "Why no deploy yet?" below).

## Quick start

```bash
cd apps/dashboard
pnpm install
pnpm dev                    # http://localhost:3001
```

Open `http://localhost:3001` — every dashboard panel renders against the
local JSONL files under `~/.claude/sentinel/metrics/` plus the per-project
Linear cache at `~/.claude/sentinel/linear-assigned-firefly-pro.json`.

## Verify everything passes

```bash
cd apps/dashboard
pnpm tsc        # zero TypeScript errors expected
pnpm lint       # zero ESLint warnings expected
pnpm test       # all vitest tests green (page-level integration + per-tier snapshots)
pnpm build      # production build succeeds; / route prerenders as static
```

The page-level integration test (`tests/e2e/dashboard-page.test.tsx`)
catches stack-wide regressions including any future RSC-serialization
issues — it renders the actual `<HomePage />` Server Component through
`react-dom/server` and asserts every expected `data-testid` lands in the
HTML.

## Sharing the local dashboard

To show the running dashboard to someone temporarily without deploying:

```bash
cloudflared tunnel --url http://localhost:3001
```

This produces a public `*.trycloudflare.com` URL that proxies your local
`pnpm dev` for as long as the tunnel stays up. Kill the tunnel and the URL
dies — no DNS, no committed config, no Vercel project.

## Data dependencies

The dashboard adapters degrade gracefully when source files are missing,
so panels render an "empty" state on a fresh machine:

| Adapter / port read | Source file | Populated by |
|--------------------|-------------|--------------|
| `readCycleTimeEvents` | `~/.claude/sentinel/metrics/cycle-time.jsonl` | SEN-1 webhook collector |
| `readDeploys` | `~/.claude/sentinel/metrics/deploys.jsonl` | SEN-9 deploy tracker |
| `readTokenUsage` | `~/.claude/sentinel/metrics/tokens-per-ticket.jsonl` | SEN-7 token aggregator |
| `readIncidents` | (not yet wired) | future SEN-11 |
| `getActiveTickets` | `~/.claude/sentinel/linear-assigned-firefly-pro.json` | Linear refresh cron |

If a panel shows zeros / idle / dashes, check whether the corresponding
source file exists and has recent rows.

## Why no deploy yet?

`tasks.md` SEN-31 originally read "Deploy to Vercel staging + production",
but the project doesn't yet have a Vercel-team home picked — the three
configured Vercel accounts (atlus-pest-solutions, firefly-pro, fireflypro)
each host other products and don't obviously own sentinel. Gary deferred
the deploy on 2026-05-18 pending that decision. When you're ready to ship:

1. Decide which Vercel team will own the sentinel-dashboard project (or
   create a new team).
2. `vercel link` inside `apps/dashboard/` to connect the project.
3. Add a `vercel.json` if any non-default build settings are needed (none
   currently are — Next defaults work).
4. Optional: add a `.github/workflows/deploy-dashboard.yml` that runs
   `vercel deploy` on push to main for staging-only auto-deploy, leaving
   production promotion manual.
5. Update `tasks.md` SEN-31 with the actual deploy URL and commit.

Until then: local-run is the supported path, and `pnpm dev` + cloudflared
tunnel handle every "I need to show this to someone" case.
