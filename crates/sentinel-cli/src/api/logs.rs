//! Logs API Endpoints
//!
//! GET /api/logs — JSONL log reader with filtering
//!
//! Reads 8 log files from ~/.claude/metrics/ and returns sorted, filtered entries.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{extract::Query, routing::get, Json, Router};
use serde::{Deserialize, Serialize};

use super::AppState;

/// Log file mapping: filename → category.
const LOG_FILES: &[(&str, &str)] = &[
    ("activity-log.jsonl", "activity"),
    ("hook-stats.jsonl", "hooks"),
    ("errors.jsonl", "errors"),
    ("skill-telemetry.jsonl", "skill-telemetry"),
    ("sessions.jsonl", "sessions"),
    ("context-usage.jsonl", "context"),
    ("git-hygiene.jsonl", "git-hygiene"),
    ("todo-telemetry.jsonl", "todo"),
    ("hook-debug.jsonl", "debug"),
];

const CACHE_TTL: Duration = Duration::from_secs(3);

#[derive(Clone, Serialize)]
struct LogEntry {
    #[serde(flatten)]
    data: serde_json::Value,
    #[serde(rename = "_category")]
    category: String,
    #[serde(rename = "_source")]
    source: String,
}

#[derive(Serialize)]
struct LogsResponse {
    total: usize,
    categories: HashMap<String, usize>,
    offset: usize,
    limit: usize,
    entries: Vec<LogEntry>,
}

struct LogsCache {
    entries: Option<(Vec<LogEntry>, HashMap<String, usize>)>,
    last_read: Option<Instant>,
}

static CACHE: Mutex<LogsCache> = Mutex::new(LogsCache {
    entries: None,
    last_read: None,
});

fn metrics_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("metrics")
}

fn read_jsonl_file(file_path: &PathBuf, category: &str, source: &str) -> Vec<LogEntry> {
    let Ok(content) = fs::read_to_string(file_path) else {
        return Vec::new();
    };

    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .map(|data| LogEntry {
                    data,
                    category: category.to_string(),
                    source: source.to_string(),
                })
        })
        .collect()
}

fn parse_ts(entry: &LogEntry) -> i64 {
    entry
        .data
        .get("ts")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

fn get_all_logs() -> (Vec<LogEntry>, HashMap<String, usize>) {
    let mut cache = CACHE.lock().unwrap();

    if let (Some(ref cached), Some(last)) = (&cache.entries, cache.last_read) {
        if last.elapsed() < CACHE_TTL {
            return cached.clone();
        }
    }

    let dir = metrics_dir();
    let mut all_entries = Vec::new();
    let mut categories = HashMap::new();

    for &(file, category) in LOG_FILES {
        let entries = read_jsonl_file(&dir.join(file), category, file);
        categories.insert(category.to_string(), entries.len());
        all_entries.extend(entries);
    }

    all_entries.sort_by(|a, b| parse_ts(b).cmp(&parse_ts(a)));

    let result = (all_entries, categories);
    cache.entries = Some(result.clone());
    cache.last_read = Some(Instant::now());
    result
}

#[derive(Deserialize)]
struct LogsQuery {
    category: Option<String>,
    search: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/logs", get(logs))
}

async fn logs(Query(params): Query<LogsQuery>) -> Json<LogsResponse> {
    let (all_entries, categories) = get_all_logs();

    let limit = params.limit.unwrap_or(200).min(2000);
    let offset = params.offset.unwrap_or(0);

    let filtered: Vec<LogEntry> = all_entries
        .into_iter()
        .filter(|e| {
            if let Some(ref cat) = params.category {
                if &e.category != cat {
                    return false;
                }
            }
            if let Some(ref search) = params.search {
                let search_lower = search.to_lowercase();
                let json_str = serde_json::to_string(&e.data).unwrap_or_default().to_lowercase();
                if !json_str.contains(&search_lower) {
                    return false;
                }
            }
            true
        })
        .collect();

    let total = filtered.len();
    let entries: Vec<LogEntry> = filtered.into_iter().skip(offset).take(limit).collect();

    Json(LogsResponse {
        total,
        categories,
        offset,
        limit,
        entries,
    })
}
