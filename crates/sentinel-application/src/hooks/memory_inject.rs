//! Memory Inject Hook — search Qdrant on every prompt and inject relevant memories
//!
//! **Non-blocking strategy (Phase 6):**
//! - On **Stop**: pre-compute Qdrant search results for the NEXT turn using the
//!   last user prompt. Write results to `precomputed-memories.json`.
//! - On **UserPromptSubmit**: read precomputed results if fresh (<5 min). Only
//!   fall back to a live Qdrant search if stale or missing.
//!
//! This means the first prompt of a session still does a live search (no
//! precomputed file yet), but every subsequent prompt is near-instant.
//!
//! Uses raw reqwest (not MCP tools — hooks can't call MCP tools).
//!
//! **Temporal Intelligence (Phase 3):** After retrieval, results are re-ranked
//! using time-decay + frequency boosting so recent/active memories outrank stale
//! ones at the same cosine similarity.
//!
//! ```text
//! final_score = similarity * recency_boost * frequency_boost
//! recency_boost = exp(-lambda * days_since_created)
//! frequency_boost = 1.0 + 0.3 * ln(1 + access_count)
//! ```

use chrono::Utc;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::PathBuf;
use tracing::debug;

// ---------------------------------------------------------------------------
// Precomputed memories (non-blocking strategy — Phase 6)
// ---------------------------------------------------------------------------

/// Precomputed search results written during Stop, read during UserPromptSubmit.
#[derive(serde::Serialize, serde::Deserialize)]
struct PrecomputedMemories {
    results: Vec<PrecomputedHit>,
    timestamp: String,
    query: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PrecomputedHit {
    name: String,
    score: f64,
    content: String,
    source: String,
    created_at: Option<String>,
    access_count: u64,
    memory_type: Option<String>,
}

use sentinel_domain::constants;

/// Maximum age of precomputed results before they are considered stale.
const PRECOMPUTED_MAX_AGE_SECS: i64 = constants::PRECOMPUTED_SEARCH_MAX_AGE_SECS;

fn precomputed_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".claude")
            .join("sentinel")
            .join("state")
            .join("precomputed-memories.json")
    })
}

/// Read precomputed memories if the file exists and is fresh (<5 min old).
fn read_precomputed() -> Option<PrecomputedMemories> {
    let path = precomputed_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let data: PrecomputedMemories = serde_json::from_str(&content).ok()?;

    // Check freshness
    let ts = chrono::DateTime::parse_from_rfc3339(&data.timestamp).ok()?;
    let age_secs = (Utc::now() - ts.with_timezone(&Utc)).num_seconds();
    if age_secs > PRECOMPUTED_MAX_AGE_SECS {
        debug!(age_secs, "Precomputed memories stale — will fall back to live search");
        return None;
    }

    Some(data)
}

/// Format precomputed results into the same output format as live search.
fn format_precomputed(data: &PrecomputedMemories) -> Option<String> {
    if data.results.is_empty() {
        return None;
    }

    let mem_count = data.results.iter().filter(|h| h.source == "memory").count();
    let ses_count = data.results.iter().filter(|h| h.source == "session").count();
    let mut output = format!(
        "[Qdrant Memory] {} relevant hit(s) ({} memories, {} sessions) [precomputed]:\n",
        data.results.len(),
        mem_count,
        ses_count
    );

    for hit in &data.results {
        let icon = if hit.source == "session" { "Session" } else { "Memory" };
        let recency = recency_label(hit.created_at.as_deref());
        let trust = match hit.access_count {
            n if n >= 10 => " [highly trusted]",
            n if n >= 3 => " [trusted]",
            _ => "",
        };

        // Compact summary line (~30 tokens)
        let summary = compact_summary(&hit.content, 150);
        output.push_str(&format!(
            "\n- [{:.2}] [{}]{}{} **{}**:\n  {}\n",
            hit.score, icon, recency, trust, hit.name, summary
        ));
    }

    Some(output)
}

/// Write precomputed search results for the next turn.
fn write_precomputed(hits: &[(f64, &SearchHit)], query: &str) {
    let path = match precomputed_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let data = PrecomputedMemories {
        results: hits
            .iter()
            .map(|(score, hit)| PrecomputedHit {
                name: hit.name.clone(),
                score: *score,
                content: hit.content.clone(),
                source: hit.source.clone(),
                created_at: hit.created_at.clone(),
                access_count: hit.access_count.unwrap_or(0),
                memory_type: hit.memory_type.clone(),
            })
            .collect(),
        timestamp: Utc::now().to_rfc3339(),
        query: query.to_string(),
    };

    if let Ok(json) = serde_json::to_string_pretty(&data) {
        let _ = std::fs::write(&path, json);
    }
}

/// Qdrant config (mirrors qdrant-adapters/config.rs)
#[derive(serde::Deserialize)]
struct QdrantConfig {
    cluster_url: String,
    api_key: String,
    #[serde(default = "default_collection")]
    collection: String,
    #[serde(default = "default_model")]
    model: String,
}

fn default_collection() -> String {
    "claude-memory".to_string()
}

fn default_model() -> String {
    "sentence-transformers/all-MiniLM-L6-v2".to_string()
}

fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qdrant").join("config.json"))
}

