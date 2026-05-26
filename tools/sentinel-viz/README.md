# sentinel-viz (legacy)

This directory used to host:
- `sentinel_bridge.py` — JSONL → SQLite ingestion daemon (Python)
- `viz_server.py` — stdlib HTTP server + D3 viz UI (Python)
- `harness-shims/*.py` — per-harness translators (codex / opencode / qwen / gemini)

All four pieces have been ported to Rust:
- **Ingestion + harness shims** → `tools/sentinel-bridge/` (`sentinel-bridge tail | backfill | shim NAME`)
- **API + UI** → `tools/sentinel-viz-api/` (Axum, port 8082) and `tools/sentinel-viz-next/` (Next.js + MUI)

The Python sources were removed when the Rust ports reached parity (4.4s
to ingest 221k hook records vs ~30s in Python, identical SQLite schema
and payload shapes). See git history for the originals.
