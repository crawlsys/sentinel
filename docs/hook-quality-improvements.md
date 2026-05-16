# Hook Quality Improvements — Observed Friction and Targeted Fixes

**Status:** Proposed (advisory; Gary picks up at his pace)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source:** Direct friction observed during the 2026-05-15 / 2026-05-16 session that wired up Sentinel's hook engine and exercised it across multiple repos.

---

## TL;DR

Three concrete hook-quality issues surfaced during the session that just wired Sentinel's hook engine end-to-end. Each is a small, targeted fix; together they materially improve the day-one experience of Sentinel running on top of an existing development workflow. All three honor hex/DDD layering (no IO in `consul-domain`-style purity — sentinel's equivalent is `sentinel-domain` having no IO; these fixes touch `sentinel-application/src/hooks/`, with one optional port extension).

The issues, in priority order:

1. **`worktree_reminder` false positive** — detects only `.claude/worktrees/`-style worktrees; misses any worktree created with raw `git worktree add`. Fired during this session inside a worktree, prompting a worktree reminder. *Low-cost fix; high noise reduction.*

2. **`tool_usage_gate` granularity** — gates Edit/Write uniformly regardless of whether the target is source code or a memory note. Blocked a 10-line memory write the same way it would block a 500-line source edit. *Moderate-cost fix; addresses the broader R3 retirement principle (reversibility-graded gating).*

3. **`doc_drift` BUILDING.md accuracy** — fires on every project missing `BUILDING.md`, regardless of whether the project follows the canonical Sentinel template. Surfaced during this session against a non-Sentinel-template repo. *Low-cost fix; reduces low-signal noise.*

This is an advisory doc, not a quarantine or governance policy. Gary picks up each at his pace.

---

## Issue 1 — `worktree_reminder` false positive on non-`.claude/worktrees/` worktrees

### Observed behavior

During the session, the user was in a worktree at `/Users/jared.cluff/firefly/gitrepos/sentinel-worktrees/ai-factory-brief` (created via raw `git worktree add` on a sibling path). On a subsequent UserPromptSubmit, `worktree_reminder` injected its standard reminder:

> 🟡 [Worktree Reminder] You are in a git repository. Use `EnterWorktree` to create an isolated worktree before making code changes.

This was a false positive: the session *was already inside a worktree* — just not one under the `.claude/worktrees/` path that `EnterWorktree` produces.

### Root cause

`crates/sentinel-application/src/hooks/worktree_reminder.rs:32-36`:

```rust
fn is_inside_worktree(cwd: &str) -> bool {
    let normalized = cwd.replace('\\', "/");
    normalized.contains(".claude/worktrees/")
}
```

The detection is path-pattern-based and only recognizes worktrees created by Claude Code's `EnterWorktree` tool. Worktrees created by raw `git worktree add`, by IDE integrations, or by team-shared scripts living elsewhere on disk are invisible to this check.

### Proposed fix

Use git's own definition of "inside a worktree" rather than a path heuristic. In a linked worktree, the immediate `.git` is a **file** (containing `gitdir: <path-to-main-.git>`) rather than a **directory**. The canonical detection:

```rust
fn is_inside_worktree(cwd: &str) -> bool {
    let path = Path::new(cwd);
    let mut current = Some(path);
    while let Some(dir) = current {
        let git_path = dir.join(".git");
        if git_path.is_file() {
            // .git is a file → linked worktree (per git-worktree(1))
            return true;
        }
        if git_path.is_dir() {
            // .git is a directory → main working tree
            return false;
        }
        current = dir.parent();
    }
    false
}
```

This subsumes the existing `.claude/worktrees/` check (those are linked worktrees too) and catches every other worktree convention. The current `is_git_repo` check at lines 18-29 remains unchanged.

### Test additions

In `worktree_reminder.rs`'s test module (or its companion test file if separated):

```rust
#[test]
fn detects_linked_worktree_via_git_file() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    fs::create_dir_all(main.join(".git")).unwrap();
    let wt = tmp.path().join("worktree");
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join(".git"), format!("gitdir: {}\n", main.join(".git").display())).unwrap();
    assert!(is_inside_worktree(wt.to_str().unwrap()));
}

#[test]
fn detects_claude_code_worktree_under_dot_claude_worktrees() {
    // Backward-compatibility test: the .claude/worktrees/ pattern still detected
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join(".claude").join("worktrees").join("feat-x");
    fs::create_dir_all(&main).unwrap();
    fs::write(main.join(".git"), "gitdir: ...\n").unwrap();
    assert!(is_inside_worktree(main.to_str().unwrap()));
}

#[test]
fn does_not_treat_main_worktree_as_linked() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join(".git")).unwrap();
    assert!(!is_inside_worktree(tmp.path().to_str().unwrap()));
}
```

### Hex/DDD impact

Minor. The function lives in `sentinel-application/src/hooks/worktree_reminder.rs` already. Adding a `FileSystemPort` parameter for testability would be the cleaner long-term move — currently the function uses `std::path::Path::is_file()` / `is_dir()` directly, which is a small IO call inside application code. Acceptable as-is (consistent with surrounding hook patterns); cleaner if eventually routed through a port.