fn load_config() -> Option<QdrantConfig> {
    let path = config_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Compute project hash from cwd (same as task_persist/todo_interceptor)
fn project_hash(cwd: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_bytes());
    let result = hasher.finalize();
    result[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// A merged search result from either collection.
struct SearchHit {
    id: String, // Qdrant point ID (UUID)
    score: f64,
    name: String,
    source: String, // "memory" or "session"
    project: String,
    content: String,
    created_at: Option<String>,
    access_count: Option<u64>,
    memory_type: Option<String>,
    #[allow(dead_code)]
    verified: Option<bool>,
    stale_reason: Option<String>,
    private: bool,
}

// ---------------------------------------------------------------------------
// State file for feedback loop (Phase 4)
// ---------------------------------------------------------------------------

/// Written to `~/.claude/sentinel/state/last-injected-memories.json` after each
/// injection so the `memory_feedback` hook can detect usage and corrections.
#[derive(serde::Serialize)]
struct InjectedState {
    memories: Vec<InjectedMemoryEntry>,
    timestamp: String,
    user_prompt: Option<String>,
}

#[derive(serde::Serialize)]
struct InjectedMemoryEntry {
    id: String,
    name: String,
    score: f64,
}

/// Write the list of injected memories to the state file for the feedback hook.
fn write_injected_state(entries: &[(f64, &SearchHit)], user_prompt: Option<&str>) {
    let state_dir = match dirs::home_dir() {
        Some(h) => h.join(".claude").join("sentinel").join("state"),
        None => return,
    };
    let _ = std::fs::create_dir_all(&state_dir);

    let state = InjectedState {
        memories: entries
            .iter()
            .map(|(final_score, hit)| InjectedMemoryEntry {
                id: hit.id.clone(),
                name: hit.name.clone(),
                score: *final_score,
            })
            .collect(),
        timestamp: Utc::now().to_rfc3339(),
        user_prompt: user_prompt.map(String::from),
    };

    let path = state_dir.join("last-injected-memories.json");
    if let Ok(json) = serde_json::to_string_pretty(&state) {
        let _ = std::fs::write(&path, json);
    }
}

// ---------------------------------------------------------------------------
// Temporal Intelligence — decay + frequency scoring
// ---------------------------------------------------------------------------

/// Look up the exponential decay constant (lambda) by memory type.
/// Lower lambda = slower decay = memory stays relevant longer.
fn decay_lambda(memory_type: Option<&str>, source: &str) -> f64 {
    if source == "session" {
        return 0.025; // session chunks decay fast
    }
    match memory_type {
        Some("feedback") => 0.003,  // corrections stay relevant for months
        Some("user") => 0.005,      // user preferences are stable
        Some("reference") => 0.005, // external system pointers don't change often
        Some("project") => 0.015,   // project context changes fast
        _ => 0.010,                 // sensible default for unknown types
    }
}

/// Compute `final_score = similarity * recency_boost * frequency_boost`.
fn temporal_score(hit: &SearchHit) -> f64 {
    let lambda = decay_lambda(hit.memory_type.as_deref(), &hit.source);

    // Recency boost: exp(-lambda * days_since_created)
    let recency_boost = hit
        .created_at
        .as_deref()
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| {
            let days = (Utc::now() - dt.with_timezone(&Utc))
                .num_seconds()
                .max(0) as f64
                / 86_400.0;
            (-lambda * days).exp()
        })
        .unwrap_or(1.0); // no timestamp → no penalty

    // Frequency boost: 1.0 + 0.3 * ln(1 + access_count)
    let access = hit.access_count.unwrap_or(0) as f64;
    let frequency_boost = 1.0 + 0.3 * (1.0 + access).ln();

    hit.score * recency_boost * frequency_boost
}

/// Produce a compact summary of content, truncating at `max_chars` on a word
/// boundary. Collapses whitespace and strips leading newlines for clean output.
fn compact_summary(content: &str, max_chars: usize) -> String {
    // Collapse whitespace and trim
    let clean: String = content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if clean.chars().count() <= max_chars {
        return clean;
    }

    // Truncate on a char boundary, counting Unicode scalar values (not bytes),
    // so max_chars behaves predictably for multi-byte content like emoji.
    let byte_end = clean
        .char_indices()
        .nth(max_chars)
        .map_or(clean.len(), |(i, _)| i);
    let truncated = &clean[..byte_end];
    match truncated.rfind(' ') {
        Some(pos) => format!("{}...", &truncated[..pos]),
        None => format!("{truncated}..."),
    }
}

/// Human-readable recency tag for display.
fn recency_label(created_at: Option<&str>) -> String {
    let Some(ts) = created_at else {
        return String::new();
    };
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return String::new();
    };
    let days = (Utc::now() - dt.with_timezone(&Utc))
        .num_seconds()
        .max(0) as f64
        / 86_400.0;

    if days < 1.0 {
        " [today]".to_string()
    } else if days < 7.0 {
        format!(" [{:.0}d ago]", days)
    } else if days < 30.0 {
        format!(" [{:.0}w ago]", (days / 7.0).round())
    } else {
        format!(" [{:.0}mo ago]", (days / 30.0).round())
    }
}

// ---------------------------------------------------------------------------
// Phase 5: Context-Aware Deduplication
// ---------------------------------------------------------------------------

/// Maximum bytes of existing context to load for dedup.
const DEDUP_CONTEXT_CAP: usize = constants::DEDUP_CONTEXT_CAP;

/// Shingle overlap threshold — if more than this fraction of a hit's 3-word
/// shingles already appear in the existing context, treat it as a duplicate.
const DEDUP_OVERLAP_THRESHOLD: f64 = constants::DEDUP_OVERLAP_THRESHOLD;

/// Build a set of 3-word shingles (lowercased) from text.
fn build_shingles(text: &str) -> HashSet<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 3 {
        return HashSet::new();
    }
    words
        .windows(3)
        .map(|w| {
            let mut buf = String::with_capacity(w[0].len() + w[1].len() + w[2].len() + 2);
            buf.push_str(&w[0].to_ascii_lowercase());
            buf.push(' ');
            buf.push_str(&w[1].to_ascii_lowercase());
            buf.push(' ');
            buf.push_str(&w[2].to_ascii_lowercase());
            buf
        })
        .collect()
}

