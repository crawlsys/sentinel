#!/usr/bin/env python3
"""
gemini_shim.py — translate gemini-cli session logs into bridge
hook-invocation format.

Gemini CLI persists per-project session logs to
    ~/.gemini/tmp/<project>/logs.json

Each file is a JSON array. Each element:
    {sessionId, messageId, type, timestamp, message}

Only `type=user` events are present today (no tool-call surface),
so this shim is intentionally narrow:

    messageId == 0  → SessionStart + UserPromptSubmit
    messageId  > 0  → UserPromptSubmit

Session id is the gemini sessionId. The project-dir slug is used
as repo_root context.

Output: ~/.gemini/sentinel/metrics/hook-invocations.jsonl
(added to sentinel_bridge.py METRICS_DIRS).

State: ~/.gemini/sentinel/metrics/gemini-shim.state.json
       tracks last-seen (file, max_messageId) per session so reruns
       don't duplicate.
"""

import argparse
import json
import time
import uuid
from pathlib import Path

GEMINI_TMP    = Path.home() / ".gemini" / "tmp"
SHIM_METRICS  = Path.home() / ".gemini" / "sentinel" / "metrics"
SHIM_OUT      = SHIM_METRICS / "hook-invocations.jsonl"
SHIM_STATE    = SHIM_METRICS / "gemini-shim.state.json"

POLL_INTERVAL_S = 5.0


def _load_state() -> dict:
    if SHIM_STATE.exists():
        try:
            return json.loads(SHIM_STATE.read_text())
        except json.JSONDecodeError:
            pass
    return {"seen": {}}  # {session_id: max_messageId}


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
) -> dict:
    return {
        "event": event,
        "hook": hook,
        "outcome": "allow",
        "repo_root": repo_root,
        "session_id": session_id,
        "trace_id": str(uuid.uuid4()),
        "ts": ts,
        "duration_us": 0,
        "source_harness": "gemini",
    }


def _process_file(path: Path, seen: dict) -> int:
    project_slug = path.parent.name
    try:
        records = json.loads(path.read_text())
    except (json.JSONDecodeError, OSError):
        return 0
    if not isinstance(records, list):
        return 0
    emitted = 0
    repo_root = f"~/{project_slug}"
    for rec in records:
        sid = rec.get("sessionId")
        mid = rec.get("messageId", 0)
        ts = rec.get("timestamp", "")
        typ = rec.get("type")
        if not sid:
            continue
        last_mid = seen.get(sid, -1)
        if mid <= last_mid:
            continue
        if mid == 0:
            _emit(_make_record(
                event="SessionStart",
                hook="gemini_shim",
                session_id=sid,
                ts=ts,
                repo_root=repo_root,
            ))
            emitted += 1
        if typ == "user":
            _emit(_make_record(
                event="UserPromptSubmit",
                hook="gemini_shim",
                session_id=sid,
                ts=ts,
                repo_root=repo_root,
            ))
            emitted += 1
        seen[sid] = mid
    return emitted


def _discover() -> list[Path]:
    if not GEMINI_TMP.exists():
        return []
    return list(GEMINI_TMP.glob("*/logs.json"))


def one_shot() -> None:
    state = _load_state()
    total = 0
    for f in _discover():
        emitted = _process_file(f, state["seen"])
        if emitted:
            total += emitted
            print(f"gemini-shim: {f.parent.name}/logs.json → {emitted} records")
    _save_state(state)
    print(f"gemini-shim: total {total} records emitted")


def tail() -> None:
    state = _load_state()
    print(f"gemini-shim: tail mode, polling {GEMINI_TMP}")
    while True:
        any_emitted = False
        for f in _discover():
            emitted = _process_file(f, state["seen"])
            if emitted:
                any_emitted = True
                print(f"gemini-shim: {f.parent.name} +{emitted}")
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
            print("\ngemini-shim: stopped")


if __name__ == "__main__":
    main()
