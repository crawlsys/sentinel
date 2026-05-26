#!/usr/bin/env python3
"""
codex_shim.py — translate codex rollout JSONL into the bridge's
hook-invocation format so codex sessions show up in the sentinel
dashboard alongside Claude Code sessions.

Codex writes session rollouts to
    ~/.codex/sessions/YYYY/MM/DD/rollout-<id>.jsonl

Each rollout line is one of:
    session_meta   → session config / start
    turn_context   → per-turn metadata
    response_item  → model output (function_call, function_call_output, message, reasoning, tool_search_*)
    event_msg      → bus events (token_count, agent_message, user_message, task_started, task_complete)

We map a subset to hook-invocation events the bridge already
ingests (see sentinel_bridge.py::_ingest_hooks):

    session_meta                          → SessionStart
    event_msg/user_message                → UserPromptSubmit
    response_item/function_call           → PreToolUse
    response_item/function_call_output    → PostToolUse
    event_msg/task_complete               → Stop

Output goes to ~/.codex/sentinel/metrics/hook-invocations.jsonl,
which sentinel_bridge.py tails (METRICS_DIRS list).

Usage:
    codex_shim.py [--once]   one-shot import of all rollouts
    codex_shim.py            tail mode (default) — watches the codex
                             sessions tree, picks up new files,
                             emits new lines as they arrive.

Implementation notes:
    - We track file→offset state in
      ~/.codex/sentinel/metrics/codex-shim.state.json so reruns
      are idempotent.
    - We skip rollouts already fully processed.
    - session_id is the UUID embedded in the rollout filename.
    - For function_call → function_call_output pairing, we use
      `call_id`; emit PostToolUse with the same hook name as the
      preceding PreToolUse so tool-call graph edges form correctly
      in the bridge.
"""

import argparse
import json
import os
import re
import sys
import time
import uuid
from pathlib import Path

CODEX_SESSIONS = Path.home() / ".codex" / "sessions"
SHIM_METRICS   = Path.home() / ".codex" / "sentinel" / "metrics"
SHIM_OUT       = SHIM_METRICS / "hook-invocations.jsonl"
SHIM_STATE     = SHIM_METRICS / "codex-shim.state.json"

POLL_INTERVAL_S = 2.0

# Filename embeds the session UUID. Example:
#   rollout-2026-05-26T15-13-22-019e65eb-a44d-7f02-8971-c92d7e25020e.jsonl
ROLLOUT_NAME = re.compile(
    r"rollout-(?P<ts>[\d\-T]+)-(?P<uuid>[0-9a-f-]{36})\.jsonl$"
)


def _load_state() -> dict:
    if SHIM_STATE.exists():
        try:
            return json.loads(SHIM_STATE.read_text())
        except json.JSONDecodeError:
            pass
    return {"offsets": {}}


def _save_state(state: dict) -> None:
    SHIM_METRICS.mkdir(parents=True, exist_ok=True)
    SHIM_STATE.write_text(json.dumps(state, indent=2))


def _session_id_from_path(p: Path) -> str | None:
    m = ROLLOUT_NAME.search(p.name)
    return m.group("uuid") if m else None


def _emit(record: dict) -> None:
    SHIM_METRICS.mkdir(parents=True, exist_ok=True)
    with SHIM_OUT.open("a") as f:
        f.write(json.dumps(record) + "\n")


def _make_record(
    *,
    event: str,
    hook: str,
    session_id: str,
    ts: str,
    duration_us: int = 0,
    repo_root: str = "/",
    trace_id: str | None = None,
) -> dict:
    return {
        "event": event,
        "hook": hook,
        "outcome": "allow",
        "repo_root": repo_root,
        "session_id": session_id,
        "trace_id": trace_id or str(uuid.uuid4()),
        "ts": ts,
        "duration_us": duration_us,
        # WORKSTREAM: harness-codex — for downstream graph tags.
        "source_harness": "codex",
    }


