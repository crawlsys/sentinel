#!/usr/bin/env python3
"""Sentinel - LLM-powered SIEM for Kubernetes homelab."""

import json
import logging
import os
import time
from collections import deque
from datetime import datetime, timezone
from threading import Thread

import requests
from prometheus_client import (
    Counter,
    Gauge,
    Histogram,
    Info,
    start_http_server,
)

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(message)s",
    datefmt="%Y-%m-%dT%H:%M:%S",
)
log = logging.getLogger("sentinel")

LOKI_URL = os.environ.get("LOKI_URL", "http://loki.loki.svc.cluster.local:3100")
OLLAMA_URL = os.environ.get("OLLAMA_URL", "http://ollama.ollama.svc.cluster.local:11434")
OLLAMA_MODEL = os.environ.get("OLLAMA_MODEL", "qwen2.5:7b")
ALERTMANAGER_URL = os.environ.get("ALERTMANAGER_URL", "http://kube-prometheus-stack-alertmanager.monitoring.svc.cluster.local:9093")
POLL_INTERVAL = int(os.environ.get("POLL_INTERVAL", "60"))
MAX_LINES_PER_QUERY = int(os.environ.get("MAX_LINES_PER_QUERY", "200"))
COOLDOWN_SECONDS = int(os.environ.get("COOLDOWN_SECONDS", "300"))
METRICS_PORT = int(os.environ.get("METRICS_PORT", "9090"))
NEXTDNS_API_KEY = os.environ.get("NEXTDNS_API_KEY", "")
NEXTDNS_PROFILE = os.environ.get("NEXTDNS_PROFILE", "65ef2e")
DNS_BLOCK_THRESHOLD = int(os.environ.get("DNS_BLOCK_THRESHOLD", "500")) # Tuned up from 50

# How long to keep history for pattern detection (in seconds)
HISTORY_WINDOW = 120 * 60  # 120 minutes
# Number of consecutive OPERATIONAL verdicts before alerting
OPERATIONAL_ALERT_THRESHOLD = 5 

# --- Prometheus Metrics ---
POLL_CYCLES = Counter("sentinel_poll_cycles_total", "Total poll cycles completed")
LINES_COLLECTED = Counter(
    "sentinel_lines_collected_total", "Log lines collected from Loki", ["source"]
)
VERDICTS = Counter(
    "sentinel_verdicts_total", "Verdicts returned by LLM", ["source", "verdict"]
)
ALERTS_SENT = Counter(
    "sentinel_alerts_sent_total", "Pushover alerts sent", ["source", "verdict"]
)
ALERTS_SUPPRESSED = Counter(
    "sentinel_alerts_suppressed_total", "Alerts suppressed by cooldown/logic", ["source"]
)
LOKI_QUERY_DURATION = Histogram(
    "sentinel_loki_query_duration_seconds", "Loki query latency", ["source"],
    buckets=[0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30],
)
OLLAMA_DURATION = Histogram(
    "sentinel_ollama_duration_seconds", "Ollama inference latency", ["source"],
    buckets=[0.5, 1, 2.5, 5, 10, 30, 60, 120],
)
LOKI_ERRORS = Counter("sentinel_loki_errors_total", "Loki query failures")
OLLAMA_ERRORS = Counter("sentinel_ollama_errors_total", "Ollama inference failures")
ALERT_SEND_ERRORS = Counter("sentinel_alert_send_errors_total", "Alert send failures")
POLL_CYCLE_DURATION = Histogram(
    "sentinel_poll_cycle_duration_seconds", "Full poll cycle duration",
    buckets=[5, 10, 30, 60, 120, 300],
)
SOURCES_MONITORED = Gauge("sentinel_sources_monitored", "Number of query sources")
SENTINEL_INFO = Info("sentinel", "Sentinel build info")
DNS_BLOCKED = Gauge("sentinel_dns_blocked_total", "DNS queries blocked by NextDNS (rolling window)")
DNS_BLOCKED_BY_DEVICE = Gauge("sentinel_dns_blocked_by_device", "DNS blocked queries per device", ["device", "ip"])
DNS_TOTAL = Gauge("sentinel_dns_queries_total", "Total DNS queries (rolling window)")

