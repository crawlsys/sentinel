#!/usr/bin/env python3
"""Sentinel/activegraph live viz.

Reads the activegraph SQLite event store (populated by sentinel_bridge.py) and
serves a single-page D3 visualisation of sessions ↔ hook invocations with
outcome-coloured nodes, per-session liveness pulses, a slide-out info panel
with transcript-derived activity rollups, and a right-rail event ticker.

Live updates push via Server-Sent Events at `/api/stream` — the server probes
MAX(seq) every 250ms and emits a full snapshot only on change. The client uses
`EventSource()` (no polling). Auto-reconnects on drop.

Run:
    python3 viz_server.py [--port 8081] [--db PATH] [--host 127.0.0.1]

Default DB:  ~/.agents/scratch/activegraph-bridge/sentinel.db

Then open http://localhost:8081/.
"""
from __future__ import annotations

import argparse
import json
import sqlite3
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

DEFAULT_DB = Path.home() / ".agents/scratch/activegraph-bridge/sentinel.db"
TRANSCRIPT_ROOTS = [
    Path.home() / ".claude/projects",
    Path.home() / ".claude-sentinel/projects",
]


def find_transcript(session_id: str):
    """Locate the conversation transcript jsonl for a session, across both Claude homes."""
    if not session_id:
        return None
    name = f"{session_id}.jsonl"
    for root in TRANSCRIPT_ROOTS:
        if not root.exists():
            continue
        for sub in root.iterdir():
            if not sub.is_dir():
                continue
            cand = sub / name
            if cand.exists():
                return cand
    return None


def _trim(s: str, n: int) -> str:
    s = s.replace("\n", " ").strip()
    return s if len(s) <= n else s[:n] + "…"


def session_activity(session_id: str, limit: int = 60, at_ts: str | None = None, window_secs: int = 30):
    """Return a compressed activity stream for a session.

    Returns both:
      - `events`: flat atomic stream (user/assistant/tool_use/tool_result)
      - `segments`: roll-up where each assistant message = one "turn" segment that
        bundles its text + tool_use calls + matched tool_results. User string
        messages become their own segments. Reduces ticker noise dramatically.

    If `at_ts` is provided, the window filter applies to BOTH events and segments.
    """
    path = find_transcript(session_id)
    if not path:
        return {"session_id": session_id, "transcript": None, "events": [], "segments": [], "at_ts": at_ts}

    rows = []
    try:
        with path.open() as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    rows.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    except OSError as e:
        return {"session_id": session_id, "transcript": str(path), "events": [], "error": str(e)}

    out = []                  # flat events (atomic)
    segments = []             # roll-ups (one per assistant turn / user input)
    tool_use_to_seg = {}      # tool_use_id → segment idx, for matching results
    tool_use_to_tc = {}       # tool_use_id → tool_call dict in that segment

    for r in rows:
        ts = r.get("timestamp") or ""
        typ = r.get("type") or ""
        if typ == "user":
            msg = r.get("message", {})
            content = msg.get("content")
            if isinstance(content, str):
                if content.startswith("<local-command-caveat>") or content.startswith("<system-reminder>") or content.startswith("Caveat:"):
                    continue
                out.append({"ts": ts, "kind": "user", "text": _trim(content, 280)})
                segments.append({
                    "ts": ts, "kind": "user_input",
                    "label": "user input",
                    "preview": _trim(content, 220),
                    "tools": [], "tool_count": 0,
                })
            elif isinstance(content, list):
                for c in content:
                    if not isinstance(c, dict) or c.get("type") != "tool_result":
                        continue
                    tu_id = c.get("tool_use_id")
                    result = c.get("content")
                    result_text = ""
                    if isinstance(result, list):
                        for sub in result:
                            if isinstance(sub, dict) and sub.get("type") == "text":
                                result_text += sub.get("text", "")
                    elif isinstance(result, str):
                        result_text = result
                    is_error = bool(c.get("is_error"))
                    if result_text:
                        out.append({"ts": ts, "kind": "tool_result", "text": _trim(result_text, 200), "is_error": is_error})
                    # Attach result to matching tool_call in the segment that owns this tool_use_id
                    if tu_id and tu_id in tool_use_to_tc:
                        tc = tool_use_to_tc[tu_id]
                        tc["result_preview"] = _trim(result_text, 180)
                        tc["result_ts"] = ts
                        tc["error"] = is_error
                        # Update segment timing
                        seg_idx = tool_use_to_seg.get(tu_id)
                        if seg_idx is not None and seg_idx < len(segments):
                            segments[seg_idx]["ts_end"] = ts
                            if is_error:
                                segments[seg_idx]["had_error"] = True
        elif typ == "assistant":
            msg = r.get("message", {})
            blocks = msg.get("content") or []
            # Build a single segment from this assistant message
            seg = {
                "ts": ts, "ts_end": ts, "kind": "assistant_turn",
                "text": "", "tools": [], "tool_calls": [], "tool_count": 0,
                "had_error": False,
            }
            for c in blocks:
                if not isinstance(c, dict):
                    continue
                if c.get("type") == "text":
                    txt = c.get("text", "").strip()
                    if txt:
                        out.append({"ts": ts, "kind": "assistant", "text": _trim(txt, 280)})
                        if seg["text"]:
                            seg["text"] += " " + txt
                        else:
                            seg["text"] = txt
                elif c.get("type") == "tool_use":
                    name = c.get("name", "")
                    inp = c.get("input", {})
                    summary = _tool_summary(name, inp)
                    out.append({"ts": ts, "kind": "tool_use", "tool": name, "text": summary})
                    tu_id = c.get("id", "")
                    tc = {"id": tu_id, "tool": name, "summary": summary}
                    seg["tools"].append(name)
                    seg["tool_calls"].append(tc)
                    seg["tool_count"] += 1
                    if tu_id:
                        tool_use_to_seg[tu_id] = len(segments)  # will be the index after append
                        tool_use_to_tc[tu_id] = tc
            # Compose label + preview
            if seg["tools"]:
                from collections import Counter as _C
                cnt = _C(seg["tools"])
                parts = []
                # Order tools by their position so the label reads chronologically (dedup-aware)
                seen = set()
                for t in seg["tools"]:
                    if t in seen:
                        continue
                    seen.add(t)
                    n = cnt[t]
                    parts.append(f"{n}× {t}" if n > 1 else t)
                seg["label"] = ", ".join(parts)
            else:
                seg["label"] = "assistant text"
            seg["preview"] = _trim(seg["text"], 220) if seg["text"] else (
                seg["tool_calls"][0]["summary"] if seg["tool_calls"] else "")
            seg["text"] = _trim(seg["text"], 600)  # keep more for expand view
            segments.append(seg)
    # Either scope to ±window around at_ts, or just tail to last `limit`
    if at_ts:
        from datetime import datetime as _dt
        def _parse(t):
            if not t:
                return None
            try:
                # Normalize Z → +00:00 first so everything is tz-aware
                t2 = t.replace("Z", "+00:00")
                # Cap nanoseconds → microseconds (fromisoformat dislikes >6 fractional digits pre-3.11)
                if "." in t2:
                    head, _, rest = t2.partition(".")
                    if "+" in rest:
                        frac, _, tz = rest.partition("+")
                        t2 = f"{head}.{frac[:6]}+{tz}"
                    elif "-" in rest:
                        frac, _, tz = rest.partition("-")
                        t2 = f"{head}.{frac[:6]}-{tz}"
                    else:
                        t2 = f"{head}.{rest[:6]}"
                return _dt.fromisoformat(t2)
            except Exception:
                return None
        anchor = _parse(at_ts)
        if anchor:
            filtered = []
            for ev in out:
                pt = _parse(ev.get("ts", ""))
                if not pt:
                    continue
                if abs((pt - anchor).total_seconds()) <= window_secs:
                    filtered.append(ev)
            seg_filtered = []
            for s in segments:
                pt = _parse(s.get("ts", ""))
                if not pt:
                    continue
                pt_end = _parse(s.get("ts_end") or s.get("ts")) or pt
                # Segment is in-window if its time range overlaps [anchor-w, anchor+w]
                if (pt - anchor).total_seconds() <= window_secs and (anchor - pt_end).total_seconds() <= window_secs:
                    seg_filtered.append(s)
            return {
                "session_id": session_id, "transcript": str(path),
                "events": filtered, "segments": seg_filtered,
                "total": len(out), "total_segments": len(segments),
                "at_ts": at_ts, "window_secs": window_secs,
            }
    return {
        "session_id": session_id, "transcript": str(path),
        "events": out[-limit:], "segments": segments[-limit:],
        "total": len(out), "total_segments": len(segments), "at_ts": at_ts,
    }


def detect_awaiting_user(session_id: str):
    """Returns (kind, question_text, options).

    kind values:
      "question"     — last pending tool_use is AskUserQuestion (structured)
      "reply"       — agent's turn finished, no pending tool_uses, transcript's
                      last entry is an assistant message (= agent at the prompt
                      waiting on free-form user reply, e.g. "want me to open
                      the worktree?"). question_text is the tail of the last
                      assistant text block; options is [].
      None          — not awaiting.

    "question" is the strong signal — there's an explicit unmatched tool_use_id.
    "reply" is heuristic — agent has finished its turn cleanly but there's no
    user message yet, so it's sitting at the prompt expecting an answer.
    """
    path = find_transcript(session_id)
    if not path:
        return (None, None, None)
    pending: dict[str, dict] = {}
    last_pending_id: str | None = None
    last_assistant_text: str = ""
    last_assistant_ts: str = ""
    last_entry_type: str = ""
    try:
        with path.open() as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    r = json.loads(line)
                except json.JSONDecodeError:
                    continue
                typ = r.get("type") or ""
                msg = r.get("message", {})
                if typ == "assistant":
                    last_entry_type = "assistant"
                    last_assistant_ts = r.get("timestamp", "")
                    # Capture the LAST text block in this assistant message (often
                    # the post-tool summary / question to the user).
                    text_blocks = [
                        c.get("text", "")
                        for c in (msg.get("content") or [])
                        if isinstance(c, dict) and c.get("type") == "text" and (c.get("text") or "").strip()
                    ]
                    if text_blocks:
                        last_assistant_text = text_blocks[-1]
                    for c in (msg.get("content") or []):
                        if isinstance(c, dict) and c.get("type") == "tool_use":
                            tu_id = c.get("id")
                            if tu_id:
                                pending[tu_id] = {
                                    "name": c.get("name", ""),
                                    "input": c.get("input", {}),
                                    "ts": r.get("timestamp", ""),
                                }
                                last_pending_id = tu_id
                elif typ == "user":
                    last_entry_type = "user"
                    content = msg.get("content")
                    if isinstance(content, list):
                        for c in content:
                            if isinstance(c, dict) and c.get("type") == "tool_result":
                                tu_id = c.get("tool_use_id")
                                if tu_id and tu_id in pending:
                                    pending.pop(tu_id, None)
                                    if last_pending_id == tu_id:
                                        last_pending_id = None
    except OSError:
        return (None, None, None)

    # 1) Structured AskUserQuestion still pending
    if last_pending_id and last_pending_id in pending:
        p = pending[last_pending_id]
        if p["name"] == "AskUserQuestion":
            inp = p.get("input") or {}
            qs = inp.get("questions") or []
            if qs and isinstance(qs[0], dict):
                q0 = qs[0]
                return ("question", _trim(q0.get("question", ""), 600), q0.get("options") or [])
            return ("question", None, [])
        # A non-question tool is pending — that's "agent is working", not "waiting".
        return (None, None, None)

    # 2) Agent finished its turn cleanly. If the LAST entry in the transcript
    #    is an assistant message (no user reply yet), the session is sitting
    #    at the prompt awaiting free-form input. The agent's last text is the
    #    most useful "what is it waiting on?" snippet.
    if last_entry_type == "assistant" and last_assistant_text:
        return ("reply", _trim(last_assistant_text, 600), [])

    return (None, None, None)


