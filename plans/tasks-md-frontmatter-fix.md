# Plan: fix tasks.md frontmatter in two locations

**Date:** 2026-04-30
**Session:** 34c6d946-0809-4702-9ab3-2cd759bd0cd4
**Approved by:** Gary Somerhalder ("lets do all that way we have tasks saved permanently")

## Context

Three copies of the 31-task SENTINEL/FACTORY/DASH roadmap were created via `cp` from `~/.claude/persistent-tasks/4a2bc449/tasks.md`:

1. `C:\Users\garys\Documents\GitHub\sentinel\tasks.md` (new repo-root copy)
2. `C:\Users\garys\.claude\persistent-tasks\c08fea48\tasks.md` (sentinel-cwd persistent-tasks bucket)
3. `C:\Users\garys\.claude\persistent-tasks\4a2bc449\tasks.md` (canonical source — left untouched)

All three currently carry the 4a2bc449 source frontmatter. Locations 1 and 2 need their YAML frontmatter rewritten so each file accurately identifies its own location and current session.

## Changes

Frontmatter-only edits to two files. Body content (1004 lines of task descriptions) is identical to source and stays as-is.

### File 1: ~/.claude/persistent-tasks/c08fea48/tasks.md

New frontmatter sets project_hash to c08fea48, cwd to the sentinel repo root, session to current, updated to today, plus a "source" line.

### File 2: sentinel/tasks.md

New frontmatter omits project_hash (not a persistent-tasks bucket file), points cwd at sentinel, sets session+updated, adds source + "Live TaskList IDs 1-31 match" note.

## Out of scope

- File 3 (4a2bc449) is the historical canonical entry — not modified.
- No code changes anywhere. tasks.md is documentation/state.

## Risk

Negligible. Frontmatter-only YAML edits to two files. Reversible — original at 4a2bc449 is untouched.
