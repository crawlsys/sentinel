#!/usr/bin/env python3
"""
qwen_shim.py — translate qwen-code chat JSONLs into bridge
hook-invocation format so qwen sessions show up in the dashboard.

Qwen Code persists per-session chats to
    ~/.qwen/projects/<project-slug>/chats/<session-uuid>.jsonl

Each line is one event:
    type=user                                  → UserPromptSubmit
    type=assistant with parts[].functionCall   → PreToolUse (per fn call)
    type=tool_result with parts[].functionResponse → PostToolUse
    type=system                                → skipped (provider notices)

Session id = filename uuid (also present as sessionId in every row).

Output: ~/.qwen/sentinel/metrics/hook-invocations.jsonl
(added to sentinel_bridge.py METRICS_DIRS).

State: ~/.qwen/sentinel/metrics/qwen-shim.state.json
       byte offset per file.

Usage:
    qwen_shim.py --once    one-shot backfill
    qwen_shim.py           tail (default)
"""

import argparse
import json
import re
import time
import uuid
from pathlib import Path

QWEN_PROJECTS = Path.home() / ".qwen" / "projects"
SHIM_METRICS  = Path.home() / ".qwen" / "sentinel" / "metrics"
SHIM_OUT      = SHIM_METRICS / "hook-invocations.jsonl"
SHIM_STATE    = SHIM_METRICS / "qwen-shim.state.json"

POLL_INTERVAL_S = 3.0

CHAT_NAME = re.compile(r"^(?P<uuid>[0-9a-f-]{36})\.jsonl$")


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
        "duration_us": 0,
        "source_harness": "qwen",
    }


def _translate(obj: dict, session_id: str) -> list[dict]:
    typ = obj.get("type")
    ts = obj.get("timestamp", "")
    cwd = obj.get("cwd", "/")
    out: list[dict] = []
    if typ == "user":
        out.append(_make_record(
            event="UserPromptSubmit",
            hook="qwen_shim",
            session_id=session_id,
            ts=ts,
            repo_root=cwd,
        ))
    elif typ == "assistant":
        parts = ((obj.get("message") or {}).get("parts")) or []
        for p in parts:
            fc = p.get("functionCall")
            if fc:
                tool = fc.get("name", "unknown")
                out.append(_make_record(
                    event="PreToolUse",
                    hook=f"qwen_shim_tool_{tool}",
                    session_id=session_id,
                    ts=ts,
                    repo_root=cwd,
                ))
    elif typ == "tool_result":
        parts = ((obj.get("message") or {}).get("parts")) or []
        for p in parts:
            fr = p.get("functionResponse")
            if fr:
                tool = fr.get("name", "unknown")
                out.append(_make_record(
                    event="PostToolUse",
                    hook=f"qwen_shim_tool_{tool}",
                    session_id=session_id,
                    ts=ts,
                    repo_root=cwd,
                ))
    return out


def _process_file(path: Path, offsets: dict) -> tuple[int, int]:
    m = CHAT_NAME.match(path.name)
    if not m:
        return (0, 0)
    sid = m.group("uuid")
    start = offsets.get(str(path), 0)
    emitted = 0
    new_offset = start
    try:
        with path.open() as f:
            f.seek(start)
            # Emit SessionStart synthetically when this is the first read.
            if start == 0:
                _emit(_make_record(
                    event="SessionStart",
                    hook="qwen_shim",
                    session_id=sid,
                    ts="",  # will be overwritten by first real line
                    repo_root="/",
                ))
                emitted += 1
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
                for rec in _translate(obj, sid):
                    _emit(rec)
                    emitted += 1
    except FileNotFoundError:
        return (emitted, start)
    return (emitted, new_offset)


def _discover() -> list[Path]:
    if not QWEN_PROJECTS.exists():
        return []
    out: list[Path] = []
    for p in QWEN_PROJECTS.rglob("*.jsonl"):
        if "chats" in p.parts:
            out.append(p)
    out.sort(key=lambda p: p.stat().st_mtime)
    return out


def one_shot() -> None:
    state = _load_state()
    total = 0
    for r in _discover():
        emitted, new_off = _process_file(r, state["offsets"])
        if emitted:
            state["offsets"][str(r)] = new_off
            total += emitted
            print(f"qwen-shim: {r.name} → {emitted} records")
    _save_state(state)
    print(f"qwen-shim: total {total} records emitted")


def tail() -> None:
    state = _load_state()
    print(f"qwen-shim: tail mode, polling {QWEN_PROJECTS}")
    while True:
        any_emitted = False
        for r in _discover():
            emitted, new_off = _process_file(r, state["offsets"])
            if emitted:
                state["offsets"][str(r)] = new_off
                any_emitted = True
                print(f"qwen-shim: {r.name} → {emitted} new records")
        if any_emitted:
            _save_state(state)
        time.sleep(POLL_INTERVAL_S)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--once", action="store_true")
    args = parser.parse_args()
    SHIM_METRICS.mkdir(parents=True, exist_ok=True)
    if args.once:
        one_shot()
    else:
        try:
            tail()
        except KeyboardInterrupt:
            print("\nqwen-shim: stopped")


if __name__ == "__main__":
    main()