QUERIES = [
    {
        "name": "argo-security",
        "query": '{namespace="argo-system"} |~ "(?i)(denied|unauthorized|forbidden|unknown.user|deleted.*app|created.*app)" !~ "(?i)(helm template|manifest.*cache|SSH_AUTH_SOCK|trigger.*not configured|unknown field|Reconciliation completed|finished call|created.*resource|Synced|Succeeded|ComparedTo|sync completed|Sync operation to)"',
        "context": "ArgoCD GitOps controller. Can deploy arbitrary workloads to the cluster. "
                   "Watch for: unauthorized access, unexpected app creation/deletion, sync failures from unknown sources, RBAC violations.",
        "priority": "critical",
    },
    {
        "name": "kube-security",
        "query": '{namespace="kube-system"} |~ "(?i)(forbidden|unauthorized|denied|401|403|failed.*auth|invalid.*token|certificate.*error)" !~ "(?i)(nodePublishSecretRef|bws-token|reconcile spc|429|Too Many Requests|rate.limit|ghcr-login-secret)"',
        "context": "Kubernetes control plane. Watch for: API auth failures, RBAC denials, "
                   "invalid tokens, certificate issues. Ignore routine CSI reconciliation errors and BWS rate limits.",
        "priority": "critical",
    },
    {
        "name": "cert-manager",
        "query": '{namespace="cert-manager"} |~ "(?i)(error|fail|denied|invalid|expired|issued|ready)"',
        "context": "TLS certificate manager. Unexpected certificate issuance could indicate "
                   "domain hijacking or MitM. Watch for: certs issued for unknown domains, failures, expiry.",
        "priority": "high",
    },
    {
        "name": "arc-runners",
        "query": '{namespace="actions-runner-system"} |~ "(?i)(error|warn|fail|register|unauthorized|denied)"',
        "context": "GitHub Actions self-hosted runner controller. Executes CI/CD workloads with cluster access. "
                   "Watch for: unauthorized runner registration, auth failures, unexpected errors.",
        "priority": "high",
    },
    {
        "name": "myapp",
        "query": '{namespace=~"myapp.*", container!~".*postgres.*|.*rabbitmq.*"} |~ "(?i)(unauthorized|forbidden|panic|fatal|injection|xss|csrf)" !~ "(?i)(relation.*does not exist|connection refused|timeout)"',
        "context": "MyApp public web application (prod + dev). Watch for: auth bypass attempts, "
                   "elevated error rates, panics, database connection failures, suspicious request patterns.",
        "priority": "high",
    },
    {
        "name": "myapp",
        "query": '{namespace="myapp", container!="crawler"} |~ "(?i)(unauthorized|forbidden|panic|fatal)" !~ "(?i)(pyroscope|session.*send|connection refused|timeout|Failed to send session|fetch failed: HTTP 403 Forbidden)"',
        "context": "MyApp search engine (public-facing). Excludes crawler noise. Watch for: "
                   "API errors, auth issues, injection attempts, database failures.",
        "priority": "high",
    },
    {
        "name": "cloudflared",
        "query": '{namespace="cloudflared"} |~ "(?i)(error|fail|disconnect|unauthorized|tunnel|reconnect)" !~ "(?i)(Unable to reach the origin service|connection refused|connect: connection refused|ERR  error=)"',
        "context": "Cloudflare tunnel - single ingress point for all public services. "
                   "Watch for: tunnel disconnections, auth failures, connection anomalies.",
        "priority": "critical",
    },
    {
        "name": "myapp-rabbitmq",
        "query": '{namespace=~"myapp.*", container=~".*rabbitmq.*"} |~ "(?i)(error|fail|refused|auth|denied)"',
        "context": "RabbitMQ message broker for MyApp. Watch for: auth failures, "
                   "connection refused, queue errors that could indicate compromise of the message bus.",
        "priority": "medium",
    },
    {
        "name": "myapp-postgres",
        "query": '{namespace=~"myapp.*", container=~".*postgres.*"} |~ "(?i)(FATAL|panic|auth.*fail|denied|password)" !~ "(?i)(relation.*does not exist|no pg_hba.conf entry)"',
        "context": "PostgreSQL database for MyApp. Watch for: auth failures, "
                   "connection storms, replication issues, data corruption indicators.",
        "priority": "high",
    },
]

