#!/usr/bin/env python3
"""
opencode_shim.py — translate opencode session data into bridge
hook-invocation format so opencode sessions show up in the
sentinel dashboard.

Opencode persists its state in a SQLite database at
    ~/.local/share/opencode/opencode.db

Tables we read:
    session   — one row per session (id, directory, title, model, time_*)
    message   — chat turns (id, session_id, data{role, agent, modelID})
    part      — message parts incl. tool calls
                (id, message_id, session_id, data{type=tool, callID,
                 state={status, input, output}})

We translate:
    new session row                                    → SessionStart
    message with role=user                             → UserPromptSubmit
    part with type=tool and state.status=pending       → PreToolUse
    part with type=tool and state.status=completed     → PostToolUse
    session.time_archived set                          → Stop

Output: ~/.opencode/sentinel/metrics/hook-invocations.jsonl
        (added to bridge METRICS_DIRS in sentinel_bridge.py)

State: ~/.opencode/sentinel/metrics/opencode-shim.state.json
       tracks max(time_updated) seen per table for incremental polling.

Usage:
    opencode_shim.py --once     one-shot import of full history
    opencode_shim.py            tail mode (default)
"""

import argparse
import json
import sqlite3
import sys
import time
import uuid
from pathlib import Path

OPENCODE_DB    = Path.home() / ".local" / "share" / "opencode" / "opencode.db"
SHIM_METRICS   = Path.home() / ".opencode" / "sentinel" / "metrics"
SHIM_OUT       = SHIM_METRICS / "hook-invocations.jsonl"
SHIM_STATE     = SHIM_METRICS / "opencode-shim.state.json"

POLL_INTERVAL_S = 3.0


def _load_state() -> dict:
    if SHIM_STATE.exists():
        try:
            return json.loads(SHIM_STATE.read_text())
        except json.JSONDecodeError:
            pass
    return {
        "session_last_updated": 0,
        "message_last_updated": 0,
        "part_last_updated": 0,
    }


def _save_state(state: dict) -> None:
    SHIM_METRICS.mkdir(parents=True, exist_ok=True)
    SHIM_STATE.write_text(json.dumps(state, indent=2))


def _emit(record: dict) -> None:
    SHIM_METRICS.mkdir(parents=True, exist_ok=True)
    with SHIM_OUT.open("a") as f:
        f.write(json.dumps(record) + "\n")


def _ts(ms: int) -> str:
    """Opencode times are epoch-millis. Convert to ISO-8601."""
    from datetime import datetime, timezone
    return datetime.fromtimestamp(ms / 1000, tz=timezone.utc).isoformat()


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
        "source_harness": "opencode",
    }


def _process(once: bool = False) -> tuple[int, dict]:
    """Returns (emitted_count, new_state). Reads only rows newer than state."""
    state = _load_state()
    if not OPENCODE_DB.exists():
        return (0, state)

    emitted = 0
    conn = sqlite3.connect(f"file:{OPENCODE_DB}?mode=ro", uri=True)
    try:
        cur = conn.cursor()

        # 1. New sessions → SessionStart. Also catch archived → Stop.
        cur.execute(
            "SELECT id, directory, time_created, time_updated, time_archived "
            "FROM session WHERE time_updated > ? ORDER BY time_updated",
            (state["session_last_updated"],),
        )
        max_session = state["session_last_updated"]
        for sid, directory, t_created, t_updated, t_archived in cur:
            # SessionStart on first-seen creation. We re-emit on update
            # only if archived — bridge dedupes by trace_id so net new
            # is a single event.
            if t_created > state["session_last_updated"]:
                _emit(_make_record(
                    event="SessionStart",
                    hook="opencode_shim",
                    session_id=sid,
                    ts=_ts(t_created),
                    repo_root=directory or "/",
                ))
                emitted += 1
            if t_archived and t_archived > state["session_last_updated"]:
                _emit(_make_record(
                    event="Stop",
                    hook="opencode_shim",
                    session_id=sid,
                    ts=_ts(t_archived),
                    repo_root=directory or "/",
                ))
                emitted += 1
            max_session = max(max_session, t_updated)
        state["session_last_updated"] = max_session

        # 2. New messages → UserPromptSubmit when role=user.
        # We join to session to resolve directory.
        cur.execute(
            "SELECT m.id, m.session_id, m.time_created, m.data, s.directory "
            "FROM message m LEFT JOIN session s ON s.id = m.session_id "
            "WHERE m.time_updated > ? ORDER BY m.time_updated",
            (state["message_last_updated"],),
        )
        max_msg = state["message_last_updated"]
        for mid, sid, t_created, raw, directory in cur:
            try:
                data = json.loads(raw or "{}")
            except json.JSONDecodeError:
                data = {}
            role = data.get("role")
            if role == "user":
                _emit(_make_record(
                    event="UserPromptSubmit",
                    hook="opencode_shim",
                    session_id=sid,
                    ts=_ts(t_created),
                    repo_root=directory or "/",
                ))
                emitted += 1
            # Update max — every message advances, not just user ones.
            max_msg = max(max_msg, t_created)
        state["message_last_updated"] = max_msg

        # 3. New parts → PreToolUse / PostToolUse for tool parts.
        cur.execute(
            "SELECT p.id, p.session_id, p.time_created, p.time_updated, "
            "       p.data, s.directory "
            "FROM part p LEFT JOIN session s ON s.id = p.session_id "
            "WHERE p.time_updated > ? AND p.data LIKE '%\"type\":\"tool\"%' "
            "ORDER BY p.time_updated",
            (state["part_last_updated"],),
        )
        max_part = state["part_last_updated"]
        for pid, sid, t_created, t_updated, raw, directory in cur:
            try:
                data = json.loads(raw or "{}")
            except json.JSONDecodeError:
                data = {}
            tool = data.get("tool", "unknown")
            status = ((data.get("state") or {}).get("status")) or "unknown"

            # PreToolUse: emit once on initial creation.
            if t_created > state["part_last_updated"]:
                _emit(_make_record(
                    event="PreToolUse",
                    hook=f"opencode_shim_tool_{tool}",
                    session_id=sid,
                    ts=_ts(t_created),
                    repo_root=directory or "/",
                ))
                emitted += 1
            # PostToolUse: when state transitions to completed/error.
            if status in ("completed", "error", "denied"):
                _emit(_make_record(
                    event="PostToolUse",
                    hook=f"opencode_shim_tool_{tool}",
                    session_id=sid,
                    ts=_ts(t_updated),
                    repo_root=directory or "/",
                ))
                emitted += 1
            max_part = max(max_part, t_updated)
        state["part_last_updated"] = max_part

    finally:
        conn.close()

    return (emitted, state)


def one_shot() -> None:
    emitted, state = _process(once=True)
    _save_state(state)
    print(f"opencode-shim: {emitted} records emitted; state at {SHIM_STATE}")


def tail() -> None:
    print(f"opencode-shim: tail mode, polling {OPENCODE_DB} every {POLL_INTERVAL_S}s")
    print(f"opencode-shim: output → {SHIM_OUT}")
    while True:
        emitted, state = _process()
        if emitted:
            _save_state(state)
            print(f"opencode-shim: +{emitted} records")
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
            print("\nopencode-shim: stopped")


if __name__ == "__main__":
    main()