def _tool_summary(name: str, inp) -> str:
    """One-liner summarising a tool_use call. No external model — pure heuristics."""
    if not isinstance(inp, dict):
        return _trim(str(inp), 200)
    if name == "Bash":
        cmd = inp.get("command", "")
        return _trim(cmd, 200)
    if name == "Read":
        return _trim(inp.get("file_path", ""), 200)
    if name == "Edit":
        return f"{_trim(inp.get('file_path',''), 80)}  →  " + _trim(inp.get("new_string", "")[:80], 80)
    if name == "Write":
        return _trim(inp.get("file_path", ""), 200)
    if name == "Grep":
        return f"grep '{_trim(inp.get('pattern',''), 60)}' {_trim(inp.get('path','') or inp.get('glob',''), 80)}"
    if name == "Glob":
        return _trim(inp.get("pattern", ""), 200)
    if name in ("TaskCreate", "TaskUpdate"):
        return _trim(json.dumps({k: v for k, v in inp.items() if k != "metadata"}, default=str), 200)
    if name == "WebFetch":
        return _trim(inp.get("url", ""), 200)
    if name == "WebSearch":
        return _trim(inp.get("query", ""), 200)
    if name == "AskUserQuestion":
        qs = inp.get("questions") or []
        if qs and isinstance(qs[0], dict):
            return _trim(qs[0].get("question", ""), 200)
    if name == "Agent":
        return _trim(inp.get("description", "") + " · " + inp.get("subagent_type", ""), 200)
    # fallback: dump shortened JSON
    return _trim(json.dumps(inp, default=str), 200)


def peek_max_seq(db_path: Path) -> int:
    """Cheap MAX(seq) probe — single index hit, used by the SSE loop to decide
    whether a full load_graph() is needed."""
    if not db_path.exists():
        return -1
    try:
        conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        try:
            row = conn.execute("SELECT MAX(seq) FROM events").fetchone()
            return int(row[0] or 0)
        finally:
            conn.close()
    except sqlite3.Error:
        return -1


def load_graph(db_path: Path, limit: int = 100) -> dict[str, Any]:
    """Read events table, replay into a recent-window graph snapshot.

    Strategy: build full state from all events, then trim to last `limit` invocation
    nodes by their creation seq. Sessions that own any kept invocation stay; older
    invocations and orphan sessions drop. Stats below report totals (full corpus),
    plus a `window` block with what's actually rendered.
    """
    if not db_path.exists():
        return {"nodes": [], "edges": [], "events": [], "max_seq": 0, "error": f"db not found: {db_path}"}
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    cur = conn.cursor()
    nodes: dict[str, dict[str, Any]] = {}
    node_first_seq: dict[str, int] = {}
    edges_all: list[dict[str, Any]] = []
    edge_keys: set[str] = set()
    recent_events: list[dict[str, Any]] = []
    max_seq = 0
    for row in cur.execute(
        "SELECT seq, id, type, payload, timestamp FROM events ORDER BY seq ASC"
    ):
        seq = row["seq"]
        max_seq = max(max_seq, seq)
        try:
            p = json.loads(row["payload"])
        except json.JSONDecodeError:
            continue
        etype = row["type"]
        ts = row["timestamp"]
        if etype == "object.created":
            obj = p.get("object", {})
            nid = obj.get("id")
            if not nid:
                continue
            data = obj.get("data", {})
            nodes[nid] = {
                "id": nid,
                "type": obj.get("type"),
                "data": data,
                "ts": ts,
                "seq": seq,
            }
            node_first_seq[nid] = seq
        elif etype == "relation.created":
            rel = p.get("relation", {})
            src = rel.get("source")
            tgt = rel.get("target")
            if not src or not tgt:
                continue
            key = f"{src}->{tgt}:{rel.get('type')}"
            if key in edge_keys:
                continue
            edge_keys.add(key)
            edges_all.append({
                "source": src,
                "target": tgt,
                "type": rel.get("type"),
                "ts": ts,
            })
        if etype.startswith("sentinel."):
            recent_events.append({"seq": seq, "type": etype, "payload": p, "ts": ts})
    conn.close()

    # Derive chain edges: within each session, sort invocations by ts and link i → i+1.
    # trace_id is unique per hook (not per turn), so it can't be used for chaining;
    # session_id + ts order is the real chain.
    from collections import defaultdict
    by_session: dict[str, list] = defaultdict(list)
    for n in nodes.values():
        if n["type"] == "SentinelHookInvocation":
            sid = (n["data"] or {}).get("session_id")
            if sid:
                by_session[sid].append(n)
    for sid, invs in by_session.items():
        invs.sort(key=lambda n: (n["data"].get("ts") or n["ts"] or "", n["seq"]))
        for i in range(len(invs) - 1):
            a, b = invs[i], invs[i + 1]
            key = f"{a['id']}->{b['id']}:next_in_session"
            if key in edge_keys:
                continue
            edge_keys.add(key)
            edges_all.append({
                "source": a["id"],
                "target": b["id"],
                "type": "next_in_session",
                "ts": b["data"].get("ts") or b["ts"],
            })

    # Full-corpus stats
    by_type_all: dict[str, int] = {}
    by_outcome_all: dict[str, int] = {}
    for n in nodes.values():
        by_type_all[n["type"]] = by_type_all.get(n["type"], 0) + 1
        outcome = n["data"].get("outcome") if isinstance(n["data"], dict) else None
        if outcome:
            by_outcome_all[outcome] = by_outcome_all.get(outcome, 0) + 1

    # Window strategy: take the K most-recently-active sessions and include ALL their
    # invocations (up to a per-session cap). This gives proper chains per session instead
    # of scattering N invocations across many sessions.
    K_SESSIONS = 6
    PER_SESSION_CAP = 40
    # Compute max(seq) per session by looking at its invocations
    session_max_seq: dict[str, int] = {}
    inv_by_session: dict[str, list] = {}
    for n in nodes.values():
        if n["type"] == "SentinelHookInvocation":
            sid = (n["data"] or {}).get("session_id")
            if not sid:
                continue
            inv_by_session.setdefault(sid, []).append(n)
            if n["seq"] > session_max_seq.get(sid, 0):
                session_max_seq[sid] = n["seq"]
    # Top-K sessions by recency
    top_sids = [s for s, _ in sorted(session_max_seq.items(), key=lambda x: -x[1])[:K_SESSIONS]]
    kept_inv_ids: set[str] = set()
    for sid in top_sids:
        invs = sorted(inv_by_session.get(sid, []), key=lambda n: n["seq"], reverse=True)
        for n in invs[:PER_SESSION_CAP]:
            kept_inv_ids.add(n["id"])
    # Sessions owning kept invocations
    kept_session_ids: set[str] = set()
    for e in edges_all:
        if e["target"] in kept_inv_ids and e["source"].startswith("SentinelSession"):
            kept_session_ids.add(e["source"])
    kept_ids = kept_inv_ids | kept_session_ids
    kept_nodes = [n for n in nodes.values() if n["id"] in kept_ids]
    kept_edges = [e for e in edges_all if e["source"] in kept_ids and e["target"] in kept_ids]

    # Window stats
    by_type_win: dict[str, int] = {}
    by_outcome_win: dict[str, int] = {}
    for n in kept_nodes:
        by_type_win[n["type"]] = by_type_win.get(n["type"], 0) + 1
        outcome = n["data"].get("outcome") if isinstance(n["data"], dict) else None
        if outcome:
            by_outcome_win[outcome] = by_outcome_win.get(outcome, 0) + 1

    # Per-session liveness: look at transcript mtime + most recent hook ts.
    # firing  = activity in last 30s (hook OR transcript write)
    # busy    = activity in last 90s
    # idle    = activity in last 5min
    # dormant = activity in last 30min
    # dead    = no activity for >30min
    import time as _time
    from datetime import datetime as _dt
    def _ts_to_epoch(s):
        if not s: return 0.0
        try:
            t = s.replace("Z", "+00:00")
            if "." in t:
                head, _, rest = t.partition(".")
                if "+" in rest:
                    f, _, tz = rest.partition("+"); t = f"{head}.{f[:6]}+{tz}"
                elif "-" in rest:
                    f, _, tz = rest.partition("-"); t = f"{head}.{f[:6]}-{tz}"
                else:
                    t = f"{head}.{rest[:6]}"
            return _dt.fromisoformat(t).timestamp()
        except Exception:
            return 0.0
    now = _time.time()
    # Build per-session max hook ts from the FULL nodes set (not just window) for accuracy
    max_hook_ts_by_sid: dict[str, float] = {}
    for n in nodes.values():
        if n["type"] != "SentinelHookInvocation": continue
        sid = (n["data"] or {}).get("session_id")
        if not sid: continue
        ts = _ts_to_epoch((n["data"] or {}).get("ts", ""))
        if ts > max_hook_ts_by_sid.get(sid, 0):
            max_hook_ts_by_sid[sid] = ts
    # Annotate session nodes with status
    for n in kept_nodes:
        if n["type"] != "SentinelSession": continue
        sid = (n["data"] or {}).get("session_id")
        if not sid: continue
        last_hook = max_hook_ts_by_sid.get(sid, 0)
        tpath = find_transcript(sid)
        tmtime = 0.0
        if tpath and tpath.exists():
            try: tmtime = tpath.stat().st_mtime
            except OSError: pass
        last_activity = max(last_hook, tmtime)
        age = now - last_activity if last_activity else 1e9
        if age < 30:    status = "firing"
        elif age < 90:  status = "busy"
        elif age < 300: status = "idle"
        elif age < 1800: status = "dormant"
        else:           status = "dead"
        # Awaiting-user OVERRIDES every other status. Two flavours:
        #  - "question": structured AskUserQuestion (with numbered options)
        #  - "reply":    free-form ask — agent ended its turn with a question
        #                and is sitting at the prompt waiting for a text reply
        # Freshness gate: only flag awaiting if the transcript was touched in
        # the last 24h. Older "awaiting" states are abandoned sessions where
        # the user moved on rather than actually-blocking-work.
        AWAIT_FRESHNESS_SECS = 24 * 3600
        awaiting_kind, question, options = detect_awaiting_user(sid)
        if awaiting_kind and tmtime and (now - tmtime) <= AWAIT_FRESHNESS_SECS:
            status = "awaiting_user"
            n["awaiting_kind"] = awaiting_kind
            n["awaiting_question"] = question
            n["awaiting_options"] = options
        n["session_status"] = status
        n["last_activity_age_s"] = int(age) if last_activity else None

    # Ticker keeps a wider window than the graph node-window — atomic events get
    # client-side rolled-up into segments, so we ship more raw rows so the rollup
    # can cover a useful history span (~hour for moderately busy sessions).
    EVENTS_LIMIT = max(limit * 6, 600)
    return {
        "nodes": kept_nodes,
        "edges": kept_edges,
        "events": recent_events[-EVENTS_LIMIT:],
        "max_seq": max_seq,
        "window_limit": limit,
        "stats": {
            "nodes_total": len(kept_nodes),
            "edges_total": len(kept_edges),
            "by_type": by_type_win,
            "by_outcome": by_outcome_win,
            "events_total": len(recent_events[-EVENTS_LIMIT:]),
            "corpus_nodes": len(nodes),
            "corpus_edges": len(edges_all),
            "corpus_by_type": by_type_all,
            "corpus_by_outcome": by_outcome_all,
        },
    }