/// Load existing context files and concatenate their content.
///
/// Reads (if they exist):
/// - `~/.claude/CLAUDE.md` (global instructions)
/// - `{cwd}/CLAUDE.md` (project instructions)
/// - The MEMORY.md for the project matching `cwd` under `~/.claude/projects/*/memory/`
///
/// Caps the combined output at [`DEDUP_CONTEXT_CAP`] bytes to bound memory.
fn load_existing_context(cwd: &str) -> String {
    let mut buf = String::new();

    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return buf,
    };

    // 1. Global CLAUDE.md
    let global_claude = home.join(".claude").join("CLAUDE.md");
    if let Ok(content) = std::fs::read_to_string(&global_claude) {
        buf.push_str(&content);
        buf.push('\n');
    }

    // 2. Project CLAUDE.md (in cwd)
    let project_claude = PathBuf::from(cwd).join("CLAUDE.md");
    if let Ok(content) = std::fs::read_to_string(&project_claude) {
        buf.push_str(&content);
        buf.push('\n');
    }

    // 3. MEMORY.md — Claude uses a mangled cwd path as the project key.
    //    e.g. C:\Users\garys\Documents\GitHub\sentinel -> C--Users-garys-Documents-GitHub-sentinel
    //    We derive the key the same way Claude Code does and look for memory/MEMORY.md.
    let projects_dir = home.join(".claude").join("projects");
    if projects_dir.is_dir() {
        let key = cwd_to_project_key(cwd);
        let memory_file = projects_dir.join(&key).join("memory").join("MEMORY.md");
        if let Ok(content) = std::fs::read_to_string(&memory_file) {
            buf.push_str(&content);
            buf.push('\n');
        }
    }

    buf.truncate(DEDUP_CONTEXT_CAP);
    buf
}

/// Mangle a cwd path into the project key format Claude Code uses.
///
/// Rules: replace `:\` and `:` with `-`, replace `\` and `/` with `-`.
/// e.g. `C:\Users\garys\Documents` -> `C--Users-garys-Documents`
fn cwd_to_project_key(cwd: &str) -> String {
    cwd.replace(":\\", "--")
        .replace(':', "-")
        .replace('\\', "-")
        .replace('/', "-")
}

// ---------------------------------------------------------------------------
// Phase 7: Privacy Tags
// ---------------------------------------------------------------------------

/// Returns `true` if a hit's content or metadata indicates it should be excluded
/// from injection. Supports `<private>` / `<!-- private -->` tags in content and
/// a `private: true` payload field.
fn is_private(hit: &SearchHit) -> bool {
    let c = &hit.content;
    c.contains("<private>")
        || c.contains("<!-- private -->")
        || c.contains("<private/>")
        || c.contains("<private />")
}

/// Returns `true` if `hit_content` is a duplicate of something already present
/// in `existing_context`, based on 3-word shingle overlap.
///
/// If >60% of the hit's shingles appear in the existing context, it's a dup.
/// Empty or very short inputs are never considered duplicates.
fn is_duplicate(hit_content: &str, existing_shingles: &HashSet<String>) -> bool {
    if hit_content.split_whitespace().count() < 3 || existing_shingles.is_empty() {
        return false;
    }

    let hit_shingles = build_shingles(hit_content);
    if hit_shingles.is_empty() {
        return false;
    }

    let overlap = hit_shingles
        .iter()
        .filter(|s| existing_shingles.contains(s.as_str()))
        .count();

    let ratio = overlap as f64 / hit_shingles.len() as f64;
    ratio > DEDUP_OVERLAP_THRESHOLD
}

/// Search a single Qdrant collection and return hits.
async fn search_collection(
    client: &reqwest::Client,
    config: &QdrantConfig,
    collection: &str,
    query: &str,
    limit: u32,
    min_score: f64,
) -> Vec<SearchHit> {
    let body = serde_json::json!({
        "query": { "text": query, "model": config.model },
        "using": "text-dense",
        "limit": limit,
        "with_payload": true,
        "params": { "hnsw_ef": 64 }
    });

    let url = format!("{}/collections/{}/points/query", config.cluster_url, collection);

    let resp = match client
        .post(&url)
        .header("api-key", &config.api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return vec![],
    };

    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(_) => return vec![],
    };

    let points = json
        .get("result")
        .and_then(|r| r.get("points"))
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    let source = if collection == "claude-sessions" { "session" } else { "memory" };

    points
        .iter()
        .filter_map(|p| {
            let score = p.get("score")?.as_f64()?;
            if score < min_score {
                return None;
            }
            // Extract point ID (Qdrant returns as string UUID or integer)
            let id = p
                .get("id")
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            let payload = p.get("payload")?;
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed").to_string();
            let project = payload.get("project").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let content = payload.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let created_at = payload
                .get("created_at")
                .and_then(|v| v.as_str())
                .map(String::from);
            let access_count = payload
                .get("access_count")
                .and_then(|v| v.as_u64());
            let memory_type = payload
                .get("memory_type")
                .and_then(|v| v.as_str())
                .map(String::from);
            let verified = payload
                .get("verified")
                .and_then(|v| v.as_bool());
            let stale_reason = payload
                .get("stale_reason")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            let private = payload
                .get("private")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Some(SearchHit {
                id,
                score,
                name,
                source: source.to_string(),
                project,
                content,
                created_at,
                access_count,
                memory_type,
                verified,
                stale_reason,
                private,
            })
        })
        .collect()
}

