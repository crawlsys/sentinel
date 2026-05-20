"""
Sentinel → ActiveGraph bridge

Reads ~/.claude/sentinel/metrics/hook-invocations.jsonl (and sessions.jsonl),
maps each sentinel event to activegraph objects + graph events, and persists
to a SQLite-backed ActiveGraph store so you can inspect/trace/diff sentinel
workflow runs.

Usage:
    # One-shot import (reads existing JSONLs, prints trace):
    python sentinel_bridge.py [--store PATH] [--out-trace]

    # Live-tail mode (watches JSONLs, pushes new lines as they arrive):
    python sentinel_bridge.py --tail [--store PATH]

    # Inspect the result:
    activegraph inspect <store.db>
    activegraph export-trace <store.db> --format jsonl

Options:
    --store PATH  SQLite store path (default: ~/.agents/scratch/activegraph-bridge/sentinel.db)
    --tail        Live-tail the JSONL files
    --out-trace   Print the activegraph trace after ingestion (one-shot mode)
"""

import argparse
import json
import time
import sys
from datetime import datetime, timezone
from pathlib import Path

METRICS_DIRS = [
    Path.home() / ".claude/sentinel/metrics",          # real Claude
    Path.home() / ".claude-sentinel/sentinel/metrics", # sandbox sentinel (autonomous loops)
]
HOOK_INVOCATIONS_PATHS = [d / "hook-invocations.jsonl" for d in METRICS_DIRS]
SESSIONS_JSONL_PATHS   = [d / "sessions.jsonl"         for d in METRICS_DIRS]
# Legacy single-path aliases — first existing wins, for code paths that still reference singletons
METRICS_DIR = next((d for d in METRICS_DIRS if d.exists()), METRICS_DIRS[0])
HOOK_INVOCATIONS = next((p for p in HOOK_INVOCATIONS_PATHS if p.exists()), HOOK_INVOCATIONS_PATHS[0])
SESSIONS_JSONL   = next((p for p in SESSIONS_JSONL_PATHS   if p.exists()), SESSIONS_JSONL_PATHS[0])
DEFAULT_STORE = Path.home() / ".agents/scratch/activegraph-bridge/sentinel.db"

# ── activegraph imports ──────────────────────────────────────────────────────
try:
    import activegraph as ag
except ImportError:
    sys.exit("activegraph not installed — run: pipx install activegraph")


# ── helpers: read JSONL ──────────────────────────────────────────────────────

def _read_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    lines = []
    with path.open() as f:
        for line in f:
            line = line.strip()
            if line:
                try:
                    lines.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
    return lines


def _read_jsonls_merged(paths) -> list[dict]:
    """Read multiple JSONL files, concatenate. Order: by file mtime ascending so newest data sorts last."""
    paths_with_mtime = [(p, p.stat().st_mtime) for p in paths if p.exists()]
    paths_with_mtime.sort(key=lambda x: x[1])
    out = []
    for p, _ in paths_with_mtime:
        out.extend(_read_jsonl(p))
    return out


# ── ingestor helpers ─────────────────────────────────────────────────────────

def _ingest_sessions(graph: ag.Graph, sessions: list[dict]) -> dict[str, str]:
    """
    Create SentinelSession objects for each session record.
    Returns {session_id -> object.id}.
    """
    existing = {
        o.data["session_id"]: o.id
        for o in graph.objects(type="SentinelSession")
    }
    created: dict[str, str] = dict(existing)

    for s in sessions:
        sid = s.get("session_id", "unknown")
        if sid in created:
            continue
        obj = graph.add_object(
            type="SentinelSession",
            data={
                "session_id": sid,
                "cwd": s.get("cwd", ""),
                "platform": s.get("platform", ""),
                "started_at": s.get("ts", ""),
            },
            actor="sentinel_bridge",
        )
        created[sid] = obj.id

        # Domain event: a session started
        graph.emit(ag.Event(
            id=graph.ids.event(),
            type="sentinel.session_started",
            payload={"session_id": sid, "ts": s.get("ts", "")},
            actor="sentinel_bridge",
        ))

    return created