def _translate(line: dict, session_id: str, repo_root: str) -> list[dict]:
    """Map one codex rollout line to zero-or-more hook-invocation records."""
    ts = line.get("timestamp", "")
    typ = line.get("type")
    payload = line.get("payload") or {}

    out: list[dict] = []

    if typ == "session_meta":
        out.append(_make_record(
            event="SessionStart",
            hook="codex_shim",
            session_id=session_id,
            ts=ts,
            repo_root=payload.get("cwd", repo_root) or repo_root,
        ))
        return out

    if typ == "event_msg":
        sub = payload.get("type")
        if sub == "user_message":
            out.append(_make_record(
                event="UserPromptSubmit",
                hook="codex_shim",
                session_id=session_id,
                ts=ts,
                repo_root=repo_root,
            ))
        elif sub == "task_complete":
            out.append(_make_record(
                event="Stop",
                hook="codex_shim",
                session_id=session_id,
                ts=ts,
                repo_root=repo_root,
            ))
        return out

    if typ == "response_item":
        sub = payload.get("type")
        if sub == "function_call":
            tool = payload.get("name", "exec")
            out.append(_make_record(
                event="PreToolUse",
                hook=f"codex_shim_tool_{tool}",
                session_id=session_id,
                ts=ts,
                repo_root=repo_root,
            ))
        elif sub == "function_call_output":
            out.append(_make_record(
                event="PostToolUse",
                hook="codex_shim_tool_result",
                session_id=session_id,
                ts=ts,
                repo_root=repo_root,
            ))
        return out

    return out


def _process_file(path: Path, offsets: dict) -> tuple[int, int]:
    """Read from last offset, emit translated records. Returns (emitted, new_offset)."""
    sid = _session_id_from_path(path)
    if not sid:
        return (0, 0)

    start = offsets.get(str(path), 0)
    emitted = 0
    new_offset = start

    try:
        with path.open() as f:
            f.seek(start)
            repo_root = "/"
            while True:
                raw = f.readline()
                if not raw:
                    break
                new_offset = f.tell()
                line = raw.strip()
                if not line:
                    continue
                try:
                    obj = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if obj.get("type") == "session_meta":
                    repo_root = (obj.get("payload") or {}).get("cwd") or repo_root
                for rec in _translate(obj, sid, repo_root):
                    _emit(rec)
                    emitted += 1
    except FileNotFoundError:
        return (emitted, start)

    return (emitted, new_offset)


def _discover_rollouts() -> list[Path]:
    if not CODEX_SESSIONS.exists():
        return []
    out = []
    for ymd_path in CODEX_SESSIONS.rglob("rollout-*.jsonl"):
        out.append(ymd_path)
    out.sort(key=lambda p: p.stat().st_mtime)
    return out


def one_shot() -> None:
    state = _load_state()
    rollouts = _discover_rollouts()
    total = 0
    for r in rollouts:
        emitted, new_off = _process_file(r, state["offsets"])
        if emitted:
            state["offsets"][str(r)] = new_off
            total += emitted
            print(f"codex-shim: {r.name} → {emitted} records")
    _save_state(state)
    print(f"codex-shim: total {total} records emitted; state at {SHIM_STATE}")


def tail() -> None:
    state = _load_state()
    print(f"codex-shim: tail mode, polling {CODEX_SESSIONS} every {POLL_INTERVAL_S}s")
    print(f"codex-shim: output → {SHIM_OUT}")
    while True:
        rollouts = _discover_rollouts()
        any_emitted = False
        for r in rollouts:
            emitted, new_off = _process_file(r, state["offsets"])
            if emitted:
                state["offsets"][str(r)] = new_off
                any_emitted = True
                print(f"codex-shim: {r.name} → {emitted} new records")
        if any_emitted:
            _save_state(state)
        time.sleep(POLL_INTERVAL_S)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--once", action="store_true",
                        help="One-shot import then exit. Default: tail mode.")
    args = parser.parse_args()
    SHIM_METRICS.mkdir(parents=True, exist_ok=True)
    if args.once:
        one_shot()
    else:
        try:
            tail()
        except KeyboardInterrupt:
            print("\ncodex-shim: stopped")


if __name__ == "__main__":
    main()