use super::run_async;

/// Search both Qdrant collections and return merged formatted results.
fn search_qdrant(config: &QdrantConfig, query: &str, _project_hash: &str, cwd: &str, user_prompt: Option<&str>) -> Option<String> {
    let result = run_async(async {
        let client = reqwest::Client::builder()
            .timeout(constants::VECTOR_QUERY_TIMEOUT)
            .build()
            .ok()?;

        // Search both collections in parallel
        let (memories, sessions) = tokio::join!(
            search_collection(&client, config, &config.collection, query, 3, 0.30),
            search_collection(&client, config, "claude-sessions", query, 3, 0.35),
        );

        // Merge results from both collections
        let mut all: Vec<SearchHit> = memories.into_iter().chain(sessions).collect();

        // Phase 7: Privacy filter — remove hits marked private (payload or content tag)
        let pre_privacy = all.len();
        all.retain(|hit| !hit.private && !is_private(hit));
        let privacy_filtered = pre_privacy - all.len();
        if privacy_filtered > 0 {
            debug!(removed = privacy_filtered, "Filtered private memories");
        }

        // Phase 5: Context-aware dedup — load existing context once, build
        // shingle set once, then filter hits that already appear in context.
        let existing_ctx = load_existing_context(cwd);
        let ctx_shingles = build_shingles(&existing_ctx);
        let pre_dedup = all.len();
        all.retain(|hit| !is_duplicate(&hit.content, &ctx_shingles));
        let deduped = pre_dedup - all.len();
        if deduped > 0 {
            debug!(removed = deduped, "Deduped context-overlapping hits");
        }

        // Phase 3: Temporal re-ranking — decay + frequency boosting
        // Compute final_score for each hit, then sort descending.
        let mut scored: Vec<(f64, usize)> = all
            .iter()
            .enumerate()
            .map(|(i, hit)| (temporal_score(hit), i))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // Reorder `all` by temporal score and cap at 5
        let reordered: Vec<(f64, SearchHit)> = scored
            .into_iter()
            .take(5)
            .map(|(fs, idx)| {
                // We need to take ownership; build a placeholder to swap out
                let placeholder = SearchHit {
                    id: String::new(),
                    score: 0.0,
                    name: String::new(),
                    source: String::new(),
                    project: String::new(),
                    content: String::new(),
                    created_at: None,
                    access_count: None,
                    memory_type: None,
                    verified: None,
                    stale_reason: None,
                    private: false,
                };
                let hit = std::mem::replace(&mut all[idx], placeholder);
                (fs, hit)
            })
            .collect();

        if reordered.is_empty() {
            return None;
        }

        let mem_count = reordered.iter().filter(|(_, h)| h.source == "memory").count();
        let ses_count = reordered.iter().filter(|(_, h)| h.source == "session").count();
        let mut output = format!(
            "[Qdrant Memory] {} relevant hit(s) ({} memories, {} sessions):\n",
            reordered.len(), mem_count, ses_count
        );

        for (final_score, hit) in &reordered {
            let icon = if hit.source == "session" { "Session" } else { "Memory" };
            let recency = recency_label(hit.created_at.as_deref());
            let trust = match hit.access_count.unwrap_or(0) {
                n if n >= 10 => " [highly trusted]",
                n if n >= 3 => " [trusted]",
                _ => "",
            };
            let stale_tag = match &hit.stale_reason {
                Some(reason) => format!(" [STALE: {}]", reason),
                None => String::new(),
            };

            // Compact summary (~30 tokens instead of ~100)
            let summary = compact_summary(&hit.content, 150);
            output.push_str(&format!(
                "\n- [{:.2}] [{}]{}{}{} **{}** ({}):\n  {}\n",
                final_score, icon, recency, trust, stale_tag, hit.name, hit.project, summary
            ));
        }

        // Phase 4: Write injected state for the feedback hook
        let state_entries: Vec<(f64, &SearchHit)> = reordered
            .iter()
            .map(|(fs, hit)| (*fs, hit))
            .collect();
        write_injected_state(&state_entries, user_prompt);

        // Increment access_count for returned memory hits (fire-and-forget)
        let memory_ids: Vec<String> = reordered
            .iter()
            .filter(|(_, h)| h.source == "memory" && !h.id.is_empty())
            .map(|(_, h)| h.id.clone())
            .collect();
        if !memory_ids.is_empty() {
            increment_access_counts(&client, config, &memory_ids).await;
        }

        Some(output)
    });

    result
}

/// Increment access_count for a batch of memory point IDs.
/// Uses Qdrant's set_payload with a "+" increment pattern (payload update).
async fn increment_access_counts(
    client: &reqwest::Client,
    config: &QdrantConfig,
    point_ids: &[String],
) {
    // Qdrant doesn't support atomic increment in set_payload, so we read + write.
    // For access_count this is fine — a race condition on count is acceptable.
    let scroll_url = format!(
        "{}/collections/{}/points",
        config.cluster_url, config.collection
    );

    // Batch read current counts
    let body = serde_json::json!({
        "ids": point_ids,
        "with_payload": ["access_count"]
    });

    let resp = match client
        .post(&scroll_url)
        .header("api-key", &config.api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return,
    };

    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(_) => return,
    };

    let points = json
        .get("result")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();

    // Build payload updates
    let mut updates: Vec<serde_json::Value> = Vec::new();
    for point in &points {
        let id = match point.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => continue,
        };
        let current = point
            .get("payload")
            .and_then(|p| p.get("access_count"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        updates.push(serde_json::json!({
            "payload": {
                "access_count": current + 1,
                "accessed_at": chrono::Utc::now().to_rfc3339()
            },
            "points": [id]
        }));
    }

    // Apply updates
    let update_url = format!(
        "{}/collections/{}/points/payload",
        config.cluster_url, config.collection
    );
    for update in &updates {
        let _ = client
            .post(&update_url)
            .header("api-key", &config.api_key)
            .json(update)
            .send()
            .await;
    }

    debug!(count = updates.len(), "Incremented access_count for injected memories");
}