def _ingest_hooks(
    graph: ag.Graph,
    hooks: list[dict],
    session_map: dict[str, str],
    seen_traces: set[str],
) -> int:
    """
    Create SentinelHookInvocation objects + link them to their sessions.
    Returns count of new invocations ingested.
    """
    added = 0
    for h in hooks:
        tid = h.get("trace_id", "")
        if tid in seen_traces:
            continue
        seen_traces.add(tid)
        added += 1

        sid = h.get("session_id", "unknown")

        # Ensure we have a session object (create stub if not seen in sessions.jsonl)
        if sid not in session_map:
            obj = graph.add_object(
                type="SentinelSession",
                data={
                    "session_id": sid,
                    "cwd": "",
                    "platform": "",
                    "started_at": "",
                },
                actor="sentinel_bridge",
            )
            session_map[sid] = obj.id

        hook_obj = graph.add_object(
            type="SentinelHookInvocation",
            data={
                "hook": h.get("hook", ""),
                "event": h.get("event", ""),
                "outcome": h.get("outcome", ""),
                "session_id": sid,
                "trace_id": tid,
                "duration_us": h.get("duration_us", 0),
                "repo_root": h.get("repo_root", ""),
                "ts": h.get("ts", ""),
            },
            actor="sentinel_bridge",
        )

        # Relation: Session --[has_invocation]--> HookInvocation
        graph.add_relation(
            source=session_map[sid],
            target=hook_obj.id,
            type="has_invocation",
            actor="sentinel_bridge",
        )

        # Domain event for reactive behaviors to subscribe to
        graph.emit(ag.Event(
            id=graph.ids.event(),
            type="sentinel.hook_ingested",
            payload={
                "hook": h.get("hook", ""),
                "sentinel_event": h.get("event", ""),
                "outcome": h.get("outcome", ""),
                "session_id": sid,
                "trace_id": tid,
                "duration_us": h.get("duration_us", 0),
                "ts": h.get("ts", ""),
            },
            actor="sentinel_bridge",
        ))

        # Extra domain event for deny outcomes — useful for alerting behaviors
        if h.get("outcome") == "deny":
            graph.emit(ag.Event(
                id=graph.ids.event(),
                type="sentinel.hook_denied",
                payload={
                    "hook": h.get("hook", ""),
                    "sentinel_event": h.get("event", ""),
                    "session_id": sid,
                    "trace_id": tid,
                    "ts": h.get("ts", ""),
                },
                actor="sentinel_bridge",
            ))

    return added


# ── runtime builder ───────────────────────────────────────────────────────────

def _build_fresh_runtime(store_path: Path) -> tuple[ag.Runtime, ag.Graph]:
    """Create a fresh activegraph Runtime backed by a new SQLite store."""
    store_path.parent.mkdir(parents=True, exist_ok=True)
    graph = ag.Graph()
    rt = ag.Runtime(graph=graph, persist_to=f"sqlite:///{store_path}")
    return rt, graph


def _load_or_create(store_path: Path) -> tuple[ag.Runtime, ag.Graph]:
    if store_path.exists():
        rt = ag.Runtime.load(str(store_path))
        return rt, rt.graph
    return _build_fresh_runtime(store_path)


# ── modes ─────────────────────────────────────────────────────────────────────

def one_shot(store_path: Path, out_trace: bool) -> None:
    """Read all existing JSONL data, ingest into activegraph, save, report."""
    rt, graph = _load_or_create(store_path)

    sessions = _read_jsonls_merged(SESSIONS_JSONL_PATHS)
    hooks = _read_jsonls_merged(HOOK_INVOCATIONS_PATHS)

    # Build set of already-ingested trace IDs (for resumable imports)
    seen_traces: set[str] = {
        o.data.get("trace_id", "")
        for o in graph.objects(type="SentinelHookInvocation")
    }
    seen_traces.discard("")

    session_map = _ingest_sessions(graph, sessions)
    new_count = _ingest_hooks(graph, hooks, session_map, seen_traces)
    rt.save_state()

    status = rt.status()
    n_sessions = len(graph.objects(type="SentinelSession"))
    n_hooks = len(graph.objects(type="SentinelHookInvocation"))

    print(f"[sentinel-bridge] Ingested {n_sessions} sessions, {n_hooks} hook invocations ({new_count} new)")
    print(f"[sentinel-bridge] Store:  {store_path}")
    print(f"[sentinel-bridge] Run ID: {status.run_id}")

    if out_trace:
        print()
        rt.print_trace()

    print()
    print("─── Inspect commands ───────────────────────────────────────────")
    print(f"  activegraph inspect {store_path}")
    print(f"  activegraph export-trace {store_path} --run-id {status.run_id} --format jsonl")
    print(f"  activegraph export-trace {store_path} --run-id {status.run_id}")
    print()

    # Quick summary: by-hook breakdown
    invocations = graph.objects(type="SentinelHookInvocation")
    hook_counts: dict[str, int] = {}
    outcome_counts: dict[str, int] = {}
    event_counts: dict[str, int] = {}
    for inv in invocations:
        h = inv.data.get("hook", "?")
        o = inv.data.get("outcome", "?")
        e = inv.data.get("event", "?")
        hook_counts[h] = hook_counts.get(h, 0) + 1
        outcome_counts[o] = outcome_counts.get(o, 0) + 1
        event_counts[e] = event_counts.get(e, 0) + 1

    print("─── Hook invocations by lifecycle event ─────────────────────────")
    for evt_type, count in sorted(event_counts.items(), key=lambda x: -x[1]):
        print(f"  {evt_type:<30}  {count:>4}")

    print()
    print("─── Hook invocations by hook name ───────────────────────────────")
    for hook_name, count in sorted(hook_counts.items(), key=lambda x: -x[1])[:20]:
        print(f"  {hook_name:<40}  {count:>4}")

    print()
    print("─── Outcomes ────────────────────────────────────────────────────")
    for outcome, count in sorted(outcome_counts.items(), key=lambda x: -x[1]):
        print(f"  {outcome:<20}  {count:>4}")


