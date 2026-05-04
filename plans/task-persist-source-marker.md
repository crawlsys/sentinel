# Plan: Add source: auto marker to task_persist memory frontmatter

## Goal
Add `source: auto` to the YAML frontmatter that `write_memory_summary` writes
to `~/.claude/projects/<key>/memory/project_tasks.md` so memory audits can
filter auto-generated task snapshots from human-curated memories.

## Approach
Approach (b): Add a `source: auto` line to the YAML frontmatter block in
`write_memory_summary` in `task_persist.rs`. The qdrant-mcp-rust `store_memory`
does not accept a `source` field (not in the function signature or domain
`Memory` struct), so we cannot use approach (a) without modifying that repo.
The flat file written here IS the memory file, so the frontmatter is the
correct place for this marker.

## Change
File: `crates/sentinel-application/src/hooks/task_persist.rs`
Function: `write_memory_summary`
Add: `body.push_str("source: auto\n");` after `type: project\n`

## Tests to update
`test_write_memory_summary_creates_files` asserts `body.contains("type: project")`
— add assertion for `source: auto` too.
