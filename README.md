# Sentinel

LLM-powered security monitor for Kubernetes clusters. Sentinel polls Loki for log entries, classifies them using a local LLM, and fires alerts through Alertmanager when it detects suspicious or critical activity.

## How it works

Every 60 seconds (configurable), Sentinel:

1. Queries **Loki** for log entries across your configured sources
2. Sends each batch to an **Ollama**-compatible LLM with source-specific context
3. Classifies the batch as `SAFE`, `OPERATIONAL`, `SUSPICIOUS`, or `CRITICAL`
4. Pushes alerts to **Alertmanager** with cooldown logic to prevent alert fatigue

Instead of maintaining detection rules, you describe what each service does and what threats look like in plain English. The model handles pattern matching.

## Requirements

- **Loki** — log aggregation
- **Ollama** (or any compatible API) — LLM inference
- **Alertmanager** — alert routing

## Quick start

```bash
docker run -e LOKI_URL=http://loki:3100 \
           -e OLLAMA_URL=http://ollama:11434 \
           -e OLLAMA_MODEL=qwen3:32b-q4_K_M \
           -e ALERTMANAGER_URL=http://alertmanager:9093 \
           ghcr.io/crawlsys/sentinel:latest
```

## Configuration

All configuration is via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `LOKI_URL` | `http://loki.loki.svc.cluster.local:3100` | Loki endpoint |
| `OLLAMA_URL` | `http://ollama.ollama.svc.cluster.local:11434` | Ollama endpoint |
| `OLLAMA_MODEL` | `qwen3:32b-q4_K_M` | Model to use for classification |
| `ALERTMANAGER_URL` | `http://...alertmanager:9093` | Alertmanager endpoint |
| `POLL_INTERVAL` | `60` | Seconds between poll cycles |
| `COOLDOWN_SECONDS` | `900` | Minimum seconds between alerts for the same source |
| `MAX_LINES_PER_QUERY` | `200` | Max log lines per Loki query |
| `SUSPICIOUS_ALERT_THRESHOLD` | `2` | Consecutive SUSPICIOUS verdicts before alerting |
| `MIN_LINES_THRESHOLD` | `3` | Minimum log lines to bother classifying |
| `METRICS_PORT` | `9090` | Prometheus metrics port |
| `SENTINEL_RULES_FILE` | *(none)* | Path to YAML/JSON rules file |
| `SENTINEL_RULES` | *(none)* | Inline JSON rules (env var) |
| `NEXTDNS_API_KEY` | *(none)* | NextDNS API key for DNS monitoring |
| `NEXTDNS_PROFILE` | `65ef2e` | NextDNS profile ID |
| `DNS_BLOCK_THRESHOLD` | `500` | Blocked DNS queries in 5m before alerting |

## Custom rules

Sentinel ships with generic Kubernetes infrastructure rules (ArgoCD, kube-system, cert-manager, Cloudflare tunnels). To add your own application-specific rules, create a YAML file:

```yaml
rules:
  - name: my-app
    query: '{namespace="my-app"} |~ "(?i)(unauthorized|forbidden|panic|fatal)"'
    context: >-
      My application. Watch for: auth failures, panics, unexpected errors.
    priority: high
```

Mount it into the container and set `SENTINEL_RULES_FILE=/app/rules.yaml`. See `rules.yaml.example` for a full reference.

Rules can also be passed inline via the `SENTINEL_RULES` environment variable as JSON.

## Kubernetes deployment

A Helm chart is available for deploying Sentinel to Kubernetes. The chart handles the ConfigMap for custom rules, volume mounts, and ServiceMonitor for Prometheus scraping.

## Metrics

Sentinel exposes Prometheus metrics on port 9090:

- `sentinel_poll_cycles_total` — completed poll cycles
- `sentinel_verdicts_total` — verdicts by source and classification
- `sentinel_alerts_sent_total` — alerts sent to Alertmanager
- `sentinel_alerts_suppressed_total` — alerts suppressed by cooldown
- `sentinel_ollama_duration_seconds` — LLM inference latency
- `sentinel_loki_query_duration_seconds` — Loki query latency
- `sentinel_loki_errors_total` / `sentinel_ollama_errors_total` — error counts

A Grafana dashboard JSON is included in the Helm chart.

## Model selection

Sentinel works with any Ollama-compatible model. In practice, classification accuracy improves significantly with larger models. The 32B parameter range (e.g., `qwen3:32b-q4_K_M`) provides a good balance between accuracy and inference speed.

For thinking models (e.g., `qwen3.5-opus-distilled`), Sentinel disables chain-of-thought output to ensure the verdict fits within the response.

## License

MIT