SYSTEM_PROMPT = """You are a security analyst for a Kubernetes homelab. Classify log batches.

VERDICTS:
- SAFE: Normal operational noise, routine errors, or expected behavior.
- OPERATIONAL: System outages, internal errors, connection failures, or performance issues. These are NOT attacks, but the system is degraded.
- SUSPICIOUS: Potential security threats, auth failures from unusual sources, or unexpected changes.
- CRITICAL: Definite security breach, unauthorized access, or active attack.

KNOWN NOISE (always SAFE):
- CSI secrets-store "nodePublishSecretRef not found" / "bws-token" not found
- ArgoCD routine operations (sync, manifest cache, etc.)
- Routine connection timeouts or DNS resolution failures (transient)
- MyApp crawler getting 403s from news sites

OPERATIONAL ISSUES (flag as OPERATIONAL):
- Persistent database connection failures (e.g. "no such host", "connection refused")
- Application panics or fatal errors that are self-contained
- Service-to-service communication failures

SUSPICIOUS/CRITICAL (flag as SUSPICIOUS or CRITICAL):
- Auth failures from unknown IPs
- Unexpected ArgoCD app creation/deletion
- Unauthorized API access (real 401/403s)

Respond in EXACTLY this format:
VERDICT: SAFE|OPERATIONAL|SUSPICIOUS|CRITICAL
REASON: one sentence summary"""

USER_PROMPT_TEMPLATE = """Source: {name} (priority: {priority})
Context: {context}

{line_count} log entries from the last {window}s:

{logs}"""


class MemorySubsystem:
    """Simple in-memory store for recent verdicts and summaries."""
    def __init__(self, window_seconds: int):
        self.window = window_seconds
        self.history: dict[str, deque] = {}

    def add(self, source: str, verdict: str, reason: str):
        if source not in self.history:
            self.history[source] = deque()
        
        now = time.time()
        self.history[source].append({
            "ts": now,
            "verdict": verdict,
            "reason": reason
        })
        
        # Cleanup old entries
        while self.history[source] and self.history[source][0]["ts"] < (now - self.window):
            self.history[source].popleft()

    def get_recent(self, source: str, count: int = 10) -> list[dict]:
        if source not in self.history:
            return []
        return list(self.history[source])[-count:]

    def is_persistent_operational(self, source: str, threshold: int) -> bool:
        """Check if we have a persistent pattern of OPERATIONAL failures."""
        recent = self.get_recent(source, threshold)
        if len(recent) < threshold:
            return False
        return all(d["verdict"] == "OPERATIONAL" for d in recent)


# Global state
memory = MemorySubsystem(HISTORY_WINDOW)
last_alert: dict[str, float] = {}


def query_loki(query: str, start_ns: int, end_ns: int, limit: int, source: str) -> list[dict]:
    """Query Loki and return list of {stream, values} dicts."""
    params = {
        "query": query,
        "start": str(start_ns),
        "end": str(end_ns),
        "limit": str(limit),
        "direction": "backward",
    }
    try:
        with LOKI_QUERY_DURATION.labels(source=source).time():
            resp = requests.get(
                f"{LOKI_URL}/loki/api/v1/query_range",
                params=params,
                timeout=30,
            )
            resp.raise_for_status()
        data = resp.json()
        return data.get("data", {}).get("result", [])
    except Exception as e:
        log.error("Loki query failed for %s: %s", source, e)
        LOKI_ERRORS.inc()
        return []


def extract_lines(results: list[dict]) -> list[str]:
    """Flatten Loki results into a deduplicated list of log lines."""
    lines = []
    for stream in results:
        pod = stream.get("stream", {}).get("pod", "unknown")
        container = stream.get("stream", {}).get("container", "")
        prefix = f"[{pod}/{container}]" if container else f"[{pod}]"
        for _ts, line in stream.get("values", []):
            lines.append(f"{prefix} {line}")
    return lines