---

## Issue 2 — `tool_usage_gate` granularity: same gate for source edits and memory notes

### Observed behavior

During the session, an attempted Write of a ~30-line memory file at `/Users/jared.cluff/.claude/projects/-Users-jared-cluff-firefly/memory/architecture_hexagonal_ddd.md` was gated by `tool_usage_gate` the same way it would have gated a Write of `crates/sentinel-domain/src/lib.rs`. Both required: sequential-thinking marker + task created + plan mode + active task in_progress. The full preconditions stack fired for a trivial markdown memory note.

This isn't a bug — the gate is doing what it's designed to do. But the *granularity* is uniform: every Edit/Write triggers the same four-check stack regardless of target risk class. This is the exact pattern the brief's recommendation **R3** (retire novelty-as-primary) and **A6** (reversibility-graded tripwires) are designed to fix.

### Root cause

`crates/sentinel-application/src/hooks/tool_usage_gate.rs:564-572`:

```rust
let in_scope = match tool {
    "Edit" | "Write" => true,
    "Bash" => bash_command_is_mutating(input),
    other if other.starts_with("mcp__") => is_mutating_mcp_tool(other),
    _ => false,
};
if !in_scope {
    return HookOutput::allow();
}
```

`in_scope` is a binary yes/no based on tool name alone. There is no path-based or content-based discrimination — every Write is treated as a maximally-risky mutation.

### Proposed fix — minimum viable

Add a path-based **trivial-write exemption** for known-safe locations. These are paths where the content cannot cause a code regression or production change:

```rust
fn is_trivial_write_path(input: &HookInput) -> bool {
    let Some(file_path) = input.tool_input.get("file_path").and_then(|v| v.as_str()) else {
        return false;
    };
    let normalized = file_path.replace('\\', "/");

    // Exemption list — locations whose writes cannot regress code or affect prod.
    const TRIVIAL_PREFIXES: &[&str] = &[
        "/.claude/projects/",           // Claude Code session metadata + memory
        "/.claude/plans/",              // Claude Code plan files
        "/.claude/sentinel/state/",     // Sentinel session state
        "/.claude/sentinel/metrics/",   // Sentinel metrics (JSONL)
    ];
    // Match anywhere in path (handles different home dir conventions)
    TRIVIAL_PREFIXES.iter().any(|p| normalized.contains(p))
}
```

Apply at the top of `process` before the in_scope check:

```rust
if matches!(tool, "Edit" | "Write") && is_trivial_write_path(input) {
    return HookOutput::allow();
}
```