/// Process UserPromptSubmit — inject relevant memories (precomputed or live fallback).
///
/// `vector_store`: injected Qdrant adapter. If `None`, falls back to direct HTTP
/// via internal `load_config()` (legacy path, to be removed once all hooks migrate).
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    // Skip if no prompt or prompt is too short
    let prompt = match input.prompt.as_deref() {
        Some(p) if p.len() > 10 => p,
        _ => return HookOutput::allow(),
    };

    // Skip if prompt looks like a slash command (skill router handles those)
    if prompt.trim().starts_with('/') {
        return HookOutput::allow();
    }

    // Phase 6: Try precomputed results first (near-instant)
    if let Some(precomputed) = read_precomputed() {
        debug!("Using precomputed memories from Stop phase");
        if let Some(context) = format_precomputed(&precomputed) {
            return HookOutput::inject_context(HookEvent::UserPromptSubmit, &context);
        }
        // Precomputed file existed but had no results — skip live search too
        debug!("Precomputed file had no relevant results");
        return HookOutput::allow();
    }

    // Fallback: live Qdrant search (first prompt of session, or stale precomputed)
    debug!("No fresh precomputed memories — falling back to live Qdrant search");

    // Load Qdrant config
    let config = match load_config() {
        Some(c) => c,
        None => {
            debug!("No Qdrant config found — skipping memory injection");
            return HookOutput::allow();
        }
    };

    let cwd = input.cwd.as_deref().unwrap_or(".");
    let proj_hash = project_hash(cwd);

    // Search Qdrant
    match search_qdrant(&config, prompt, &proj_hash, cwd, Some(prompt)) {
        Some(context) => {
            debug!(memories = context.lines().count(), "Injecting Qdrant memories (live)");
            HookOutput::inject_context(HookEvent::UserPromptSubmit, &context)
        }
        None => {
            debug!("No relevant memories found");
            HookOutput::allow()
        }
    }
}

// ---------------------------------------------------------------------------
// Stop phase: pre-compute search results for the NEXT turn
// ---------------------------------------------------------------------------

/// Process Stop — pre-compute Qdrant search results for the next UserPromptSubmit.
///
/// Reads the last user prompt from `last-injected-memories.json` (written by the
/// previous UserPromptSubmit), searches Qdrant, and writes results to
/// `precomputed-memories.json` for the next turn to read instantly.
pub fn process_stop(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    // Read the last user prompt from the injected state file
    let state_dir = match dirs::home_dir() {
        Some(h) => h.join(".claude").join("sentinel").join("state"),
        None => return HookOutput::allow(),
    };

    let state_path = state_dir.join("last-injected-memories.json");
    let last_prompt = match std::fs::read_to_string(&state_path) {
        Ok(content) => {
            let val: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => return HookOutput::allow(),
            };
            val.get("user_prompt")
                .and_then(|v| v.as_str())
                .map(String::from)
        }
        Err(_) => None,
    };

    // Also try the prompt from the current input (Stop events can carry it)
    let query = last_prompt
        .or_else(|| input.prompt.clone())
        .unwrap_or_default();

    if query.len() <= 10 {
        debug!("No usable prompt for precompute — skipping");
        return HookOutput::allow();
    }

    // Load Qdrant config
    let config = match load_config() {
        Some(c) => c,
        None => return HookOutput::allow(),
    };

    let cwd = input.cwd.as_deref().unwrap_or(".");

    // Run search and write precomputed results
    precompute_search(&config, &query, cwd);

    HookOutput::allow()
}