def analyze_with_ollama(name: str, priority: str, context: str, lines: list[str], window: int) -> tuple[str, str]:
    """Send log batch to Ollama for classification."""
    # Truncate if too many lines
    if len(lines) > 100:
        truncated = lines[:60] + [f"... ({len(lines) - 80} lines omitted) ..."] + lines[-20:]
    else:
        truncated = lines

    user_prompt = USER_PROMPT_TEMPLATE.format(
        name=name,
        priority=priority,
        context=context,
        line_count=len(lines),
        window=window,
        logs="\n".join(truncated),
    )

    try:
        with OLLAMA_DURATION.labels(source=name).time():
            resp = requests.post(
                f"{OLLAMA_URL}/api/chat",
                json={
                    "model": OLLAMA_MODEL,
                    "messages": [
                        {"role": "system", "content": SYSTEM_PROMPT},
                        {"role": "user", "content": user_prompt},
                    ],
                    "stream": False,
                    "options": {"temperature": 0.1, "num_predict": 200},
                },
                timeout=120,
            )
            resp.raise_for_status()
        content = resp.json().get("message", {}).get("content", "").strip()

        # Parse verdict
        verdict = "UNKNOWN"
        explanation = content
        lines_out = content.strip().split("\n")
        for line in lines_out:
            upper = line.upper().strip()
            for v in ("CRITICAL", "SUSPICIOUS", "OPERATIONAL", "SAFE"):
                if upper.startswith(f"VERDICT: {v}") or upper.startswith(f"VERDICT:{v}") or upper == v:
                    verdict = v
                    break
            if upper.startswith("REASON:"):
                explanation = line.split(":", 1)[1].strip()

        if verdict == "UNKNOWN":
            for v in ("CRITICAL", "SUSPICIOUS", "OPERATIONAL", "SAFE"):
                if v in content.upper():
                    verdict = v
                    break

        return verdict, explanation
    except Exception as e:
        log.error("Ollama analysis failed: %s", e)
        OLLAMA_ERRORS.inc()
        return "ERROR", str(e)


def send_to_alertmanager(source: str, verdict: str, message: str, severity: str = "warning"):
    """Push an alert to Alertmanager via its HTTP API."""
    try:
        alert = {
            "labels": {
                "alertname": f"Sentinel{verdict.title()}",
                "source": source,
                "verdict": verdict.lower(),
                "severity": severity,
                "job": "sentinel",
            },
            "annotations": {
                "summary": f"[{verdict}] {source}",
                "description": message[:2048],
            },
        }
        resp = requests.post(
            f"{ALERTMANAGER_URL}/api/v2/alerts",
            json=[alert],
            timeout=15,
        )
        resp.raise_for_status()
        log.info("Alert sent to Alertmanager: [%s] %s", verdict, source)
    except Exception as e:
        log.error("Alertmanager push failed: %s", e)
        ALERT_SEND_ERRORS.inc()


def check_nextdns():
    """Poll NextDNS analytics for DNS bypass/block activity."""
    if not NEXTDNS_API_KEY:
        return

    headers = {"X-Api-Key": NEXTDNS_API_KEY}
    base = f"https://api.nextdns.io/profiles/{NEXTDNS_PROFILE}"
    window = f"-{POLL_INTERVAL * 5}s"

    try:
        resp = requests.get(f"{base}/analytics/status?from={window}", headers=headers, timeout=15)
        resp.raise_for_status()
        for entry in resp.json().get("data", []):
            if entry["status"] == "blocked":
                DNS_BLOCKED.set(entry["queries"])
            DNS_TOTAL.inc(entry["queries"])

        resp = requests.get(f"{base}/analytics/devices?from={window}", headers=headers, timeout=15)
        resp.raise_for_status()
        for dev in resp.json().get("data", []):
            DNS_BLOCKED_BY_DEVICE.labels(device=dev.get("name") or dev.get("id", "unknown"), ip=dev.get("localIp", "unknown")).set(dev.get("queries", 0))

        # Check for bypass patterns: high blocked count
        resp = requests.get(f"{base}/analytics/status?from=-5m", headers=headers, timeout=15)
        resp.raise_for_status()
        blocked_5m = sum(e["queries"] for e in resp.json().get("data", []) if e["status"] == "blocked")

        if blocked_5m > DNS_BLOCK_THRESHOLD:
            if not check_cooldown("dns-bypass"):
                resp = requests.get(f"{base}/analytics/domains?from=-5m&status=blocked&limit=5", headers=headers, timeout=15)
                resp.raise_for_status()
                top_domains = [d["domain"] for d in resp.json().get("data", [])]

                send_to_alertmanager("dns-bypass", "SUSPICIOUS", f"{blocked_5m} blocks in 5m\nTop: {', '.join(top_domains)}")
                ALERTS_SENT.labels(source="dns-bypass", verdict="SUSPICIOUS").inc()
                last_alert["dns-bypass"] = time.time()
        else:
            log.info("[dns-bypass] %d blocks in 5m (threshold: %d)", blocked_5m, DNS_BLOCK_THRESHOLD)

        # Check for adult/porn content blocks
        ADULT_REASONS = {"parental-control:porn", "parental-control:dating", "category:porn", "category:adult"}
        resp = requests.get(f"{base}/logs?status=blocked&limit=50", headers=headers, timeout=15)
        resp.raise_for_status()
        for entry in resp.json().get("data", []):
            reasons = {r.get("id", "") for r in entry.get("reasons", [])}
            if reasons & ADULT_REASONS:
                device = entry.get("device", {})
                device_name = device.get("name", "unknown")
                domain = entry.get("domain", "unknown")
                cooldown_key = f"adult-{device_name}"
                if not check_cooldown(cooldown_key):
                    send_to_alertmanager(
                        "parental-alert", "CRITICAL",
                        f"Adult content DNS query blocked\nDevice: {device_name} ({device.get('localIp', '?')})\nDomain: {domain}",
                        severity="critical",
                    )
                    ALERTS_SENT.labels(source="parental-alert", verdict="CRITICAL").inc()
                    last_alert[cooldown_key] = time.time()
                break  # one alert per cycle is enough

    except Exception as e:
        log.error("NextDNS check failed: %s", e)