INDEX_HTML = r"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>sentinel · activegraph live</title>
<style>
  :root {
    --bg: #0d1117; --fg: #c9d1d9; --muted: #6e7681; --accent: #58a6ff;
    --ok: #3fb950; --deny: #f85149; --ask: #d29922; --session: #bc8cff;
    --line: #30363d;
  }
  * { box-sizing: border-box; }
  html, body { margin: 0; padding: 0; background: var(--bg); color: var(--fg);
               font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
  #grid { display: grid; grid-template-columns: 1fr 360px; height: 100vh; }
  #graph { position: relative; min-width: 0; min-height: 0; overflow: hidden; }
  svg { width: 100%; height: 100%; display: block; cursor: grab; }
  svg:active { cursor: grabbing; }
  .node circle, .node text { cursor: default; }
  .node { cursor: pointer; }
  #panel {
    position: absolute; top: 50%; right: 0; transform: translate(100%, -50%);
    width: 20vw; height: 66vh; background: #161b22; border: 1px solid var(--line);
    border-right: none; border-radius: 6px 0 0 6px; padding: 12px;
    box-shadow: -4px 0 16px #0008; transition: transform 220ms ease-out;
    overflow-y: auto; font-size: 11px; z-index: 10;
  }
  #panel.open { transform: translate(0, -50%); }
  #panel .close { float: right; cursor: pointer; color: var(--muted); font-size: 14px; padding: 0 4px; }
  #panel .close:hover { color: var(--fg); }
  #panel h3 { margin: 0 0 6px; font-size: 13px; color: var(--accent); }
  #panel .sec { margin-top: 12px; padding-top: 8px; border-top: 1px solid var(--line); }
  #panel .sec h4 { margin: 0 0 6px; font-size: 10px; color: var(--muted); text-transform: uppercase; letter-spacing: 1px; }
  #panel pre { white-space: pre-wrap; word-break: break-all; font-size: 10px; color: var(--fg); background: #0d1117; padding: 6px; border-radius: 3px; border: 1px solid var(--line); margin: 0; }
  #panel .kv { display: flex; justify-content: space-between; padding: 1px 0; }
  #panel .kv span:last-child { color: var(--accent); }
  #panel .chain { padding: 3px 6px; margin: 2px 0; background: #0d1117; border-left: 2px solid var(--line); cursor: pointer; }
  #panel .chain:hover { border-left-color: var(--accent); }
  #panel .chain.current { border-left-color: var(--accent); background: #1f6feb22; }
  #panel .act { padding: 4px 6px; margin: 3px 0; background: #0d1117; border-left: 2px solid var(--line); font-size: 10px; }
  #panel .act .label { display: inline-block; min-width: 70px; color: var(--muted); font-weight: bold; }
  #panel .act.user .label { color: var(--accent); }
  #panel .act.assistant .label { color: var(--session); }
  #panel .act.tool_use .label { color: var(--ok); }
  #panel .act.tool_result .label { color: var(--ask); }
  #panel .act .ts { color: var(--muted); font-size: 9px; float: right; }
  #panel .act code { background: #21262d; padding: 0 4px; border-radius: 2px; word-break: break-all; }
  /* Segment rollups */
  #panel .seg { padding: 6px 8px; margin: 5px 0; background: #0d1117; border-left: 3px solid var(--line); border-radius: 0 3px 3px 0; font-size: 11px; }
  #panel .seg.assistant_turn { border-left-color: var(--session); }
  #panel .seg.user_input { border-left-color: var(--accent); }
  #panel .seg.had-error { border-left-color: var(--deny); }
  #panel .seg-head { display: flex; align-items: baseline; gap: 6px; cursor: pointer; }
  #panel .seg-head .ts { color: var(--muted); font-size: 9px; white-space: nowrap; }
  #panel .seg-head .label { color: var(--ok); font-weight: bold; flex-shrink: 0; }
  #panel .seg.user_input .seg-head .label { color: var(--accent); }
  #panel .seg.assistant_turn:not(:has(.tool)) .seg-head .label { color: var(--session); font-weight: normal; }
  #panel .seg-head .caret { margin-left: auto; color: var(--muted); transition: transform 120ms; font-size: 9px; }
  #panel .seg.expanded .seg-head .caret { transform: rotate(90deg); }
  #panel .seg-preview { margin-top: 4px; color: var(--fg); font-size: 10px; line-height: 1.4; opacity: 0.9; }
  #panel .seg-details { display: none; margin-top: 6px; padding-top: 6px; border-top: 1px solid var(--line); }
  #panel .seg.expanded .seg-details { display: block; }
  #panel .seg-details .tool { padding: 3px 6px; margin: 3px 0; background: #161b22; border-left: 2px solid var(--ok); font-size: 10px; word-break: break-word; }
  #panel .seg-details .tool.error { border-left-color: var(--deny); }
  #panel .seg-details .tool .tname { color: var(--ok); font-weight: bold; margin-right: 6px; }
  #panel .seg-details .tool .result { color: var(--muted); font-size: 10px; margin-top: 3px; padding-left: 12px; border-left: 1px dashed var(--line); }
  #panel .seg-details .text-full { color: var(--fg); font-size: 10px; line-height: 1.5; margin-bottom: 6px; }
  #ticker .row { cursor: pointer; }
  #ticker .row:hover { background: #1f6feb22; }
  #ticker .row.active { background: #1f6feb44; border-left-color: var(--accent); }
  /* Grouped ticker rows */
  #ticker .row.grouped .grp-count {
    display: inline-block; min-width: 18px; padding: 0 4px; margin-right: 6px;
    border-radius: 8px; background: #21262d; color: var(--accent);
    font-size: 10px; text-align: center; font-weight: bold;
  }
  #ticker .row.grouped.has-deny .grp-count { color: var(--deny); }
  #ticker .row.grouped .grp-tools { color: var(--muted); font-size: 10px; }
  #ticker .row.grouped .grp-caret { color: var(--muted); float: right; font-size: 9px; transition: transform 120ms; }
  #ticker .row.grouped.expanded .grp-caret { transform: rotate(90deg); }
  #ticker .row.grouped .grp-members { display: none; margin-top: 4px; padding-left: 22px; border-left: 1px dashed var(--line); }
  #ticker .row.grouped.expanded .grp-members { display: block; }
  #ticker .row.grouped .grp-member { padding: 2px 0; font-size: 10px; color: var(--fg); cursor: pointer; }
  #ticker .row.grouped .grp-member:hover { color: var(--accent); }
  /* Settings widget (collapsible) */
  #settings { background: #0d1117; border: 1px solid var(--line); border-radius: 4px; margin-bottom: 12px; font-size: 11px; }
  #settings > summary { padding: 6px 10px; cursor: pointer; list-style: none; color: var(--accent); font-size: 11px; font-weight: bold; user-select: none; display: flex; align-items: center; gap: 6px; }
  #settings > summary::-webkit-details-marker { display: none; }
  #settings > summary::before { content: "▶"; font-size: 9px; color: var(--muted); transition: transform 120ms; }
  #settings[open] > summary::before { transform: rotate(90deg); }
  #settings > summary .status-inline { margin-left: auto; font-size: 10px; color: var(--muted); font-weight: normal; }
  #settings > summary .status-inline.ok { color: var(--ok); }
  #settings .settings-body { padding: 8px 10px 10px; border-top: 1px solid var(--line); }
  #settings .sec-title { font-size: 10px; color: var(--muted); text-transform: uppercase; letter-spacing: 1px; margin: 0 0 4px; padding-top: 8px; }
  #settings .sec-title:first-child { padding-top: 0; }
  #settings input[type="password"], #settings input[type="text"], #settings select {
    width: 100%; background: #161b22; color: var(--fg); border: 1px solid var(--line);
    border-radius: 3px; padding: 3px 5px; margin-bottom: 4px; font-family: inherit; font-size: 11px; box-sizing: border-box;
  }
  #settings input[type="checkbox"] { width: 12px; height: 12px; margin: 0; vertical-align: middle; cursor: pointer; }
  #settings .row { display: flex; gap: 4px; align-items: center; }
  #settings button { background: var(--accent); color: #fff; border: 0; border-radius: 3px; padding: 3px 8px; cursor: pointer; font-size: 11px; }
  #settings button:hover { filter: brightness(1.15); }
  #settings button.secondary { background: #21262d; }
  #settings label.opt { display: flex; align-items: center; gap: 6px; padding: 3px 0; cursor: pointer; font-size: 11px; color: var(--fg); }
  #settings label.opt .hint { font-size: 9px; color: var(--muted); }
  #settings .status { font-size: 10px; color: var(--muted); margin-top: 6px; }
  #settings .status.ok { color: var(--ok); }
  #settings .status.err { color: var(--deny); }
  /* Summary block inside panel */
  #panel .summary { padding: 8px; background: linear-gradient(135deg, #1f6feb15, #0d1117); border-left: 2px solid var(--accent); border-radius: 0 4px 4px 0; font-size: 11px; line-height: 1.5; }
  #panel .summary.loading { color: var(--muted); font-style: italic; }
  #panel .summary.err { color: var(--deny); }
  #side { border-left: 1px solid var(--line); padding: 12px; overflow-y: auto; }
  h1 { font-size: 14px; margin: 0 0 8px; color: var(--accent); }
  h2 { font-size: 11px; margin: 14px 0 4px; color: var(--muted); text-transform: uppercase; letter-spacing: 1px; }
  .stat { display: flex; justify-content: space-between; font-size: 12px; padding: 2px 0; }
  .stat span:last-child { color: var(--accent); }
  .pill { display: inline-block; padding: 1px 6px; border-radius: 8px; font-size: 10px; margin-right: 4px; background: #21262d; color: var(--muted); }
  .pill.ok { color: var(--ok); }
  .pill.deny { color: var(--deny); }
  .pill.ask { color: var(--ask); }
  #ticker { font-size: 11px; }
  #ticker .row { padding: 4px 6px; border-left: 2px solid var(--line); margin-bottom: 3px; word-break: break-all; }
  #ticker .row.new { animation: flash 1.5s ease-out; }
  @keyframes flash { from { background: #1f6feb33; } to { background: transparent; } }
  #ticker .row .meta { color: var(--muted); font-size: 10px; }
  #legend { position: absolute; bottom: 10px; left: 10px; background: #161b22cc; padding: 6px 10px; border: 1px solid var(--line); border-radius: 4px; font-size: 11px; }
  #legend .dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 4px; vertical-align: middle; }
  #status { position: absolute; top: 10px; right: 10px; font-size: 10px; color: var(--muted); }
  .node circle { stroke: #fff2; stroke-width: 1px; cursor: pointer; transition: stroke 120ms, opacity 120ms; }
  .node text { font-size: 9px; fill: var(--fg); pointer-events: none; transition: opacity 120ms; }
  .link { stroke: var(--line); stroke-opacity: 0.6; transition: stroke 120ms, stroke-opacity 120ms, stroke-width 120ms; }
  .node.recent circle { stroke: var(--accent); stroke-width: 2px; }
  /* Liveness pulses — different intensity per session state */
  @keyframes pulse {
    0%, 100% { stroke: var(--accent); stroke-width: 2px; filter: drop-shadow(0 0 3px var(--accent)); }
    50%      { stroke: var(--accent); stroke-width: 3px; filter: drop-shadow(0 0 12px var(--accent)) drop-shadow(0 0 22px var(--accent)); }
  }
  @keyframes pulse-soft {
    0%, 100% { stroke: var(--session); stroke-width: 1.5px; filter: drop-shadow(0 0 2px var(--session)); }
    50%      { stroke: var(--session); stroke-width: 2px;   filter: drop-shadow(0 0 8px var(--session)); }
  }
  @keyframes pulse-ring {
    0%   { r: 12; opacity: 0.7; }
    100% { r: 28; opacity: 0; }
  }
  @keyframes pulse-ring-soft {
    0%   { r: 12; opacity: 0.45; }
    100% { r: 22; opacity: 0; }
  }
  /* firing: full bright pulse on any node (session OR invocation) with very recent hook */
  .node.firing circle:not(.pulse-ring) { animation: pulse 1.4s ease-in-out infinite; }
  .node.firing .pulse-ring {
    fill: none; stroke: var(--accent); stroke-width: 1.5px;
    animation: pulse-ring 1.4s ease-out infinite;
    pointer-events: none;
  }
  /* busy: session is doing work (transcript writes) but no recent hook — gentler */
  .node.busy circle:not(.pulse-ring) { animation: pulse-soft 2.6s ease-in-out infinite; }
  .node.busy .pulse-ring {
    fill: none; stroke: var(--session); stroke-width: 1px;
    animation: pulse-ring-soft 2.6s ease-out infinite;
    pointer-events: none;
  }
  /* idle: alive but no recent activity — slight outline, no pulse */
  .node.idle circle:not(.pulse-ring) { stroke: var(--session); stroke-width: 1px; opacity: 0.85; }
  /* dormant: cold but not dead — moderate fade */
  .node.dormant circle:not(.pulse-ring) { opacity: 0.55; }
  /* dead: confirmed inactive — gone dark */
  .node.dead circle:not(.pulse-ring) { opacity: 0.30; filter: grayscale(0.6); }
  .node.dead text { opacity: 0.40; }
  /* awaiting_user: distinct amber pulse — blocked on AskUserQuestion */
  @keyframes pulse-await {
    0%, 100% { stroke: var(--ask); stroke-width: 2px; filter: drop-shadow(0 0 4px var(--ask)); }
    50%      { stroke: var(--ask); stroke-width: 3px; filter: drop-shadow(0 0 14px var(--ask)) drop-shadow(0 0 24px var(--ask)); }
  }
  @keyframes pulse-ring-await {
    0%   { r: 12; opacity: 0.85; }
    100% { r: 32; opacity: 0; }
  }
  .node.awaiting_user circle:not(.pulse-ring) {
    animation: pulse-await 1.6s ease-in-out infinite;
    fill: var(--ask) !important;
  }
  .node.awaiting_user .pulse-ring {
    fill: none; stroke: var(--ask); stroke-width: 2px;
    animation: pulse-ring-await 1.6s ease-out infinite;
    pointer-events: none;
  }
  /* Waiting-on-you callout (right rail, above ticker) */
  #await-callout { display: none; margin-bottom: 12px; }
  #await-callout.shown { display: block; }
  #await-callout .await-card {
    padding: 10px; background: linear-gradient(135deg, #d2992233, #161b22);
    border: 1px solid var(--ask); border-radius: 4px;
    margin-bottom: 6px; cursor: pointer;
    box-shadow: 0 0 12px #d2992244;
  }
  #await-callout .await-card:hover { background: linear-gradient(135deg, #d2992255, #161b22); }
  #await-callout .await-head {
    color: var(--ask); font-size: 10px; font-weight: bold;
    text-transform: uppercase; letter-spacing: 1px; margin-bottom: 4px;
    display: flex; align-items: center; gap: 6px;
  }
  #await-callout .await-head .pulse-dot {
    display: inline-block; width: 8px; height: 8px; border-radius: 50%;
    background: var(--ask); animation: pulse-dot 1.4s ease-in-out infinite;
  }
  @keyframes pulse-dot {
    0%, 100% { box-shadow: 0 0 0 0 var(--ask); }
    50% { box-shadow: 0 0 0 6px transparent; }
  }
  #await-callout .await-q { font-size: 11px; color: var(--fg); line-height: 1.4; }
  #await-callout .await-opts { font-size: 10px; color: var(--muted); margin-top: 6px; }
  #await-callout .await-opts .opt { padding: 1px 0; }
  #await-callout .await-opts .opt-n { color: var(--ask); font-weight: bold; margin-right: 4px; }
  #await-callout .await-sid { font-size: 9px; color: var(--muted); margin-top: 6px; }
  /* Focus mode: distance-graded — BFS hop count drives opacity + accent stroke.
     Bright accent in 0..2 hops, fade through ~8 hops, normal beyond. Inline styles
     applied in JS (applyFocus); these are just the selected-node highlight. */
  .node.selected circle { stroke: var(--accent); stroke-width: 3px; filter: drop-shadow(0 0 5px var(--accent)); }
  .tip { position: absolute; padding: 6px 8px; background: #161b22; border: 1px solid var(--line); border-radius: 4px; font-size: 11px; pointer-events: none; opacity: 0; max-width: 320px; }
</style>
</head>
<body>
<div id="grid">
  <div id="graph">
    <svg></svg>
    <div id="legend">
      <span><span class="dot" style="background: var(--session)"></span>session</span>
      &nbsp;<span><span class="dot" style="background: var(--ok)"></span>allow</span>
      &nbsp;<span><span class="dot" style="background: var(--deny)"></span>deny</span>
      &nbsp;<span><span class="dot" style="background: var(--ask)"></span>ask</span>
      &nbsp;<span style="color:var(--muted)">· drag=pan · scroll=zoom · 2×click=reset</span>
    </div>
    <div id="status">connecting…</div>
    <div class="tip" id="tip"></div>
    <div id="panel">
      <span class="close" onclick="closePanel()">×</span>
      <div id="panel-body"></div>
    </div>
  </div>
  <div id="side">
    <h1>sentinel · activegraph</h1>
    <details id="settings">
      <summary>settings <span class="status-inline" id="ai-status"></span></summary>
      <div class="settings-body">
        <div class="sec-title">behavior</div>
        <label class="opt">
          <input type="checkbox" id="opt-auto-watch" onchange="saveAIConfig()">
          <span>auto-watch new activity <span class="hint">(jump to newest event when idle)</span></span>
        </label>
        <label class="opt">
          <input type="checkbox" id="opt-auto-summarize" onchange="saveAIConfig()">
          <span>auto-summarize on card open</span>
        </label>

        <div class="sec-title">openai key</div>
        <input type="password" id="ai-key" placeholder="sk-..." autocomplete="off">
        <div class="row">
          <select id="ai-model">
            <option value="gpt-4o-mini" selected>gpt-4o-mini</option>
            <option value="gpt-4o">gpt-4o</option>
            <option value="gpt-5.4-mini">gpt-5.4-mini</option>
            <option value="gpt-5.4">gpt-5.4</option>
          </select>
          <button onclick="saveAIConfig()">save</button>
          <button class="secondary" onclick="clearAIConfig()">clear</button>
        </div>
        <div class="status" id="ai-status-detail"></div>
      </div>
    </details>
    <div id="await-callout"></div>
    <div id="stats"></div>
    <h2>recent events</h2>
    <div id="ticker"></div>
  </div>
</div>
<script src="/static/d3.v7.min.js"></script>
<script>
const svg = d3.select("svg");
const tipEl = document.getElementById("tip");
const statusEl = document.getElementById("status");
const statsEl = document.getElementById("stats");
const tickerEl = document.getElementById("ticker");
const W = () => document.getElementById("graph").clientWidth;
const H = () => document.getElementById("graph").clientHeight;

const zoomLayer = svg.append("g").attr("class", "zoom-layer");
const gLinks = zoomLayer.append("g").attr("class", "links");
const gNodes = zoomLayer.append("g").attr("class", "nodes");

const zoom = d3.zoom()
  .scaleExtent([0.2, 4])
  .filter(event => {
    // Skip wheel-zoom if user is scrolling the right rail; otherwise allow.
    if (event.type === "wheel") return true;
    // For mousedown: only pan if click is NOT on a node (let node-drag handle nodes)
    return !event.target.closest(".node");
  })
  .on("zoom", event => zoomLayer.attr("transform", event.transform));
svg.call(zoom);
// Double-click on empty area to reset view
svg.on("dblclick.zoom", null);
svg.on("dblclick", (event) => {
  if (event.target.closest(".node")) return;
  svg.transition().duration(300).call(zoom.transform, d3.zoomIdentity);
});

const centerForce = () => d3.forceCenter(W()/2, H()/2);
const sim = d3.forceSimulation()
  .force("link", d3.forceLink().id(d => d.id).distance(l => l.type === "next_in_session" ? 30 : 80).strength(l => l.type === "next_in_session" ? 0.9 : 0.25))
  .force("charge", d3.forceManyBody().strength(-220))
  .force("center", centerForce())
  .force("x", d3.forceX(() => W()/2).strength(0.02))
  .force("y", d3.forceY(() => H()/2).strength(0.02))
  .force("collide", d3.forceCollide().radius(d => d.r + 6));

let lastSeq = -1;
let lastEventIds = new Set();
let latestGraph = null;
let selectedNodeId = null;
let selectedEventSeq = null;
const PULSE_WINDOW_SECS = 30;

// ── AI config (localStorage) ─────────────────────────────────────────────────
function loadAIConfig() {
  const key   = localStorage.getItem("sentinel_viz_openai_key") || "";
  const model = localStorage.getItem("sentinel_viz_openai_model") || "gpt-4o-mini";
  const auto  = localStorage.getItem("sentinel_viz_openai_auto") === "1";
  const watch = localStorage.getItem("sentinel_viz_auto_watch") === "1";
  document.getElementById("ai-key").value = key;
  document.getElementById("ai-model").value = model;
  document.getElementById("opt-auto-summarize").checked = auto;
  document.getElementById("opt-auto-watch").checked = watch;

  // Inline status on the summary line (always visible)
  const inline = document.getElementById("ai-status");
  const detail = document.getElementById("ai-status-detail");
  const bits = [];
  if (key) bits.push(model);
  if (auto) bits.push("auto-summary");
  if (watch) bits.push("auto-watch");
  inline.textContent = bits.length ? bits.join(" · ") : "not configured";
  inline.className = "status-inline" + (key ? " ok" : "");
  if (detail) {
    detail.textContent = key ? "openai key set" : "set an OpenAI key to enable summaries";
    detail.className = "status" + (key ? " ok" : "");
  }
}
function saveAIConfig() {
  const key   = document.getElementById("ai-key").value.trim();
  const model = document.getElementById("ai-model").value;
  const auto  = document.getElementById("opt-auto-summarize").checked;
  const watch = document.getElementById("opt-auto-watch").checked;
  if (key) localStorage.setItem("sentinel_viz_openai_key", key);
  localStorage.setItem("sentinel_viz_openai_model", model);
  localStorage.setItem("sentinel_viz_openai_auto", auto ? "1" : "0");
  localStorage.setItem("sentinel_viz_auto_watch", watch ? "1" : "0");
  loadAIConfig();
}
function clearAIConfig() {
  localStorage.removeItem("sentinel_viz_openai_key");
  document.getElementById("ai-key").value = "";
  document.getElementById("opt-auto-summarize").checked = false;
  loadAIConfig();
}
function getAIConfig() {
  return {
    key:   localStorage.getItem("sentinel_viz_openai_key") || "",
    model: localStorage.getItem("sentinel_viz_openai_model") || "gpt-4o-mini",
    auto:  localStorage.getItem("sentinel_viz_openai_auto") === "1",
    autoWatch: localStorage.getItem("sentinel_viz_auto_watch") === "1",
  };
}
function maybeAutoSummarize(sessionId, atTs) {
  const cfg = getAIConfig();
  if (!cfg.key || !cfg.auto) return;
  // Tiny debounce — wait a beat so panel-summary div is in the DOM
  setTimeout(() => {
    const el = document.getElementById("panel-summary");
    if (el) summarizeActivity(sessionId, atTs, "panel-summary");
  }, 50);
}

function summaryCacheKey(sessionId, atTs, model) {
  return `sentinel_viz_summary:${sessionId || ""}:${atTs || "session"}:${model}`;
}
function renderCachedSummary(el, cached) {
  el.className = "summary";
  el.innerHTML = cached.html + `<div style="font-size:9px;color:var(--muted);margin-top:6px">cached · <a href="#" onclick="event.preventDefault();this.dataset.refire=1;summarizeActivity('${cached.sid}','${cached.atTs||''}','${el.id}',true)" style="color:var(--muted)">regenerate</a></div>`;
}
async function summarizeActivity(sessionId, atTs, targetId, force = false) {
  const cfg = getAIConfig();
  const el = document.getElementById(targetId);
  if (!el) return;
  if (!cfg.key) {
    el.className = "summary err";
    el.textContent = "Set an OpenAI key in the right rail first.";
    return;
  }
  const ck = summaryCacheKey(sessionId, atTs, cfg.model);
  if (!force) {
    const raw = localStorage.getItem(ck);
    if (raw) {
      try {
        const cached = JSON.parse(raw);
        renderCachedSummary(el, { ...cached, sid: sessionId, atTs });
        return;
      } catch { /* fall through to regen */ }
    }
  }
  el.className = "summary loading";
  el.textContent = "summarizing…";
  let url = `/api/activity/${encodeURIComponent(sessionId)}?limit=60`;
  if (atTs) url += `&at_ts=${encodeURIComponent(atTs)}&window=45`;
  let activity;
  try {
    activity = await (await fetch(url)).json();
  } catch (e) {
    el.className = "summary err";
    el.textContent = "couldn't load activity: " + e.message;
    return;
  }
  const evs = (activity.events || []).map(ev => {
    const ts = (ev.ts || "").slice(11, 19);
    if (ev.kind === "tool_use") return `[${ts}] tool ${ev.tool}: ${ev.text}`;
    if (ev.kind === "tool_result") return `[${ts}] ↳ ${ev.text}`;
    if (ev.kind === "user") return `[${ts}] user: ${ev.text}`;
    if (ev.kind === "assistant") return `[${ts}] assistant: ${ev.text}`;
    return `[${ts}] ${ev.kind}: ${ev.text || ""}`;
  }).join("\n");
  if (!evs) {
    el.className = "summary";
    el.textContent = "no activity to summarize.";
    return;
  }
  const scope = atTs ? `the ±45s window around ${atTs}` : `the most recent activity in session ${sessionId.slice(0,8)}…`;
  const messages = [
    { role: "system", content: "You are summarizing the activity of an autonomous coding agent for an at-a-glance status panel. Be terse, concrete, and focus on what the agent decided and what state it produced. 2-4 short sentences. No preamble, no markdown headers, no bullet lists unless absolutely warranted." },
    { role: "user", content: `Summarize what the agent did in ${scope}. Activity log (chronological):\n\n${evs}` },
  ];
  try {
    const r = await fetch("https://api.openai.com/v1/chat/completions", {
      method: "POST",
      headers: { "Authorization": "Bearer " + cfg.key, "Content-Type": "application/json" },
      body: JSON.stringify({ model: cfg.model, messages, max_tokens: 220, temperature: 0.3 }),
    });
    if (!r.ok) {
      const err = await r.text();
      throw new Error(`${r.status} ${err.slice(0,160)}`);
    }
    const data = await r.json();
    const text = (data.choices?.[0]?.message?.content || "").trim();
    const html = text.replace(/</g, "&lt;").replace(/\n/g, "<br>");
    el.className = "summary";
    el.innerHTML = html + `<div style="font-size:9px;color:var(--muted);margin-top:6px">fresh · <a href="#" onclick="event.preventDefault();summarizeActivity('${sessionId}','${atTs||''}','${el.id}',true)" style="color:var(--muted)">regenerate</a></div>`;
    // Persist
    try {
      localStorage.setItem(ck, JSON.stringify({ html, ts_generated: new Date().toISOString() }));
    } catch (_) { /* localStorage full or unavailable — fine, summary is still shown */ }
  } catch (e) {
    el.className = "summary err";
    el.textContent = "openai error: " + e.message;
  }
}

function relTime(iso) {
  if (!iso) return "";
  const t = new Date(iso).getTime();
  if (isNaN(t)) return iso;
  const sec = (Date.now() - t) / 1000;
  if (sec < 1) return "now";
  if (sec < 60) return sec.toFixed(0) + "s ago";
  if (sec < 3600) return (sec/60).toFixed(0) + "m ago";
  if (sec < 86400) return (sec/3600).toFixed(1) + "h ago";
  return (sec/86400).toFixed(1) + "d ago";
}
// Wrap a relative time in a span tagged with the source ISO so it can be re-ticked.
function rtSpan(iso) {
  if (!iso) return "";
  // Escape both the attribute value AND the body (relTime may return the iso
  // verbatim if it fails to parse).
  return `<span data-relts="${escHtml(String(iso))}">${escHtml(relTime(iso))}</span>`;
}
function tickRelTimes() {
  document.querySelectorAll("[data-relts]").forEach(el => {
    el.textContent = relTime(el.dataset.relts);
  });
}
setInterval(tickRelTimes, 30 * 1000);

function openPanelForEvent(seq, manual = true) {
  if (!latestGraph) return;
  const e = latestGraph.events.find(x => x.seq === seq);
  if (!e) return;
  if (manual) noteUserInteraction();
  selectedEventSeq = seq;
  selectedNodeId = null;
  const p = e.payload || {};
  const sid = p.session_id;
  const chain = sid ? latestGraph.events.filter(x => (x.payload || {}).session_id === sid)
    .sort((a,b) => new Date((a.payload||{}).ts || a.ts) - new Date((b.payload||{}).ts || b.ts)) : [];
  document.getElementById("panel-body").innerHTML = panelHtmlFromEvent(e, chain);
  document.getElementById("panel").classList.add("open");
  document.querySelectorAll("#ticker .row").forEach(r => r.classList.toggle("active", parseInt(r.dataset.seq) === seq));
  // Find the invocation node for this event. trace_id alone isn't unique (it's
  // a correlation id shared across hooks for one user-initiated operation), so
  // also match on hook + sentinel_event + ts. Falls back to the session node.
  const invNode = latestGraph.nodes.find(n =>
    n.type === "SentinelHookInvocation"
    && (n.data || {}).trace_id === p.trace_id
    && (!p.hook || (n.data || {}).hook === p.hook)
    && (!p.sentinel_event || (n.data || {}).event === p.sentinel_event)
    && (!p.ts || (n.data || {}).ts === p.ts)
  ) || null;
  const focusId = invNode ? invNode.id : (sid ? (latestGraph.nodes.find(n => n.type === "SentinelSession" && (n.data||{}).session_id === sid) || {}).id : null);
  applyFocus(focusId);
  panToNode(focusId);
  if (sid) {
    const ts = p.ts || e.ts;
    loadActivityInto("panel-activity", sid, ts);
    maybeAutoSummarize(sid, ts);
  }
}

function openPanelForNode(nodeId, manual = true) {
  if (!latestGraph) return;
  const n = latestGraph.nodes.find(x => x.id === nodeId);
  if (!n) return;
  if (manual) noteUserInteraction();
  selectedNodeId = nodeId;
  selectedEventSeq = null;
  let chain = [];
  let sid = null;
  let nodeTs = null;
  if (n.type === "SentinelHookInvocation") {
    sid = (n.data || {}).session_id;
    nodeTs = (n.data || {}).ts;
    chain = latestGraph.nodes.filter(x => x.type === "SentinelHookInvocation" && (x.data || {}).session_id === sid)
      .sort((a,b) => new Date(a.data.ts || a.ts) - new Date(b.data.ts || b.ts));
  } else if (n.type === "SentinelSession") {
    sid = (n.data || {}).session_id;
    chain = latestGraph.nodes
      .filter(x => x.type === "SentinelHookInvocation" && (x.data || {}).session_id === sid)
      .sort((a,b) => new Date(a.data.ts || a.ts) - new Date(b.data.ts || b.ts));
  }
  document.getElementById("panel-body").innerHTML = panelHtmlFromNode(n, chain);
  document.getElementById("panel").classList.add("open");
  applyFocus(nodeId);
  panToNode(nodeId);
  // For an invocation, scope activity to its timestamp; for a session, show recent.
  if (sid) {
    loadActivityInto("panel-activity", sid, nodeTs);
    maybeAutoSummarize(sid, nodeTs);
  }
}

function panToNode(nodeId, scale = 1.6) {
  if (!nodeId) return;
  const nodeData = gNodes.selectAll(".node").data().find(d => d.id === nodeId);
  if (!nodeData || nodeData.x == null) return;
  const w = W(), h = H();
  // If the panel is open it covers the right ~20% — bias target leftward so node is centered in visible area
  const panelOpen = document.getElementById("panel").classList.contains("open");
  const targetX = panelOpen ? w * 0.40 : w * 0.5;
  const targetY = h * 0.5;
  const t = d3.zoomIdentity.translate(targetX - nodeData.x * scale, targetY - nodeData.y * scale).scale(scale);
  svg.transition().duration(450).ease(d3.easeCubicOut).call(zoom.transform, t);
}

// Focus model: only the N closest nodes (by weighted shortest-path) get visible treatment,
// everything else fades to OUT_OPACITY. Chain hops are cheap (1), session-hub hops expensive (4).
const VISIBLE_RANK_MAX = 8;   // selected + the 7 nearest neighbours stay in focus
const OUT_OPACITY = 0.12;     // hard fade for everything past VISIBLE_RANK_MAX
const EDGE_WEIGHT = { "next_in_session": 1, "has_invocation": 4 };
function edgeWeight(e) { return EDGE_WEIGHT[e.type] ?? 2; }

function applyFocus(selectedId) {
  const resetStyles = (sel) => {
    sel.style("opacity", null).style("stroke", null).style("stroke-width", null).style("stroke-opacity", null);
  };
  if (!selectedId) {
    gNodes.selectAll(".node").classed("selected", false);
    resetStyles(gNodes.selectAll(".node"));
    resetStyles(gNodes.selectAll(".node circle:not(.pulse-ring)"));
    resetStyles(gLinks.selectAll("line"));
    return;
  }
  // Weighted Dijkstra from selected — explore the whole reachable graph (cheap).
  const dist = new Map(); dist.set(selectedId, 0);
  const edges = latestGraph?.edges || [];
  const pq = [[0, selectedId]];
  while (pq.length) {
    pq.sort((a, b) => a[0] - b[0]);
    const [d, cur] = pq.shift();
    if (d > (dist.get(cur) ?? Infinity)) continue;
    for (const e of edges) {
      const s = e.source.id || e.source, t = e.target.id || e.target;
      const w = edgeWeight(e);
      if (s === cur) {
        const nd = d + w;
        if (nd < (dist.get(t) ?? Infinity)) { dist.set(t, nd); pq.push([nd, t]); }
      } else if (t === cur) {
        const nd = d + w;
        if (nd < (dist.get(s) ?? Infinity)) { dist.set(s, nd); pq.push([nd, s]); }
      }
    }
  }
  // Rank nodes by distance — selected = rank 0, next-closest = 1, etc.
  const sortedByDist = Array.from(dist.entries()).sort((a, b) => a[1] - b[1]);
  const rankOf = new Map(sortedByDist.map(([id], i) => [id, i]));

  // Opacity by rank: rank 0 full bright, eases out to ~0.25 at the visible boundary, hard fade beyond.
  const opacityForRank = r => {
    if (r === undefined) return OUT_OPACITY;
    if (r === 0) return 1.0;
    if (r >= VISIBLE_RANK_MAX) return OUT_OPACITY;
    // ease-out cubic from 1.0 to 0.25
    const t = r / VISIBLE_RANK_MAX;
    return 1.0 - 0.75 * (1 - Math.pow(1 - t, 2));
  };
  const accentForRank = r => {
    if (r === undefined || r >= VISIBLE_RANK_MAX) return null;
    if (r <= 2) return "var(--accent)";
    if (r <= 5) return "#6cb6ff";
    return null;
  };

  gNodes.selectAll(".node")
    .classed("selected", d => d.id === selectedId)
    .style("opacity", d => opacityForRank(rankOf.get(d.id)));
  gNodes.selectAll(".node circle:not(.pulse-ring)")
    .style("stroke", d => accentForRank(rankOf.get(d.id)))
    .style("stroke-width", d => {
      const r = rankOf.get(d.id);
      if (r === 0) return "3px";
      if (r !== undefined && r <= 2) return "2px";
      return null;
    });
  gLinks.selectAll("line")
    .style("opacity", d => {
      const s = d.source.id || d.source, t = d.target.id || d.target;
      const rs = rankOf.get(s), rt = rankOf.get(t);
      if (rs === undefined && rt === undefined) return OUT_OPACITY;
      // Edge is as visible as its more-faded endpoint
      const maxR = Math.max(rs ?? 99, rt ?? 99);
      return Math.max(opacityForRank(maxR), OUT_OPACITY);
    })
    .style("stroke", d => {
      const s = d.source.id || d.source, t = d.target.id || d.target;
      const maxR = Math.max(rankOf.get(s) ?? 99, rankOf.get(t) ?? 99);
      return accentForRank(maxR);
    })
    .style("stroke-width", d => {
      const s = d.source.id || d.source, t = d.target.id || d.target;
      const maxR = Math.max(rankOf.get(s) ?? 99, rankOf.get(t) ?? 99);
      if (maxR === 0) return "2.5px";
      if (maxR <= 2) return "1.8px";
      return null;
    });
}

function closePanel() {
  document.getElementById("panel").classList.remove("open");
  selectedNodeId = null;
  selectedEventSeq = null;
  document.querySelectorAll("#ticker .row.active").forEach(r => r.classList.remove("active"));
  applyFocus(null);
  // Don't immediately re-yank the view — leave a beat after close before auto-watch fires
  noteUserInteraction();
}

function escHtml(s) {
  return (s || "").replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
function renderSegment(s, idx) {
  const ts = s.ts || "";
  const tsShort = ts ? ts.slice(11, 19) : "";
  const errCls = s.had_error ? " had-error" : "";
  const toolDetails = (s.tool_calls || []).map(tc => {
    const errClass = tc.error ? " error" : "";
    const res = tc.result_preview ? `<div class="result">${escHtml(tc.result_preview)}</div>` : "";
    return `<div class="tool${errClass}"><span class="tname">${escHtml(tc.tool)}</span>${escHtml(tc.summary)}${res}</div>`;
  }).join("");
  const textFull = s.text ? `<div class="text-full">${escHtml(s.text)}</div>` : "";
  const details = (textFull || toolDetails)
    ? `<div class="seg-details">${textFull}${toolDetails}</div>` : "";
  const errMark = s.had_error ? ' <span style="color:var(--deny)">⚠</span>' : "";
  return `<div class="seg ${s.kind}${errCls}" data-seg="${idx}">
    <div class="seg-head" onclick="this.parentElement.classList.toggle('expanded')">
      <span class="ts" title="${escHtml(ts)}">${tsShort}</span>
      <span class="ts-rel" data-relts="${escHtml(ts)}" style="color:var(--muted);font-size:9px;margin-left:4px">${relTime(ts)}</span>
      <span class="label">${escHtml(s.label)}${errMark}</span>
      <span class="caret">▶</span>
    </div>
    ${s.preview ? `<div class="seg-preview">${escHtml(s.preview)}</div>` : ""}
    ${details}
  </div>`;
}

async function loadActivityInto(targetId, sessionId, atTs) {
  const el = document.getElementById(targetId);
  if (!el) return;
  el.innerHTML = '<span style="color:var(--muted)">loading…</span>';
  try {
    let url = `/api/activity/${encodeURIComponent(sessionId)}?limit=80`;
    if (atTs) url += `&at_ts=${encodeURIComponent(atTs)}&window=30`;
    const r = await fetch(url);
    if (!r.ok) throw new Error(r.statusText);
    const a = await r.json();
    const segs = a.segments || [];
    if (segs.length === 0) {
      el.innerHTML = a.transcript
        ? `<span style="color:var(--muted)">no transcript activity ${atTs ? "in ±30s of this invocation" : ""}</span>`
        : '<span style="color:var(--muted)">no transcript on disk for this session</span>';
      return;
    }
    // Newest last so it reads top→bottom chronologically
    const rows = segs.map((s, i) => renderSegment(s, i));
    const scopeNote = atTs
      ? `<div style="color:var(--muted);font-size:10px;margin-bottom:6px">${segs.length} steps in ±${a.window_secs || 30}s of invocation (session total ${a.total_segments || "?"})</div>`
      : `<div style="color:var(--muted);font-size:10px;margin-bottom:6px">last ${segs.length} of ${a.total_segments || "?"} session steps</div>`;
    el.innerHTML = scopeNote + rows.join("");
  } catch (e) {
    el.innerHTML = '<span style="color:var(--deny)">error: ' + e.message + '</span>';
  }
}

function panelHtmlFromEvent(e, chain) {
  const p = e.payload || {};
  const ts = p.ts || e.ts;
  const outcome = p.outcome || "";
  const oclass = outcome === "allow" ? "ok" : outcome === "deny" ? "deny" : outcome === "ask" ? "ask" : "";
  let h = `<h3>${escHtml(p.hook || e.type)}</h3>`;
  h += `<div class="kv"><span>event</span><span>${escHtml(p.sentinel_event || e.type)}</span></div>`;
  if (outcome) h += `<div class="kv"><span>outcome</span><span class="pill ${oclass}">${escHtml(outcome)}</span></div>`;
  h += `<div class="kv"><span>session</span><span>${escHtml((p.session_id || "").slice(0,12))}…</span></div>`;
  if (p.trace_id) h += `<div class="kv"><span>trace</span><span>${escHtml(p.trace_id.slice(0,8))}…</span></div>`;
  if (p.duration_us !== undefined) h += `<div class="kv"><span>duration</span><span>${Number(p.duration_us)|0}µs</span></div>`;
  h += `<div class="kv"><span>time</span><span>${rtSpan(ts)}</span></div>`;
  h += `<div class="kv"><span>ts</span><span style="font-size:9px">${escHtml(ts || "")}</span></div>`;
  const sumTs1 = (p.ts || e.ts || "").replace(/['"\\<>]/g, "");
  const sumSid1 = (p.session_id || "").replace(/['"\\<>]/g, "");
  h += `<div class="sec"><h4>summary <button onclick="summarizeActivity('${sumSid1}','${sumTs1}','panel-summary')" style="float:right;background:var(--accent);color:#fff;border:0;border-radius:3px;padding:1px 8px;font-size:10px;cursor:pointer">summarize</button></h4><div id="panel-summary" class="summary" style="margin-top:6px"><span style="color:var(--muted)">click summarize to call openai</span></div></div>`;
  h += `<div class="sec"><h4>session activity</h4><div id="panel-activity"></div></div>`;
  h += `<div class="sec"><h4>raw payload</h4><pre>${escHtml(JSON.stringify(p, null, 2))}</pre></div>`;
  if (chain.length > 1) {
    h += `<div class="sec"><h4>session chain (${chain.length})</h4>`;
    for (const c of chain.slice(-20)) {
      const cp = c.payload || {};
      const cls = c.seq === e.seq ? "chain current" : "chain";
      h += `<div class="${cls}" onclick="openPanelForEvent(${Number(c.seq)|0})">${escHtml(cp.hook || c.type)} <span style="color:var(--muted);font-size:10px">· ${escHtml((cp.outcome||"").slice(0,6))}</span></div>`;
    }
    h += `</div>`;
  }
  return h;
}

function panelHtmlFromNode(n, chain) {
  const d = n.data || {};
  let h = `<h3>${escHtml(n.type)}</h3>`;
  h += `<div class="kv"><span>id</span><span>${escHtml(n.id)}</span></div>`;
  if (n.type === "SentinelSession") {
    h += `<div class="kv"><span>session_id</span><span>${escHtml((d.session_id || "").slice(0,16))}…</span></div>`;
    if (d.cwd) h += `<div class="kv"><span>cwd</span><span style="font-size:9px">${escHtml(d.cwd)}</span></div>`;
    if (d.platform) h += `<div class="kv"><span>platform</span><span>${escHtml(d.platform)}</span></div>`;
    if (d.started_at) h += `<div class="kv"><span>started</span><span>${rtSpan(d.started_at)}</span></div>`;
    if (n.session_status) {
      const ageStr = n.last_activity_age_s !== null && n.last_activity_age_s !== undefined ? ` (${Number(n.last_activity_age_s)|0}s ago)` : "";
      h += `<div class="kv"><span>state</span><span>${escHtml(n.session_status)}${ageStr}</span></div>`;
    }
    if (n.session_status === "awaiting_user" && n.awaiting_question) {
      const opts = Array.isArray(n.awaiting_options) ? n.awaiting_options : [];
      const optsHtml = opts.slice(0, 6).map((o, i) => {
        const label = (o && o.label) ? o.label : (typeof o === "string" ? o : "");
        const desc = (o && o.description) ? `<div style="color:var(--muted);font-size:9px;padding-left:18px">${escHtml(o.description)}</div>` : "";
        return `<div style="padding:3px 0"><span style="color:var(--ask);font-weight:bold;margin-right:6px">${i + 1}.</span>${escHtml(label)}${desc}</div>`;
      }).join("");
      h += `<div class="sec" style="border-left:2px solid var(--ask);padding-left:8px;background:#d2992211;border-radius:0 3px 3px 0">
        <h4 style="color:var(--ask)">⏸ awaiting user input</h4>
        <div style="font-size:11px;color:var(--fg);line-height:1.4;margin-bottom:6px">${escHtml(n.awaiting_question)}</div>
        ${optsHtml || ""}
      </div>`;
    }
  } else if (n.type === "SentinelHookInvocation") {
    h += `<div class="kv"><span>hook</span><span>${escHtml(d.hook || "")}</span></div>`;
    h += `<div class="kv"><span>event</span><span>${escHtml(d.event || "")}</span></div>`;
    if (d.outcome) h += `<div class="kv"><span>outcome</span><span class="pill ${d.outcome==='allow'?'ok':d.outcome==='deny'?'deny':d.outcome==='ask'?'ask':''}">${escHtml(d.outcome)}</span></div>`;
    if (d.tool) h += `<div class="kv"><span>tool</span><span>${escHtml(d.tool)}</span></div>`;
    if (d.duration_us !== undefined) h += `<div class="kv"><span>duration</span><span>${Number(d.duration_us)|0}µs</span></div>`;
    if (d.trace_id) h += `<div class="kv"><span>trace</span><span>${escHtml(d.trace_id.slice(0,8))}…</span></div>`;
    if (d.session_id) h += `<div class="kv"><span>session</span><span>${escHtml(d.session_id.slice(0,12))}…</span></div>`;
    if (d.repo_root) h += `<div class="kv"><span>repo</span><span style="font-size:9px">${escHtml(d.repo_root.split("/").slice(-2).join("/"))}</span></div>`;
    if (d.ts) h += `<div class="kv"><span>time</span><span>${rtSpan(d.ts)}</span></div>`;
  }
  const nSid = (d.session_id || "").replace(/['"\\<>]/g, "");
  const nTs  = (d.ts || "").replace(/['"\\<>]/g, "");
  h += `<div class="sec"><h4>summary <button onclick="summarizeActivity('${nSid}','${nTs}','panel-summary')" style="float:right;background:var(--accent);color:#fff;border:0;border-radius:3px;padding:1px 8px;font-size:10px;cursor:pointer">summarize</button></h4><div id="panel-summary" class="summary" style="margin-top:6px"><span style="color:var(--muted)">click summarize to call openai</span></div></div>`;
  h += `<div class="sec"><h4>session activity</h4><div id="panel-activity"></div></div>`;
  h += `<div class="sec"><h4>raw data</h4><pre>${escHtml(JSON.stringify(d, null, 2))}</pre></div>`;
  if (chain.length > 1) {
    h += `<div class="sec"><h4>${n.type === "SentinelSession" ? "session hooks" : "session chain"} (${chain.length})</h4>`;
    for (const c of chain.slice(-20)) {
      const cd = c.data || {};
      const cls = c.id === n.id ? "chain current" : "chain";
      const safeId = (c.id || "").replace(/['"\\<>]/g, "");
      h += `<div class="${cls}" onclick="openPanelForNode('${safeId}')">${escHtml(cd.hook || c.type)} <span style="color:var(--muted);font-size:10px">· ${escHtml((cd.outcome||"").slice(0,6))} · ${rtSpan(cd.ts)}</span></div>`;
    }
    h += `</div>`;
  }
  return h;
}

function colorFor(node) {
  if (node.type === "SentinelSession") return "var(--session)";
  const o = (node.data || {}).outcome;
  if (o === "allow") return "var(--ok)";
  if (o === "deny")  return "var(--deny)";
  if (o === "ask")   return "var(--ask)";
  return "var(--muted)";
}
function radiusFor(node) {
  if (node.type === "SentinelSession") return 10;
  return 4;
}
function labelFor(node) {
  if (node.type === "SentinelSession") {
    const sid = (node.data || {}).session_id || node.id;
    return sid.length > 14 ? sid.slice(0, 14) + "…" : sid;
  }
  return (node.data || {}).hook || "";
}

const AUTO_WATCH_IDLE_MS = 10000;     // wait for the activity stream to go quiet for 10s before jumping
const USER_CLICK_COOLDOWN_MS = 8000;  // after a manual click, defer auto-jumps for 8s
let lastAutoWatchSeq = -1;
let lastUserClickMs = 0;
let autoWatchTimer = null;
function noteUserInteraction() { lastUserClickMs = Date.now(); }

function scheduleAutoWatch(g) {
  // Restart the debounce: each new max_seq during a burst pushes the jump out by another 10s.
  if (autoWatchTimer) clearTimeout(autoWatchTimer);
  autoWatchTimer = setTimeout(() => {
    autoWatchTimer = null;
    const cfg = getAIConfig();
    if (!cfg.autoWatch) return;
    // Skip if user clicked recently (regardless of whether they currently have a panel open).
    if ((Date.now() - lastUserClickMs) < USER_CLICK_COOLDOWN_MS) return;
    const events = latestGraph?.events || [];
    if (!events.length) return;
    const sorted = events.slice().sort((a, b) => {
      const ta = ((a.payload || {}).ts) || a.ts || "";
      const tb = ((b.payload || {}).ts) || b.ts || "";
      return tb.localeCompare(ta);
    });
    const newest = sorted[0];
    if (!newest) return;
    if (newest.seq === selectedEventSeq) return;  // already on the newest card
    lastAutoWatchSeq = latestGraph.max_seq;
    openPanelForEvent(newest.seq, /*manual=*/ false);
  }, AUTO_WATCH_IDLE_MS);
}

function maybeAutoWatch(g) {
  const cfg = getAIConfig();
  if (!cfg.autoWatch) return;
  if (g.max_seq <= lastAutoWatchSeq) return;
  scheduleAutoWatch(g);
}

function renderAwaitCallout(nodes) {
  const el = document.getElementById("await-callout");
  if (!el) return;
  const awaiters = (nodes || []).filter(n =>
    n.type === "SentinelSession" && n.session_status === "awaiting_user"
  );
  if (awaiters.length === 0) {
    el.classList.remove("shown");
    el.innerHTML = "";
    return;
  }
  el.classList.add("shown");
  el.innerHTML = awaiters.map(n => {
    const sid = (n.data || {}).session_id || "";
    const kind = n.awaiting_kind || "question";
    const q = n.awaiting_question || "(no text captured)";
    const opts = Array.isArray(n.awaiting_options) ? n.awaiting_options : [];
    const optsHtml = opts.slice(0, 5).map((o, i) => {
      const label = (o && o.label) ? o.label : (typeof o === "string" ? o : "");
      return `<div class="opt"><span class="opt-n">${i + 1}.</span>${escHtml(label)}</div>`;
    }).join("");
    const headLabel = kind === "reply" ? "AWAITING REPLY (FREE-FORM)" : "WAITING ON YOU";
    const safeId = (n.id || "").replace(/['"\\<>]/g, "");
    return `<div class="await-card" onclick="openPanelForNode('${safeId}')">
      <div class="await-head"><span class="pulse-dot"></span>${headLabel}</div>
      <div class="await-q">${escHtml(q)}</div>
      ${optsHtml ? `<div class="await-opts">${optsHtml}</div>` : ""}
      <div class="await-sid">session ${escHtml(sid.slice(0,12))}…</div>
    </div>`;
  }).join("");
}

function apply(g) {
  latestGraph = g;
  const w = g.window_limit || 100;
  const corpusNodes = (g.stats || {}).corpus_nodes || g.stats.nodes_total;
  const cfg = getAIConfig();
  const watchTag = cfg.autoWatch ? " · 👁 watch" : "";
  const awaitCount = (g.nodes || []).filter(n => n.type === "SentinelSession" && n.session_status === "awaiting_user").length;
  const awaitTag = awaitCount ? ` · ⏸ ${awaitCount} waiting` : "";
  statusEl.textContent = `seq ${g.max_seq} · window ${g.stats.nodes_total}/${corpusNodes} · ${g.stats.edges_total} edges · live${watchTag}${awaitTag}`;
  renderAwaitCallout(g.nodes);
  renderStats(g.stats);
  renderTicker(g.events);
  renderGraph(g.nodes, g.edges, g.max_seq > lastSeq);
  lastSeq = g.max_seq;
  // Refresh open panel content (e.g., new tool_results may have streamed in)
  if (selectedEventSeq !== null) openPanelForEvent(selectedEventSeq);
  else if (selectedNodeId !== null) openPanelForNode(selectedNodeId);
  else maybeAutoWatch(g);
}
function connectStream() {
  const es = new EventSource("/api/stream");
  es.onmessage = (ev) => {
    try { apply(JSON.parse(ev.data)); } catch (e) { console.error("parse", e); }
  };
  es.onerror = () => {
    statusEl.textContent = "reconnecting…";
    es.close();
    setTimeout(connectStream, 1500);
  };
}

function renderStats(s) {
  const lines = [];
  lines.push(`<div class="stat"><span>window nodes</span><span>${s.nodes_total} / ${s.corpus_nodes || s.nodes_total}</span></div>`);
  lines.push(`<div class="stat"><span>window edges</span><span>${s.edges_total} / ${s.corpus_edges || s.edges_total}</span></div>`);
  lines.push(`<div class="stat"><span>events shown</span><span>${s.events_total}</span></div>`);
  for (const [t, n] of Object.entries(s.by_type || {})) {
    const total = (s.corpus_by_type || {})[t] || n;
    lines.push(`<div class="stat"><span>${t}</span><span>${n} / ${total}</span></div>`);
  }
  if (Object.keys(s.by_outcome || {}).length) {
    lines.push(`<h2>outcomes (corpus)</h2>`);
    for (const [o, n] of Object.entries(s.corpus_by_outcome || s.by_outcome)) {
      const cls = o === "allow" ? "ok" : o === "deny" ? "deny" : o === "ask" ? "ask" : "";
      lines.push(`<div class="stat"><span class="pill ${cls}">${o}</span><span>${n}</span></div>`);
    }
  }
  statsEl.innerHTML = lines.join("");
}

function groupTickerEvents(events) {
  // Group consecutive ticker events that share session × sentinel_event × outcome
  // within a tight time window relative to the GROUP'S FIRST event (not the most
  // recently added — otherwise a continuous burst of multiple tool calls all
  // collapses into one giant row).
  //
  // A single PreToolUse fires up to ~10 gates in <300ms. Two distinct tool calls
  // back-to-back typically have a ≥500ms gap. So 1500ms span + a hard cap of 14
  // members per group keeps the unit-of-rollup honest.
  const sorted = events.slice().sort((a, b) => {
    const ta = ((a.payload || {}).ts) || a.ts || "";
    const tb = ((b.payload || {}).ts) || b.ts || "";
    return tb.localeCompare(ta);
  });
  const GROUP_SPAN_MS = 1500;
  const GROUP_MAX = 14;
  const groups = [];
  let current = null;
  for (const e of sorted) {
    const p = e.payload || {};
    const sig = `${p.session_id || ""}|${p.sentinel_event || ""}|${p.outcome || ""}`;
    const tms = new Date(p.ts || e.ts || 0).getTime();
    const fits = current && current.sig === sig
                 && Math.abs(current.firstTs - tms) <= GROUP_SPAN_MS
                 && current.members.length < GROUP_MAX;
    if (fits) {
      current.members.push(e);
      if (!current.hooks.includes(p.hook)) current.hooks.push(p.hook);
      if (p.outcome === "deny" || p.outcome === "block") current.hasDeny = true;
    } else {
      if (current) groups.push(current);
      current = {
        sig, firstTs: tms, members: [e], hooks: [p.hook].filter(Boolean),
        first: e, hasDeny: p.outcome === "deny" || p.outcome === "block",
      };
    }
  }
  if (current) groups.push(current);
  return groups;
}

function renderTicker(events) {
  const groups = groupTickerEvents(events).slice(0, 50);
  const rows = groups.map(g => {
    const e = g.first;
    const p = e.payload || {};
    const isNew = !lastEventIds.has(e.seq);
    const isActive = selectedEventSeq === e.seq;
    const outcome = p.outcome || "";
    const oclass = outcome === "allow" ? "ok" : outcome === "deny" ? "deny" : outcome === "ask" ? "ask" : "";
    const ts = p.ts || e.ts;
    const sev = p.sentinel_event || e.type.replace("sentinel.","");

    const tsSafe = escHtml(ts || "");
    const tsShortSafe = ts ? escHtml(ts.slice(11,19)) : "";
    const sidShort = p.session_id ? " · " + escHtml(p.session_id.slice(0,8)) : "";

    if (g.members.length === 1) {
      const durTag = p.duration_us ? " · " + (Number(p.duration_us)|0) + "µs" : "";
      return `<div class="row ${isNew ? "new" : ""} ${isActive ? "active" : ""}" data-seq="${Number(e.seq)|0}" onclick="openPanelForEvent(${Number(e.seq)|0})">
        <span class="pill ${oclass}">${escHtml(outcome || e.type.replace("sentinel.",""))}</span>
        <strong>${escHtml(p.hook || "")}</strong> ${escHtml(sev)}
        <div class="meta"><span data-relts="${tsSafe}">${relTime(ts)}</span> · ${tsShortSafe}${sidShort}${durTag}</div>
      </div>`;
    }

    // Grouped row — show count badge + collapsed hook list
    const n = g.members.length;
    const hookPreview = escHtml(g.hooks.slice(0, 3).join(", ")) + (g.hooks.length > 3 ? `, +${g.hooks.length - 3}` : "");
    const members = g.members.map(me => {
      const mp = me.payload || {};
      const moc = (mp.outcome || "") === "allow" ? "ok" : (mp.outcome === "deny" ? "deny" : (mp.outcome === "ask" ? "ask" : ""));
      const durTag = mp.duration_us ? " · " + (Number(mp.duration_us)|0) + "µs" : "";
      return `<div class="grp-member" onclick="event.stopPropagation();openPanelForEvent(${Number(me.seq)|0})">
        <span class="pill ${moc}" style="font-size:9px">${escHtml(mp.outcome || "")}</span>
        ${escHtml(mp.hook || "")}${durTag}
      </div>`;
    }).join("");

    return `<div class="row grouped ${isNew ? "new" : ""} ${g.hasDeny ? "has-deny" : ""}" onclick="openPanelForEvent(${Number(e.seq)|0})">
      <span class="grp-count">${n}</span>
      <span class="pill ${oclass}">${escHtml(outcome)}</span>
      <strong>${escHtml(sev)}</strong>
      <span class="grp-caret" onclick="event.stopPropagation();this.closest('.row').classList.toggle('expanded')">▶</span>
      <div class="grp-tools">${hookPreview}</div>
      <div class="meta"><span data-relts="${tsSafe}">${relTime(ts)}</span> · ${tsShortSafe}${sidShort}</div>
      <div class="grp-members">${members}</div>
    </div>`;
  });
  tickerEl.innerHTML = rows.join("");
  lastEventIds = new Set(events.map(e => e.seq));
}

function renderGraph(nodes, edges, hasNew) {
  // Map for object access; preserve d3-computed x/y on existing nodes.
  const oldByID = new Map();
  gNodes.selectAll(".node").each(function(d) { oldByID.set(d.id, d); });
  nodes.forEach(n => {
    n.r = radiusFor(n);
    const old = oldByID.get(n.id);
    if (old) { n.x = old.x; n.y = old.y; n.vx = old.vx; n.vy = old.vy; }
  });

  // Links: must reference node objects for d3 force
  const nodeByID = new Map(nodes.map(n => [n.id, n]));
  const links = edges
    .map(e => ({ source: nodeByID.get(e.source), target: nodeByID.get(e.target), type: e.type }))
    .filter(l => l.source && l.target);

  const link = gLinks.selectAll("line").data(links, l => l.source.id + ">" + l.target.id);
  link.exit().remove();
  link.enter().append("line").attr("class", "link").merge(link);

  // Status classes:
  //   Session nodes  → server-provided session_status (firing/busy/idle/dormant/dead)
  //   Invocation nodes → "firing" if their hook ts is within PULSE_WINDOW_SECS (30s)
  const now = Date.now();
  const STATUS_CLASSES = ["firing", "busy", "idle", "dormant", "dead"];

  const node = gNodes.selectAll(".node").data(nodes, d => d.id);
  node.exit().remove();
  const enter = node.enter().append("g").attr("class", "node");
  enter.append("circle").attr("class", "pulse-ring");
  enter.append("circle");
  enter.append("text").attr("dx", 6).attr("dy", 3);
  const merged = enter.merge(node);

  // Clear all status classes, then add the right one
  merged.each(function(d) {
    const el = this;
    STATUS_CLASSES.forEach(c => el.classList.remove(c));
    let status = null;
    if (d.type === "SentinelSession") {
      status = d.session_status || "idle";
    } else if (d.type === "SentinelHookInvocation") {
      const ts = (d.data || {}).ts;
      if (ts) {
        const age = (now - new Date(ts).getTime()) / 1000;
        if (age >= 0 && age <= PULSE_WINDOW_SECS) status = "firing";
      }
    }
    if (status) el.classList.add(status);
  });
  merged.select("circle:not(.pulse-ring)")
    .attr("r", d => d.r)
    .attr("fill", d => colorFor(d));
  merged.select(".pulse-ring").attr("r", d => d.r);
  merged.select("text").text(d => labelFor(d));
  merged.on("mouseover", (ev, d) => {
    tipEl.style.opacity = 1;
    tipEl.style.left = (ev.pageX + 12) + "px";
    tipEl.style.top  = (ev.pageY + 12) + "px";
    const tsStr = (d.data && d.data.ts) ? `<br><span style="color:var(--muted)">${rtSpan(d.data.ts)}</span>` : "";
    const hook = (d.data && d.data.hook) ? d.data.hook : d.type;
    tipEl.innerHTML = `<strong>${escHtml(hook)}</strong>${tsStr}<br><span style="color:var(--muted);font-size:10px">click for details</span>`;
  }).on("mouseout", () => { tipEl.style.opacity = 0; })
    .on("click", (ev, d) => { ev.stopPropagation(); openPanelForNode(d.id); });
  merged.call(d3.drag()
    .on("start", (ev, d) => { if (!ev.active) sim.alphaTarget(0.3).restart(); d.fx = d.x; d.fy = d.y; })
    .on("drag",  (ev, d) => { d.fx = ev.x; d.fy = ev.y; })
    .on("end",   (ev, d) => { if (!ev.active) sim.alphaTarget(0); d.fx = null; d.fy = null; }));

  sim.nodes(nodes).on("tick", () => {
    gLinks.selectAll("line")
      .attr("x1", d => d.source.x).attr("y1", d => d.source.y)
      .attr("x2", d => d.target.x).attr("y2", d => d.target.y);
    gNodes.selectAll(".node").attr("transform", d => `translate(${d.x},${d.y})`);
  });
  sim.force("link").links(links);
  if (hasNew) sim.alpha(0.5).restart();
}

loadAIConfig();
connectStream();
window.addEventListener("resize", () => sim.force("center", centerForce()).alpha(0.3).restart());
// Re-render every 5s for pulse decay even when no new events arrive
setInterval(() => { if (latestGraph) renderGraph(latestGraph.nodes, latestGraph.edges, false); }, 5000);
</script>
</body>
</html>
"""


STATIC_DIR = Path(__file__).resolve().parent / "static"
STATIC_MIME = {
    ".js":  "application/javascript",
    ".css": "text/css; charset=utf-8",
    ".png": "image/png",
    ".svg": "image/svg+xml; charset=utf-8",
    ".map": "application/json",
}


class Handler(BaseHTTPRequestHandler):
    db_path: Path = DEFAULT_DB

    def log_message(self, format, *args):  # quieter
        sys.stderr.write("[viz] " + format % args + "\n")

    def _serve_static(self, rel: str) -> bool:
        """Serve a file from tools/sentinel-viz/static/. Returns False if not found / unsafe."""
        # Guard against path traversal — only allow simple basenames.
        if "/" in rel or rel.startswith("."):
            return False
        candidate = STATIC_DIR / rel
        try:
            real = candidate.resolve()
        except OSError:
            return False
        if not real.is_file() or STATIC_DIR.resolve() not in real.parents:
            return False
        body = real.read_bytes()
        ctype = STATIC_MIME.get(real.suffix.lower(), "application/octet-stream")
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Cache-Control", "public, max-age=3600")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        return True

    def do_GET(self):
        if self.path == "/" or self.path.startswith("/index"):
            body = INDEX_HTML.encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path.startswith("/static/"):
            rel = self.path[len("/static/"):].split("?", 1)[0]
            if self._serve_static(rel):
                return
            self.send_response(404)
            self.end_headers()
            return
        if self.path.startswith("/api/graph"):
            g = load_graph(self.db_path)
            body = json.dumps(g).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path.startswith("/api/activity/"):
            # /api/activity/<session_id>?limit=N&at_ts=...&window=30
            from urllib.parse import urlparse, parse_qs
            parsed = urlparse(self.path)
            sid = parsed.path.rsplit("/", 1)[-1]
            qs = parse_qs(parsed.query)
            try:
                limit = int(qs.get("limit", ["80"])[0])
            except ValueError:
                limit = 80
            at_ts = qs.get("at_ts", [None])[0]
            try:
                window_secs = int(qs.get("window", ["30"])[0])
            except ValueError:
                window_secs = 30
            a = session_activity(sid, limit=limit, at_ts=at_ts, window_secs=window_secs)
            body = json.dumps(a).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path.startswith("/api/stream"):
            # Server-Sent Events. Pushes a full snapshot whenever max_seq changes.
            # Performance: we probe MAX(seq) (a single index hit) every tick and
            # only invoke load_graph() when the seq actually changed. With N
            # connected clients this scales as N × cheap-probe + 1 × full-load
            # per change, rather than N × full-load per tick.
            try:
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Cache-Control", "no-cache")
                self.send_header("X-Accel-Buffering", "no")
                self.end_headers()
                last_seq = -1
                while True:
                    cur_seq = peek_max_seq(self.db_path)
                    if cur_seq != last_seq:
                        g = load_graph(self.db_path)
                        last_seq = g.get("max_seq", 0)
                        payload = ("data: " + json.dumps(g) + "\n\n").encode("utf-8")
                        self.wfile.write(payload)
                        self.wfile.flush()
                    else:
                        # keep-alive comment every loop so connection doesn't idle out
                        self.wfile.write(b": ping\n\n")
                        self.wfile.flush()
                    time.sleep(0.25)
            except (BrokenPipeError, ConnectionResetError):
                return  # client disconnected
            return
        self.send_response(404)
        self.end_headers()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8081)
    ap.add_argument("--db", type=Path, default=DEFAULT_DB)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args()
    Handler.db_path = args.db
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"sentinel viz · http://{args.host}:{args.port}/  (db={args.db})", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        server.shutdown()


if __name__ == "__main__":
    main()