This adopts a **reversibility-class** view (per the brief's A6) without yet building the full reversibility-graded tripwire system. The exempted paths are trivially reversible (session metadata, plan files, state, metrics) — none of them represents code changes or production state.

### Proposed fix — full version (separate ADR)

The full reversibility-graded gating is a larger architectural change (A6 in the brief). Recommend treating Issue 2 as the *minimum viable* fix shipped now, with the full reversibility-graded system landing as a separate ADR per the brief's adoption plan. The exemption list is the bridge: it captures the most painful uniform-gating cases without committing to the full reversibility classification of every tool yet.

### Test additions

```rust
#[test]
fn trivial_write_to_memory_path_is_allowed() {
    let input = HookInput {
        tool_name: Some("Write".into()),
        tool_input: serde_json::json!({
            "file_path": "/Users/x/.claude/projects/foo/memory/note.md",
            "content": "hello",
        }),
        ..Default::default()
    };
    let fs = InMemoryFs::new();
    let env = InMemoryEnv::new();
    assert_eq!(process(&input, &fs, &env), HookOutput::allow());
}

#[test]
fn source_code_write_is_still_gated() {
    let input = HookInput {
        tool_name: Some("Write".into()),
        tool_input: serde_json::json!({
            "file_path": "/path/to/repo/src/lib.rs",
            "content": "fn foo() {}",
        }),
        session_id: Some("sess-1".into()),
        ..Default::default()
    };
    let fs = InMemoryFs::new();  // empty — no markers, no plan file
    let env = InMemoryEnv::new();
    // Gate should fire (we have no sequential-thinking marker)
    assert!(matches!(process(&input, &fs, &env), HookOutput::Deny { .. }));
}
```

### Hex/DDD impact

Minimal. `is_trivial_write_path` is pure (only reads `input.tool_input`); no new IO. Lives in `tool_usage_gate.rs` alongside the existing `bash_command_is_mutating` and `is_mutating_mcp_tool` helpers, same shape.

---

## Issue 3 — `doc_drift` BUILDING.md accuracy

### Observed behavior

During the session, `doc_drift` injected the following finding into the user's context:

> [Doc Drift] 1 documentation issue(s) detected in this project.
>
> 1. **BUILDING.md**: Missing BUILDING.md — project has no build documentation
>    - Run `sentinel init` to generate this file from templates

The active project was a multi-purpose firefly working tree containing several distinct sub-repositories (sentinel, legatus-consul-agent, firefly-pro-crm, etc.). The hook fired against the *top-level firefly directory* — which is not itself a Sentinel-template project — and demanded `BUILDING.md`.

This is a false positive of a slightly different shape than Issue 1. The detector is right that no `BUILDING.md` exists; it's wrong that one *should* exist in this location. The detector lacks context: it doesn't distinguish "project that should follow Sentinel's canonical template" from "non-Sentinel-template directory containing some Sentinel-template projects."

### Root cause

`crates/sentinel-application/src/hooks/doc_drift.rs:29, 231, 422, 528, 652` — the BUILDING.md detection runs unconditionally against the cwd whenever doc_drift fires. The hook has no notion of "is this project actually following the Sentinel canonical template?" — it just checks file existence in cwd.

### Proposed fix

Gate the BUILDING.md (and equivalent SECURITY.md, LICENSE, etc.) checks on the presence of a **template-compliance marker**. The simplest marker: a `.sentinel-init` file at the project root (touched by `sentinel init` on first run), or — if that feels intrusive — the presence of *any* canonical sentinel-template file (`CHANGELOG.md` following Keep a Changelog, `rustfmt.toml`, `.editorconfig` with the canonical contents, etc.).

Pseudo-code:

```rust
fn project_follows_sentinel_template(cwd: &Path) -> bool {
    // Marker 1: explicit (sentinel init touches this)
    if cwd.join(".sentinel-init").exists() {
        return true;
    }
    // Marker 2: implicit (any two of these together)
    let canonical_files = ["CHANGELOG.md", "rustfmt.toml", ".editorconfig"];
    let present_count = canonical_files
        .iter()
        .filter(|f| cwd.join(f).exists())
        .count();
    present_count >= 2
}
```

Then, in the doc-drift detector:

```rust
for doc in &template_documents {
    if !project_follows_sentinel_template(cwd) && doc == "BUILDING.md" {
        // Skip — this project doesn't follow the Sentinel template
        continue;
    }
    // ... existing detection logic
}
```

### Alternative: per-detector opt-in

A more conservative approach: keep the detection but downgrade severity from "RECOMMENDED" to "INFORMATIONAL" when the template-compliance markers are absent. The hook still surfaces the finding but doesn't recommend `sentinel init` to a project that may have its own conventions.

### Test additions

```rust
#[test]
fn skips_building_md_check_when_project_not_sentinel_template() {
    let tmp = tempfile::tempdir().unwrap();
    // No CHANGELOG.md, no rustfmt.toml, no .sentinel-init — not a Sentinel template
    let findings = detect_drift(tmp.path());
    assert!(!findings.iter().any(|f| f.contains("BUILDING.md")));
}

#[test]
fn detects_building_md_when_template_markers_present() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("CHANGELOG.md"), "# Changelog").unwrap();
    fs::write(tmp.path().join("rustfmt.toml"), "edition = \"2021\"").unwrap();
    let findings = detect_drift(tmp.path());
    assert!(findings.iter().any(|f| f.contains("BUILDING.md")));
}
```

### Hex/DDD impact

Minimal. `project_follows_sentinel_template` is pure (only reads file existence); no new IO beyond what's already happening in the detector.

---

## Cross-cutting observations

### These three findings are independently valid and should land separately

Each can be a small commit on its own branch, reviewed individually. They are unrelated technically (different hooks, different files, different concerns) — only related by the common observation that "false positives on common patterns drain trust in the hook engine."

### Connection to brief recommendation A6

Issue 2 (gate granularity) is a direct application of brief recommendation **A6 — reversibility-graded tripwires** at the smallest possible scope. The full A6 system classifies every tool by reversibility class. The proposed fix here uses a path-based exemption list as the bridge — covers the most painful cases without committing to the full classification yet. When the full A6 ADR lands, the exemption list folds into it cleanly.

### Connection to the user's "hook authority" framing

The user's global CLAUDE.md establishes that `[Sentinel-Authority]`-tagged hook directives must be obeyed. That trust contract is asymmetric: the more often hooks fire false positives, the more the agent (and the user) is conditioned to discount hook output mentally. Each false positive eroded is a marginal increase in the actual operating authority of the gating hooks. Worth treating this as a quality-of-authority concern, not just polish.

### Recommended order

1. **Issue 1 (worktree_reminder)** — smallest, highest-noise-reduction. Ship first.
2. **Issue 3 (doc_drift BUILDING.md)** — small, addresses observed user-context noise.
3. **Issue 2 (tool_usage_gate granularity)** — slightly larger; bridges to the bigger A6 work.

---

## Methodology

These findings come from direct session evidence (the user wired up Sentinel's hook engine and the gate fired on the patterns described). No literature citations needed — the evidence is the session itself. Sentinel session ID and timestamp are in the user's session logs if Gary wants the audit trail.

## Decision and ownership

- **Decision class:** technical debt / hook-quality improvements. Advisory, not governance.
- **Owner:** Gary Somerhalder picks up each at his pace.
- **No ratification block** — these are issue reports + proposed fixes, not commitments.
