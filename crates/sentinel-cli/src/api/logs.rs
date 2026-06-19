//! Logs API Endpoints
//!
//! GET /api/logs — JSONL log reader with filtering
//!
//! Reads log files from ~/.claude/sentinel/metrics/ and returns sorted, filtered entries.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{extract::Query, http::StatusCode, routing::get, Json, Router};
use sentinel_infrastructure::operational_api_read_graph::OperationalApiReadSurface;
use serde::{Deserialize, Serialize};

use super::{operational_read_audit, AppState};

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

#[derive(Clone, Debug, Serialize)]
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

#[derive(Debug, Clone)]
struct LogReadError {
    source: String,
    message: String,
}

impl LogReadError {
    fn new(source: &str, message: impl Into<String>) -> Self {
        Self {
            source: source.to_string(),
            message: message.into(),
        }
    }
}

impl fmt::Display for LogReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.source, self.message)
    }
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
    sentinel_infrastructure::paths::home_root_or_fatal()
        .join(".claude")
        .join("sentinel")
        .join("metrics")
}

fn read_jsonl_file(
    file_path: &std::path::Path,
    category: &str,
    source: &str,
) -> Result<Vec<LogEntry>, LogReadError> {
    let content = match fs::read_to_string(file_path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(LogReadError::new(
                source,
                format!("failed to read {}: {error}", file_path.display()),
            ));
        }
    };

    let mut entries = Vec::new();
    for (line_index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let data = serde_json::from_str::<serde_json::Value>(line).map_err(|error| {
            LogReadError::new(
                source,
                format!("malformed JSONL at line {}: {error}", line_index + 1),
            )
        })?;
        entries.push(LogEntry {
            data,
            category: category.to_string(),
            source: source.to_string(),
        });
    }
    Ok(entries)
}

fn parse_ts(entry: &LogEntry) -> i64 {
    entry
        .data
        .get("ts")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map_or(0, |dt| dt.timestamp_millis())
}

fn get_all_logs() -> Result<(Vec<LogEntry>, HashMap<String, usize>), LogReadError> {
    let mut cache = CACHE.lock().unwrap();

    if let (Some(ref cached), Some(last)) = (&cache.entries, cache.last_read) {
        if last.elapsed() < CACHE_TTL {
            return Ok(cached.clone());
        }
    }

    let dir = metrics_dir();
    let mut all_entries = Vec::new();
    let mut categories = HashMap::new();

    for &(file, category) in LOG_FILES {
        let entries = read_jsonl_file(&dir.join(file), category, file)?;
        categories.insert(category.to_string(), entries.len());
        all_entries.extend(entries);
    }

    all_entries.sort_by_key(|e| Reverse(parse_ts(e)));

    let result = (all_entries, categories);
    cache.entries = Some(result.clone());
    cache.last_read = Some(Instant::now());
    Ok(result)
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

async fn logs(
    Query(params): Query<LogsQuery>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let (all_entries, categories) = match get_all_logs() {
        Ok(logs) => logs,
        Err(error) => {
            return audited_logs_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                logs_error_json(error),
            )
            .await;
        }
    };

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
                let json_str = e.data.to_string().to_lowercase();
                if !json_str.contains(&search_lower) {
                    return false;
                }
            }
            true
        })
        .collect();

    let total = filtered.len();
    let entries: Vec<LogEntry> = filtered.into_iter().skip(offset).take(limit).collect();

    let response = LogsResponse {
        total,
        categories,
        offset,
        limit,
        entries,
    };
    let response = serde_json::to_value(response).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    audited_logs_response(StatusCode::OK, response).await
}

async fn audited_logs_response(
    status: StatusCode,
    response: serde_json::Value,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    operational_read_audit::attach_operational_api_read_graph_audit(
        OperationalApiReadSurface::Logs,
        response,
    )
    .await
    .map(|response| (status, Json(response)))
    .map_err(|error| {
        tracing::error!(
            error = %error,
            "operational logs API read graph audit failed; refusing unaudited response"
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

fn logs_error_json(error: LogReadError) -> serde_json::Value {
    serde_json::json!({
        "error": error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        previous_home: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_sentinel_home(path: &std::path::Path) -> Self {
            let previous_home = std::env::var_os("SENTINEL_HOME");
            std::env::set_var("SENTINEL_HOME", path);
            Self { previous_home }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous_home.take() {
                Some(value) => std::env::set_var("SENTINEL_HOME", value),
                None => std::env::remove_var("SENTINEL_HOME"),
            }
        }
    }

    #[test]
    fn metrics_dir_uses_authoritative_home_root() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());

        assert_eq!(
            metrics_dir(),
            tmp.path().join(".claude").join("sentinel").join("metrics")
        );
    }

    #[test]
    fn read_jsonl_file_distinguishes_missing_from_malformed_json() {
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let missing = tmp.path().join("missing.jsonl");
        let missing_entries =
            read_jsonl_file(&missing, "activity", "missing.jsonl").expect("missing is empty");
        assert!(missing_entries.is_empty());

        let malformed = tmp.path().join("activity-log.jsonl");
        std::fs::write(&malformed, "{\"ts\":\"2026-06-18T12:00:00Z\"}\nnot-json\n")
            .expect("write malformed log");
        let err = read_jsonl_file(&malformed, "activity", "activity-log.jsonl")
            .expect_err("malformed JSONL must fail closed");
        assert!(err.to_string().contains("activity-log.jsonl"));
        assert!(err.to_string().contains("line 2"));
    }
}