def tail_mode(store_path: Path) -> None:
    """Live-tail the JSONL files and push new events as they arrive."""
    print(f"[sentinel-bridge] Tail mode — watching:")
    for p in HOOK_INVOCATIONS_PATHS:
        print(f"  {p} {'(exists)' if p.exists() else '(absent — will pick up when created)'}")
    print(f"[sentinel-bridge] Store: {store_path}")
    print("Press Ctrl+C to stop.\n")

    rt, graph = _load_or_create(store_path)

    seen_traces: set[str] = {
        o.data.get("trace_id", "")
        for o in graph.objects(type="SentinelHookInvocation")
    }
    seen_traces.discard("")

    session_map: dict[str, str] = {
        o.data["session_id"]: o.id
        for o in graph.objects(type="SentinelSession")
    }

    # Seed from existing data first (merged across all metrics dirs)
    _ingest_sessions(graph, _read_jsonls_merged(SESSIONS_JSONL_PATHS))
    _ingest_hooks(graph, _read_jsonls_merged(HOOK_INVOCATIONS_PATHS), session_map, seen_traces)
    rt.save_state()

    # Track file offsets per path
    hook_offsets: dict[Path, int] = {p: (p.stat().st_size if p.exists() else 0) for p in HOOK_INVOCATIONS_PATHS}
    sess_offsets: dict[Path, int] = {p: (p.stat().st_size if p.exists() else 0) for p in SESSIONS_JSONL_PATHS}

    try:
        while True:
            time.sleep(1)
            changed = False

            for path in SESSIONS_JSONL_PATHS:
                if not path.exists():
                    continue
                new_size = path.stat().st_size
                if new_size > sess_offsets.get(path, 0):
                    with path.open() as f:
                        f.seek(sess_offsets.get(path, 0))
                        new_lines = [json.loads(ln) for ln in f if ln.strip()]
                    sess_offsets[path] = new_size
                    _ingest_sessions(graph, new_lines)
                    changed = True

            for path in HOOK_INVOCATIONS_PATHS:
                if not path.exists():
                    continue
                new_size = path.stat().st_size
                if new_size > hook_offsets.get(path, 0):
                    with path.open() as f:
                        f.seek(hook_offsets.get(path, 0))
                        new_lines = [json.loads(ln) for ln in f if ln.strip()]
                    hook_offsets[path] = new_size
                    added = _ingest_hooks(graph, new_lines, session_map, seen_traces)
                    if added:
                        ts = datetime.now(timezone.utc).strftime("%H:%M:%S")
                        tag = "real" if ".claude-sentinel" not in str(path) else "sandbox"
                        print(f"[{ts}] +{added} hook invocations ({tag})")
                    changed = True

            if changed:
                rt.save_state()

    except KeyboardInterrupt:
        status = rt.status()
        print(f"\n[sentinel-bridge] Stopped. Run ID: {status.run_id}")
        print(f"  activegraph inspect {store_path}")


# ── entry point ───────────────────────────────────────────────────────────────

def main() -> None:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--tail", action="store_true", help="Live-tail JSONL files")
    parser.add_argument("--store", default=str(DEFAULT_STORE), help="SQLite store path")
    parser.add_argument("--out-trace", action="store_true", help="Print trace after import (one-shot mode)")
    args = parser.parse_args()

    store_path = Path(args.store)

    if args.tail:
        tail_mode(store_path)
    else:
        one_shot(store_path, args.out_trace)


if __name__ == "__main__":
    main()
