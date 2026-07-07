# Claude Code boundary contract

Sentinel is a hook engine that runs *inside* Claude Code (CC). It depends on
dozens of CC-side contracts that CC can change without warning:

- **env vars** CC exports into hook/MCP child processes (e.g. `CLAUDE_CODE_SESSION_ID`),
- **hook-payload field names** CC sends on stdin (e.g. PostToolUse's `tool_response`),
- **protocol method strings** (e.g. `notifications/claude/channel`, `initialUserMessage`),
- **hook event names** (e.g. `SubagentStop`).

When CC silently renames one of these, the dependent sentinel hook goes **dark
with no error** — it deserializes `None`, hits an absent env var, or never
matches an event, and just does nothing. This class of bug is invisible to
greps and unit tests, because nothing *in the sentinel repo* knows CC changed.
The July-2026 audit found five of them this way (most severely: PostToolUse
output moved from `tool_result` to `tool_response`, which had silently disabled
the prompt-injection scanner).

This contract makes that drift **detectable on every CC release** instead of in
production.

## How it works

- **`scripts/cc-boundary-contract.tsv`** — the machine-readable manifest: one
  row per assumption, with the CC-side pattern to look for and whether it must
  be present, absent, or is a deprecation warning.
- **`scripts/cc_contract_check.sh`** — greps each pattern against a
  deobfuscated CC bundle and reports `PASS` / `WARN` / `FAIL`, exiting non-zero
  on any breaking drift.

The oracle is the version-by-version deobfuscated CC bundle produced by
`claude-code-system/decompiler` (see the `claude-code-deobfuscation-stepup`
memory) — `~/Documents/GitHub/claude-code-src/decompile-output/<ver>/decompiled.js`.
Its restored identifier *names* are inferred, but the string literals the
contract checks (env-var names, JSON field keys, method strings, event names,
zod `.describe("@deprecated …")` text) are **not** minified and are ground
truth.

## Running it

```bash
# check against the newest local bundle
scripts/cc_contract_check.sh

# check against a specific bundle
scripts/cc_contract_check.sh ~/Documents/GitHub/claude-code-src/decompile-output/2.1.202-.../decompiled.js
```

Exit `0` = all breaking contracts hold (deprecation `WARN`s allowed); exit `1`
= drift — a sentinel hook may be broken; exit `2` = usage/bundle error.

## The workflow on a CC version bump

1. Step the deobfuscator to the new CC version (`decompiler` → `decompile-output/<new-ver>/`).
2. Run `scripts/cc_contract_check.sh <new bundle>`.
3. For each `FAIL`: the named assumption drifted — fix the sentinel code the
   `sentinel_ref` column points at, then update the manifest's `expect`
   pattern to the new CC reality.
4. For each `WARN` (deprecation): plan the migration off that field/var before
   CC removes it.

## Adding an assumption

When a hook starts depending on a new CC env var / payload field / method /
event, add a row to `cc-boundary-contract.tsv` (tab-separated):

```
id	kind	must_exist	severity	sentinel_ref	expect	note
```

`must_exist`: `true` (pattern must be present), `false` (must be absent), or
`warn` (presence is a deprecation warning). Keep `expect` a narrow `egrep`
pattern that matches the CC-side string, not sentinel's usage.

## Currently tracked

Run the script for the live table. As of the 2.1.201 bundle: 25 breaking
contracts hold, 1 deprecation warning (`team_name`, which CC's schema marks
`@deprecated` — migrate before it is removed).