def check_cooldown(name: str) -> bool:
    """Return True if we should skip alerting (still in cooldown)."""
    last = last_alert.get(name, 0)
    return (time.time() - last) < COOLDOWN_SECONDS


def poll_cycle():
    """Run one polling cycle across all queries."""
    now_ns = int(time.time() * 1e9)
    window_ns = POLL_INTERVAL * 2 * int(1e9)
    start_ns = now_ns - window_ns

    for q in QUERIES:
        name = q["name"]
        results = query_loki(q["query"], start_ns, now_ns, MAX_LINES_PER_QUERY, source=name)
        lines = extract_lines(results)

        if not lines:
            continue

        LINES_COLLECTED.labels(source=name).inc(len(lines))
        verdict, explanation = analyze_with_ollama(name, q["priority"], q["context"], lines, POLL_INTERVAL * 2)

        VERDICTS.labels(source=name, verdict=verdict).inc()
        memory.add(name, verdict, explanation)
        
        log.info("[%s] verdict=%s: %s", name, verdict, explanation[:100])

        should_alert = False
        pushover_priority = 0

        if verdict in ("SUSPICIOUS", "CRITICAL"):
            should_alert = True
            pushover_priority = 1 if verdict == "CRITICAL" else 0
        elif verdict == "OPERATIONAL":
            # Alert only if it's persistent
            if memory.is_persistent_operational(name, OPERATIONAL_ALERT_THRESHOLD):
                # Check cooldown specifically for the persistent alert to avoid spamming
                if not check_cooldown(f"{name}-persistent"):
                    should_alert = True
                    last_alert[f"{name}-persistent"] = time.time()
                else:
                    ALERTS_SUPPRESSED.labels(source=name).inc()
            else:
                log.info("[%s] Transient OPERATIONAL issue, monitoring for persistence...", name)

        if should_alert:
            if check_cooldown(name) and verdict != "OPERATIONAL": # Persistent alerts handle their own cooldown
                ALERTS_SUPPRESSED.labels(source=name).inc()
            else:
                severity = "critical" if verdict == "CRITICAL" else "warning"
                body = f"{explanation}\n\n({len(lines)} log entries in last {POLL_INTERVAL * 2}s)"
                send_to_alertmanager(name, verdict, body, severity=severity)
                ALERTS_SENT.labels(source=name, verdict=verdict).inc()
                last_alert[name] = time.time()

    check_nextdns()
    POLL_CYCLES.inc()


def wait_for_services():
    """Block until Loki and Ollama are reachable."""
    for svc_name, url in [("Loki", f"{LOKI_URL}/ready"), ("Ollama", f"{OLLAMA_URL}/api/tags")]:
        while True:
            try:
                resp = requests.get(url, timeout=5)
                if resp.status_code < 500:
                    log.info("%s is ready", svc_name)
                    break
            except Exception:
                pass
            log.info("Waiting for %s at %s...", svc_name, url)
            time.sleep(5)


def main():
    log.info("Sentinel starting — model=%s poll=%ds cooldown=%ds", OLLAMA_MODEL, POLL_INTERVAL, COOLDOWN_SECONDS)
    SENTINEL_INFO.info({"model": OLLAMA_MODEL, "poll_interval": str(POLL_INTERVAL)})
    SOURCES_MONITORED.set(len(QUERIES))

    start_http_server(METRICS_PORT)
    wait_for_services()

    while True:
        try:
            with POLL_CYCLE_DURATION.time():
                poll_cycle()
        except Exception as e:
            log.exception("Poll cycle failed: %s", e)
        time.sleep(POLL_INTERVAL)


if __name__ == "__main__":
    main()
