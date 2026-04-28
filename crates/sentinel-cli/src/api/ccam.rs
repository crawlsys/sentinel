//! CCAM HQ API Endpoints
//!
//! GET /api/accounts    — all CCAM profiles with plan, expiry, cooldown
//! GET /api/sessions    — live session-env dirs with manifest + PID liveness
//! GET /api/rotation    — rotation-state.json + derived cooldown summaries
//! GET /api/utilization — utilization-cache.json per-profile percentages
//! GET /api/linear      — linear-assigned.json cache

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::routing::get;
use axum::{Json, Router};

use super::AppState;

const CACHE_TTL: Duration = Duration::from_secs(10);

fn accounts_dir() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory")
        .join(".claude")
        .join("accounts")
}

fn session_env_dir() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory")
        .join(".claude")
        .join("session-env")
}

fn sentinel_dir() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory")
        .join(".claude")
        .join("sentinel")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Accounts ────────────────────────────────────────────────────────────────

static ACCOUNTS_CACHE: Mutex<Option<(Instant, serde_json::Value)>> = Mutex::new(None);

fn load_accounts() -> serde_json::Value {
    let accounts_root = accounts_dir();
    let rotation_path = accounts_root.join("rotation-state.json");

    // Load rotation state for cooldowns and lastAssigned
    let rotation: serde_json::Value = fs::read_to_string(&rotation_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::json!({}));

    let cooldowns = rotation.get("cooldowns").cloned().unwrap_or(serde_json::json!({}));
    let last_assigned = rotation.get("lastAssigned").and_then(|v| v.as_str()).unwrap_or("");

    let entries = match fs::read_dir(&accounts_root) {
        Ok(e) => e,
        Err(_) => return serde_json::json!({ "accounts": [] }),
    };

    let now = now_ms();
    let mut accounts = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        // Skip non-profile dirs (rotation-state.json is a file, not a dir)
        let creds_path = path.join("credentials.json");
        if !creds_path.exists() {
            continue;
        }

        let creds: serde_json::Value = fs::read_to_string(&creds_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::json!({}));

        let oauth = creds.get("claudeAiOauth").cloned().unwrap_or(serde_json::json!({}));
        let expires_at = oauth.get("expiresAt").and_then(|v| v.as_u64()).unwrap_or(0);
        let subscription_type = oauth.get("subscriptionType").and_then(|v| v.as_str()).unwrap_or("unknown");
        let rate_limit_tier = oauth.get("rateLimitTier").and_then(|v| v.as_str()).unwrap_or("unknown");

        let token_status = if expires_at == 0 {
            "unknown"
        } else if expires_at < now {
            "expired"
        } else if expires_at - now < 2 * 60 * 60 * 1000 {
            "expiring_soon"
        } else {
            "live"
        };

        let expires_in_minutes: i64 = if expires_at == 0 {
            0
        } else {
            (expires_at as i64 - now as i64) / 1000 / 60
        };

        let cooldown_entry = cooldowns.get(&name);
        let on_cooldown = cooldown_entry.is_some();
        let cooldown_until = cooldown_entry.and_then(|e| e.get("until")).cloned();
        let cooldown_reason = cooldown_entry
            .and_then(|e| e.get("reason"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        accounts.push(serde_json::json!({
            "name": name,
            "subscription_type": subscription_type,
            "rate_limit_tier": rate_limit_tier,
            "token_expires_at": expires_at,
            "token_expires_in_minutes": expires_in_minutes,
            "token_status": token_status,
            "on_cooldown": on_cooldown,
            "cooldown_until": cooldown_until,
            "cooldown_reason": cooldown_reason,
            "is_active": name == last_assigned,
        }));
    }

    // Sort: active first, then alphabetical
    accounts.sort_by(|a, b| {
        let a_active = a["is_active"].as_bool().unwrap_or(false);
        let b_active = b["is_active"].as_bool().unwrap_or(false);
        b_active.cmp(&a_active).then(
            a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
        )
    });

    serde_json::json!({ "accounts": accounts })
}

async fn get_accounts() -> Json<serde_json::Value> {
    let mut cache = ACCOUNTS_CACHE.lock().unwrap();
    if let Some((ts, ref data)) = *cache {
        if ts.elapsed() < CACHE_TTL {
            return Json(data.clone());
        }
    }
    let data = load_accounts();
    *cache = Some((Instant::now(), data.clone()));
    Json(data)
}

// ── Sessions ─────────────────────────────────────────────────────────────────

static SESSIONS_CACHE: Mutex<Option<(Instant, serde_json::Value)>> = Mutex::new(None);

fn pid_alive(pid: u64) -> bool {
    // On Windows, check if process exists by opening it
    #[cfg(windows)]
    {
        use std::process::Command;
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                stdout.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        // On Unix, send signal 0 to check existence (safe — no libc/unsafe needed)
        use std::process::Command;
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

fn load_sessions() -> serde_json::Value {
    let env_root = session_env_dir();
    let entries = match fs::read_dir(&env_root) {
        Ok(e) => e,
        Err(_) => return serde_json::json!({ "sessions": [] }),
    };

    let now = now_ms();
    let mut sessions = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();

        // Parse session-env dir name: <profile>-<pid>-<epoch>
        let parts: Vec<&str> = dir_name.rsplitn(3, '-').collect();
        let (pid, started_at_ms) = if parts.len() >= 2 {
            let epoch = parts[0].parse::<u64>().unwrap_or(0);
            let pid = parts[1].parse::<u64>().unwrap_or(0);
            (pid, epoch)
        } else {
            (0, 0)
        };

        let manifest_path = path.join("session-manifest.json");
        let manifest: serde_json::Value = fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::json!({}));

        let account_profile = manifest
            .get("account_profile")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let project_key = manifest
            .get("project_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let uptime_minutes = if started_at_ms > 0 {
            (now.saturating_sub(started_at_ms)) / 1000 / 60
        } else {
            0
        };

        let alive = pid > 0 && pid_alive(pid);

        sessions.push(serde_json::json!({
            "session_env_dir": dir_name,
            "account_profile": account_profile,
            "pid": pid,
            "pid_alive": alive,
            "started_at_ms": started_at_ms,
            "uptime_minutes": uptime_minutes,
            "project_key": project_key,
        }));
    }

    // Sort: alive first, then by started_at desc
    sessions.sort_by(|a, b| {
        let a_alive = a["pid_alive"].as_bool().unwrap_or(false);
        let b_alive = b["pid_alive"].as_bool().unwrap_or(false);
        b_alive.cmp(&a_alive).then(
            b["started_at_ms"].as_u64().unwrap_or(0).cmp(&a["started_at_ms"].as_u64().unwrap_or(0))
        )
    });

    serde_json::json!({ "sessions": sessions })
}

async fn get_sessions() -> Json<serde_json::Value> {
    let mut cache = SESSIONS_CACHE.lock().unwrap();
    if let Some((ts, ref data)) = *cache {
        if ts.elapsed() < CACHE_TTL {
            return Json(data.clone());
        }
    }
    let data = load_sessions();
    *cache = Some((Instant::now(), data.clone()));
    Json(data)
}

// ── Rotation ─────────────────────────────────────────────────────────────────

async fn get_rotation() -> Json<serde_json::Value> {
    let path = accounts_dir().join("rotation-state.json");
    let raw: serde_json::Value = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::json!({}));

    let now = now_ms();

    // Enrich cooldowns with hours_remaining
    let mut cooldowns = serde_json::Map::new();
    if let Some(cd_map) = raw.get("cooldowns").and_then(|v| v.as_object()) {
        for (profile, entry) in cd_map {
            let until = entry.get("until").and_then(|v| v.as_u64()).unwrap_or(0);
            let hours_remaining = if until > now {
                ((until - now) as f64 / 1000.0 / 3600.0).ceil() as u64
            } else {
                0
            };
            let mut enriched = entry.as_object().cloned().unwrap_or_default();
            enriched.insert("hours_remaining".to_string(), serde_json::json!(hours_remaining));
            cooldowns.insert(profile.clone(), serde_json::Value::Object(enriched));
        }
    }

    Json(serde_json::json!({
        "last_assigned": raw.get("lastAssigned"),
        "rotation_count": raw.get("rotationCount"),
        "last_rotation": raw.get("lastRotation"),
        "cooldowns": cooldowns,
        "paused": raw.get("paused").cloned().unwrap_or(serde_json::json!([])),
    }))
}

// ── Utilization ──────────────────────────────────────────────────────────────

async fn get_utilization() -> Json<serde_json::Value> {
    let path = accounts_dir().join("utilization-cache.json");
    let data: serde_json::Value = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::json!({ "profiles": {}, "updated_at": null }));
    Json(data)
}

// ── Linear ───────────────────────────────────────────────────────────────────

async fn get_linear() -> Json<serde_json::Value> {
    let path = sentinel_dir().join("linear-assigned.json");
    let data: serde_json::Value = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::json!({ "updated_at": null, "issues": [] }));
    Json(data)
}

// ── Dashboard HTML ───────────────────────────────────────────────────────────

async fn get_dashboard() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("ccam_dashboard.html"))
}

// ── Router ───────────────────────────────────────────────────────────────────

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ccam", get(get_dashboard))
        .route("/accounts", get(get_accounts))
        .route("/sessions", get(get_sessions))
        .route("/rotation", get(get_rotation))
        .route("/utilization", get(get_utilization))
        .route("/linear", get(get_linear))
}