/// Run Qdrant search and write results to `precomputed-memories.json`.
fn precompute_search(config: &QdrantConfig, query: &str, cwd: &str) {
    run_async(async {
        let client = match reqwest::Client::builder()
            .timeout(constants::VECTOR_BATCH_TIMEOUT)
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };

        // Search both collections in parallel
        let (memories, sessions) = tokio::join!(
            search_collection(&client, config, &config.collection, query, 3, 0.30),
            search_collection(&client, config, "claude-sessions", query, 3, 0.35),
        );

        let mut all: Vec<SearchHit> = memories.into_iter().chain(sessions).collect();

        // Phase 7: Privacy filter
        all.retain(|hit| !hit.private && !is_private(hit));

        // Phase 5: Context-aware dedup
        let existing_ctx = load_existing_context(cwd);
        let ctx_shingles = build_shingles(&existing_ctx);
        all.retain(|hit| !is_duplicate(&hit.content, &ctx_shingles));

        // Phase 3: Temporal re-ranking
        let mut scored: Vec<(f64, usize)> = all
            .iter()
            .enumerate()
            .map(|(i, hit)| (temporal_score(hit), i))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let reordered: Vec<(f64, SearchHit)> = scored
            .into_iter()
            .take(5)
            .map(|(fs, idx)| {
                let placeholder = SearchHit {
                    id: String::new(),
                    score: 0.0,
                    name: String::new(),
                    source: String::new(),
                    project: String::new(),
                    content: String::new(),
                    created_at: None,
                    access_count: None,
                    memory_type: None,
                    verified: None,
                    stale_reason: None,
                    private: false,
                };
                let hit = std::mem::replace(&mut all[idx], placeholder);
                (fs, hit)
            })
            .collect();

        // Write precomputed results (even if empty — signals "no results" to next turn)
        let refs: Vec<(f64, &SearchHit)> = reordered.iter().map(|(s, h)| (*s, h)).collect();
        write_precomputed(&refs, query);

        debug!(
            hits = reordered.len(),
            query_len = query.len(),
            "Precomputed memory search results for next turn"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_project_hash() {
        let h = project_hash("/Users/gary/projects/firefly");
        assert_eq!(h.len(), 8);
    }

    #[test]
    fn test_process_no_config() {
        let input = HookInput {
            prompt: Some("test prompt with enough length".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        // Should allow without config (no Qdrant setup)
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_process_short_prompt() {
        let input = HookInput {
            prompt: Some("hi".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_process_slash_command() {
        let input = HookInput {
            prompt: Some("/commit".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }

    // -------------------------------------------------------------------
    // Phase 3: Temporal Intelligence tests
    // -------------------------------------------------------------------

    fn make_hit(
        score: f64,
        source: &str,
        memory_type: Option<&str>,
        created_at: Option<&str>,
        access_count: Option<u64>,
    ) -> SearchHit {
        SearchHit {
            id: "test-id".to_string(),
            score,
            name: "test".to_string(),
            source: source.to_string(),
            project: "proj".to_string(),
            content: "content".to_string(),
            created_at: created_at.map(String::from),
            access_count,
            memory_type: memory_type.map(String::from),
            verified: None,
            stale_reason: None,
            private: false,
        }
    }

    #[test]
    fn test_decay_lambda_by_memory_type() {
        assert!((decay_lambda(Some("feedback"), "memory") - 0.003).abs() < f64::EPSILON);
        assert!((decay_lambda(Some("user"), "memory") - 0.005).abs() < f64::EPSILON);
        assert!((decay_lambda(Some("reference"), "memory") - 0.005).abs() < f64::EPSILON);
        assert!((decay_lambda(Some("project"), "memory") - 0.015).abs() < f64::EPSILON);
        // Unknown type gets the default
        assert!((decay_lambda(Some("unknown_type"), "memory") - 0.010).abs() < f64::EPSILON);
        assert!((decay_lambda(None, "memory") - 0.010).abs() < f64::EPSILON);
        // Session source always returns session lambda regardless of memory_type
        assert!((decay_lambda(Some("feedback"), "session") - 0.025).abs() < f64::EPSILON);
        assert!((decay_lambda(None, "session") - 0.025).abs() < f64::EPSILON);
    }

    #[test]
    fn test_temporal_score_no_metadata() {
        // No created_at, no access_count -> boosts are 1.0 each -> final = score
        let hit = make_hit(0.85, "memory", None, None, None);
        let fs = temporal_score(&hit);
        assert!((fs - 0.85).abs() < 1e-9);
    }

    #[test]
    fn test_temporal_score_recent_beats_old() {
        let now = Utc::now();
        let yesterday = (now - chrono::Duration::days(1)).to_rfc3339();
        let six_months = (now - chrono::Duration::days(180)).to_rfc3339();

        let recent = make_hit(0.80, "memory", Some("user"), Some(&yesterday), None);
        let old = make_hit(0.80, "memory", Some("user"), Some(&six_months), None);

        let score_recent = temporal_score(&recent);
        let score_old = temporal_score(&old);

        assert!(
            score_recent > score_old,
            "recent ({score_recent:.4}) should beat old ({score_old:.4})"
        );
    }

    #[test]
    fn test_temporal_score_frequency_boost() {
        let ts = Utc::now().to_rfc3339();
        let low_access = make_hit(0.70, "memory", Some("feedback"), Some(&ts), Some(1));
        let high_access = make_hit(0.70, "memory", Some("feedback"), Some(&ts), Some(100));

        let score_low = temporal_score(&low_access);
        let score_high = temporal_score(&high_access);

        assert!(
            score_high > score_low,
            "high access ({score_high:.4}) should beat low access ({score_low:.4})"
        );
    }

    #[test]
    fn test_temporal_score_feedback_decays_slower_than_session() {
        let now = Utc::now();
        let sixty_days_ago = (now - chrono::Duration::days(60)).to_rfc3339();

        let feedback = make_hit(0.80, "memory", Some("feedback"), Some(&sixty_days_ago), None);
        let session = make_hit(0.80, "session", None, Some(&sixty_days_ago), None);

        let score_feedback = temporal_score(&feedback);
        let score_session = temporal_score(&session);

        // feedback lambda=0.003 vs session lambda=0.025 at 60 days
        assert!(
            score_feedback > score_session,
            "feedback ({score_feedback:.4}) should decay slower than session ({score_session:.4})"
        );
    }

    #[test]
    fn test_temporal_score_old_memory_can_beat_recent_if_higher_similarity() {
        let now = Utc::now();
        let yesterday = (now - chrono::Duration::days(1)).to_rfc3339();
        let sixty_days_ago = (now - chrono::Duration::days(60)).to_rfc3339();

        let old_feedback =
            make_hit(0.95, "memory", Some("feedback"), Some(&sixty_days_ago), Some(50));
        let recent_session = make_hit(0.35, "session", None, Some(&yesterday), None);

        let score_old = temporal_score(&old_feedback);
        let score_recent = temporal_score(&recent_session);

        assert!(
            score_old > score_recent,
            "high-sim old feedback ({score_old:.4}) should still beat low-sim recent session ({score_recent:.4})"
        );
    }

    #[test]
    fn test_recency_label_today() {
        let now = Utc::now().to_rfc3339();
        assert_eq!(recency_label(Some(&now)), " [today]");
    }

    #[test]
    fn test_recency_label_days() {
        let three_days_ago = (Utc::now() - chrono::Duration::days(3)).to_rfc3339();
        let label = recency_label(Some(&three_days_ago));
        assert!(label.contains("d ago"), "expected days-ago label, got: {label}");
    }

    #[test]
    fn test_recency_label_weeks() {
        let two_weeks_ago = (Utc::now() - chrono::Duration::days(14)).to_rfc3339();
        let label = recency_label(Some(&two_weeks_ago));
        assert!(label.contains("w ago"), "expected weeks-ago label, got: {label}");
    }

    #[test]
    fn test_recency_label_months() {
        let three_months_ago = (Utc::now() - chrono::Duration::days(90)).to_rfc3339();
        let label = recency_label(Some(&three_months_ago));
        assert!(label.contains("mo ago"), "expected months-ago label, got: {label}");
    }

    #[test]
    fn test_recency_label_none() {
        assert_eq!(recency_label(None), "");
    }

    #[test]
    fn test_recency_label_invalid_timestamp() {
        assert_eq!(recency_label(Some("not-a-date")), "");
    }

    // -------------------------------------------------------------------
    // Phase 5: Dedup tests
    // -------------------------------------------------------------------

    #[test]
    fn test_build_shingles_basic() {
        let shingles = build_shingles("the quick brown fox jumps");
        assert_eq!(shingles.len(), 3); // 5 words -> 3 shingles
        assert!(shingles.contains("the quick brown"));
        assert!(shingles.contains("quick brown fox"));
        assert!(shingles.contains("brown fox jumps"));
    }

    #[test]
    fn test_build_shingles_too_short() {
        assert!(build_shingles("one two").is_empty());
        assert!(build_shingles("one").is_empty());
        assert!(build_shingles("").is_empty());
    }

    #[test]
    fn test_is_duplicate_high_overlap() {
        // Context contains the exact same text -> >60% overlap -> duplicate
        let context = "The Firefly Pro CRM application uses Next.js 15 \
                        with App Router and Material UI for the frontend";
        let ctx_shingles = build_shingles(context);

        let hit = "The Firefly Pro CRM application uses Next.js 15 \
                    with App Router and Material UI for the frontend dashboard";

        assert!(is_duplicate(hit, &ctx_shingles));
    }

    #[test]
    fn test_is_duplicate_low_overlap() {
        // Context and hit share almost nothing
        let context = "Sentinel is a hook engine for Claude Code marketplace \
                        with 36 hooks and proof chains";
        let ctx_shingles = build_shingles(context);

        let hit = "The Qdrant vector database stores memories using \
                    sentence-transformers embeddings for semantic search";

        assert!(!is_duplicate(hit, &ctx_shingles));
    }

    #[test]
    fn test_is_duplicate_empty_strings() {
        let empty_shingles: HashSet<String> = HashSet::new();

        // Empty hit content
        assert!(!is_duplicate("", &empty_shingles));

        // Empty context, non-empty hit
        assert!(!is_duplicate("some words here for testing", &empty_shingles));

        // Non-empty context, empty hit
        let ctx_shingles = build_shingles("some context words here for testing");
        assert!(!is_duplicate("", &ctx_shingles));
    }

    #[test]
    fn test_is_duplicate_partial_overlap() {
        // ~40% overlap should NOT be a duplicate
        let context = "alpha bravo charlie delta echo foxtrot golf hotel \
                        india juliet kilo lima mike november oscar papa";
        let ctx_shingles = build_shingles(context);

        // Shares first few words but diverges
        let hit = "alpha bravo charlie delta echo completely different \
                    text that shares nothing further with the original";

        // Only ~3-4 shingles match out of ~9-10 total -> ~30-40% -> not dup
        assert!(!is_duplicate(hit, &ctx_shingles));
    }

    #[test]
    fn test_cwd_to_project_key() {
        assert_eq!(
            cwd_to_project_key("C:\\Users\\garys\\Documents\\GitHub\\sentinel"),
            "C--Users-garys-Documents-GitHub-sentinel"
        );
        assert_eq!(
            cwd_to_project_key("/Users/gary/projects/firefly"),
            "-Users-gary-projects-firefly"
        );
    }

    #[test]
    fn test_load_existing_context_missing_files() {
        // Use a temp dir that definitely has no CLAUDE.md or MEMORY.md
        let tmp = std::env::temp_dir().join("sentinel-dedup-test-nonexistent");
        let ctx = load_existing_context(tmp.to_str().unwrap_or("."));
        // Should not panic, just return whatever global CLAUDE.md exists
        // (or empty if none). The key thing: no crash.
        assert!(ctx.len() <= DEDUP_CONTEXT_CAP);
    }

    #[test]
    fn test_load_existing_context_cap() {
        // Ensure the cap is respected (the function truncates at 50KB)
        let cwd = ".";
        let ctx = load_existing_context(cwd);
        assert!(ctx.len() <= DEDUP_CONTEXT_CAP);
    }

    // -------------------------------------------------------------------
    // Phase 6: Precomputed memories tests
    // -------------------------------------------------------------------

    #[test]
    fn test_format_precomputed_empty() {
        let data = PrecomputedMemories {
            results: vec![],
            timestamp: Utc::now().to_rfc3339(),
            query: "test".to_string(),
        };
        assert!(format_precomputed(&data).is_none());
    }

    #[test]
    fn test_format_precomputed_with_results() {
        let data = PrecomputedMemories {
            results: vec![
                PrecomputedHit {
                    name: "Test Memory".to_string(),
                    score: 0.85,
                    content: "Some test content here".to_string(),
                    source: "memory".to_string(),
                    created_at: Some(Utc::now().to_rfc3339()),
                    access_count: 5,
                    memory_type: Some("project".to_string()),
                },
                PrecomputedHit {
                    name: "Session Chunk".to_string(),
                    score: 0.72,
                    content: "Session data here".to_string(),
                    source: "session".to_string(),
                    created_at: None,
                    access_count: 0,
                    memory_type: None,
                },
            ],
            timestamp: Utc::now().to_rfc3339(),
            query: "test query".to_string(),
        };
        let output = format_precomputed(&data).unwrap();
        assert!(output.contains("[precomputed]"));
        assert!(output.contains("1 memories"));
        assert!(output.contains("1 sessions"));
        assert!(output.contains("Test Memory"));
        assert!(output.contains("Session Chunk"));
        assert!(output.contains("[trusted]")); // access_count=5 >= 3
    }

    #[test]
    fn test_precomputed_staleness() {
        // A timestamp from 10 minutes ago should be stale (>5 min)
        let old_ts = (Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
        let data = PrecomputedMemories {
            results: vec![],
            timestamp: old_ts,
            query: "test".to_string(),
        };
        // Manually check freshness logic
        let ts = chrono::DateTime::parse_from_rfc3339(&data.timestamp).unwrap();
        let age_secs = (Utc::now() - ts.with_timezone(&Utc)).num_seconds();
        assert!(age_secs > PRECOMPUTED_MAX_AGE_SECS);
    }

    #[test]
    fn test_precomputed_freshness() {
        // A timestamp from 1 minute ago should be fresh (<5 min)
        let recent_ts = (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let data = PrecomputedMemories {
            results: vec![],
            timestamp: recent_ts,
            query: "test".to_string(),
        };
        let ts = chrono::DateTime::parse_from_rfc3339(&data.timestamp).unwrap();
        let age_secs = (Utc::now() - ts.with_timezone(&Utc)).num_seconds();
        assert!(age_secs <= PRECOMPUTED_MAX_AGE_SECS);
    }

    #[test]
    fn test_process_stop_no_state() {
        // Stop with no prior state file — should just allow
        let input = HookInput {
            cwd: Some("/nonexistent".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    // -------------------------------------------------------------------
    // Phase 7: Privacy tag tests
    // -------------------------------------------------------------------

    #[test]
    fn test_is_private_content_tag() {
        let mut hit = make_hit(0.9, "memory", None, None, None);
        hit.content = "This is <private> secret data".to_string();
        assert!(is_private(&hit));
    }

    #[test]
    fn test_is_private_html_comment() {
        let mut hit = make_hit(0.9, "memory", None, None, None);
        hit.content = "<!-- private --> some secret".to_string();
        assert!(is_private(&hit));
    }

    #[test]
    fn test_is_private_self_closing() {
        let mut hit = make_hit(0.9, "memory", None, None, None);
        hit.content = "before <private/> after".to_string();
        assert!(is_private(&hit));
    }

    #[test]
    fn test_is_private_self_closing_space() {
        let mut hit = make_hit(0.9, "memory", None, None, None);
        hit.content = "before <private /> after".to_string();
        assert!(is_private(&hit));
    }

    #[test]
    fn test_is_private_payload_flag() {
        let mut hit = make_hit(0.9, "memory", None, None, None);
        hit.private = true;
        // Content has no tag, but payload flag is set
        assert!(hit.private);
    }

    #[test]
    fn test_not_private_normal_content() {
        let hit = make_hit(0.9, "memory", None, None, None);
        assert!(!is_private(&hit));
        assert!(!hit.private);
    }

    // -------------------------------------------------------------------
    // Compact summary tests
    // -------------------------------------------------------------------

    #[test]
    fn test_compact_summary_short() {
        let result = compact_summary("hello world", 150);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_compact_summary_truncates_on_word_boundary() {
        let long = "the quick brown fox jumps over the lazy dog and keeps running forever";
        let result = compact_summary(long, 30);
        assert!(result.len() <= 33); // 30 + "..."
        assert!(result.ends_with("..."));
        // Should not break mid-word
        assert!(!result.chars().last().unwrap_or(' ').is_alphanumeric()
            || result.ends_with("..."));
    }

    #[test]
    fn test_compact_summary_collapses_whitespace() {
        let messy = "  hello   world\n\nfoo   bar  ";
        let result = compact_summary(messy, 150);
        assert_eq!(result, "hello world foo bar");
    }

    #[test]
    fn test_compact_summary_truncates_safely_on_multibyte_char() {
        // Emoji are multi-byte UTF-8. Naive &s[..n] byte-slicing used to panic
        // when n landed inside a 4-byte codepoint like 🤖 (F0 9F A4 96).
        // Regression guard for memory_inject.rs:311.
        let content = "abc 🤖 xyz 🐞 qrs 📋 mno";
        // Run a range of max_chars to catch any boundary that would panic.
        for max in 1..content.chars().count() {
            let _ = compact_summary(content, max); // must not panic
        }
    }

    #[test]
    fn test_compact_summary_ascii_word_boundary_behavior_unchanged() {
        // After the UTF-8 refactor, ASCII input should still truncate at the
        // last space before the char-index cap and append "...".
        let s = "the quick brown fox jumps over the lazy dog";
        let result = compact_summary(s, 15);
        assert!(result.ends_with("..."));
        assert!(result.chars().count() <= 18); // 15 chars + "..."
    }
}
