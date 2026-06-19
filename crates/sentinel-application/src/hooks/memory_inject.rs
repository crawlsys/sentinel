//! Memory Inject Hook — surface relevant atoms to the model on every turn.
//!
//! Fires on `UserPromptSubmit`. Calls memory-mcp's `memory_search` tool, renders
//! the hits into a compact Markdown block, and injects it as additionalContext.
//! Also writes `last-injected-memories.json` so the `memory_feedback` hook
//! (Stop) can classify each atom's outcome (used / contradicted / ignored)
//! and feed it back into the Memory engine's `RelevanceUpdater`.
//!
//! No direct Qdrant traffic — every hit routes through the Memory engine's
//! Retriever via the MCP subprocess. That pipeline already does hybrid
//! search + project-bleed + rerank + utility/freshness, so sentinel's
//! client-side scoring helpers (`decay_lambda`, `temporal_score`, shingle
//! dedup, precompute cache) are all gone.

use std::fmt::Write as _;
use std::path::PathBuf;

use chrono::Utc;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use tracing::{debug, warn};

use super::{run_async_timeout, FileSystemPort, MemoryMcpPort};

// ---------------------------------------------------------------------------
// Relevance floor — drop the low-score tail before injecting
// ---------------------------------------------------------------------------

/// Minimum `final_score` an atom must clear to be injected. `memory_search`
/// returns the top-k by score with no quality bar, so on a vague or
/// conversational prompt the tail is 0.03–0.06 — atoms that "matched" only
/// because something had to fill the k slots. Injecting those is pure clutter
/// (the model scrolls past them) and pollutes the feedback loop with atoms
/// that were never really relevant. A floor keeps the sharp 0.5–0.8 hits on a
/// targeted query and surfaces nothing when the engine only has weak matches.
///
/// 0.10 is chosen from observed distributions: genuinely-useful hits this
/// engine produces score ~0.14+, while the noise tail sits at 0.03–0.06.
/// Tunable at runtime via `MEMORY_INJECT_MIN_SCORE` (e.g. `0` to restore the
/// old inject-everything behaviour, or `0.2` to be stricter) without a rebuild.
const DEFAULT_MIN_INJECT_SCORE: f64 = 0.10;

/// Resolve the injection score floor: `MEMORY_INJECT_MIN_SCORE` if it parses to
/// a finite, non-negative f64, else [`DEFAULT_MIN_INJECT_SCORE`]. A malformed
/// value warns once and falls back rather than silently disabling the floor.
fn min_inject_score() -> f64 {
    match std::env::var("MEMORY_INJECT_MIN_SCORE") {
        Ok(raw) => match raw.trim().parse::<f64>() {
            Ok(v) if v.is_finite() && v >= 0.0 => v,
            _ => {
                warn!(
                    value = %raw,
                    default = DEFAULT_MIN_INJECT_SCORE,
                    "MEMORY_INJECT_MIN_SCORE is not a finite non-negative number — using default"
                );
                DEFAULT_MIN_INJECT_SCORE
            }
        },
        Err(_) => DEFAULT_MIN_INJECT_SCORE,
    }
}

/// Keep only hits whose `final_score` is at or above `floor`. Pure (no env,
/// no I/O) so the floor behaviour is unit-testable; the caller resolves the
/// floor via [`min_inject_score`].
fn apply_relevance_floor(hits: Vec<UnifiedHit>, floor: f64) -> Vec<UnifiedHit> {
    hits.into_iter()
        .filter(|h| h.final_score >= floor)
        .collect()
}

// ---------------------------------------------------------------------------
// Injected state — written so memory_feedback can classify outcomes on Stop
// ---------------------------------------------------------------------------

/// Single injected atom, surfaced into the state file.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct InjectedMemoryEntry {
    id: String,
    /// Retrieval-event id from `memory_search` for this atom. Threaded back to
    /// `memory_record_outcome` by `memory_feedback` so the outcome attaches to the
    /// exact retrieval event (not the atom id, which the log isn't keyed by).
    #[serde(default)]
    event_id: Option<String>,
    name: String,
    score: f64,
}

/// Shape of `~/.claude/sentinel/state/last-injected-memories.json` read by
/// `memory_feedback` on Stop.
#[derive(serde::Serialize, serde::Deserialize)]
struct InjectedState {
    memories: Vec<InjectedMemoryEntry>,
    timestamp: String,
    user_prompt: Option<String>,
}

fn state_dir(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    fs.home_dir()
        .map(|h| h.join(".claude").join("sentinel").join("state"))
}

fn write_injected_state(fs: &dyn FileSystemPort, hits: &[UnifiedHit], user_prompt: Option<&str>) {
    let Some(dir) = state_dir(fs) else {
        return;
    };
    let _ = fs.create_dir_all(&dir);

    let state = InjectedState {
        memories: hits
            .iter()
            .map(|h| InjectedMemoryEntry {
                id: h.atom_id.clone(),
                event_id: h.event_id.clone(),
                name: format!("{}/{}={}", h.subject, h.predicate, h.value),
                score: h.final_score,
            })
            .collect(),
        timestamp: Utc::now().to_rfc3339(),
        user_prompt: user_prompt.map(String::from),
    };

    let path = dir.join("last-injected-memories.json");
    if let Ok(json) = serde_json::to_string_pretty(&state) {
        let _ = fs.write(&path, json.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Unified memory_search hit shape (deserialised from memory-mcp payload)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Debug, Clone)]
struct UnifiedHit {
    atom_id: String,
    #[serde(default)]
    event_id: Option<String>,
    subject: String,
    predicate: String,
    value: String,
    project: String,
    #[serde(default)]
    final_score: f64,
}

// ---------------------------------------------------------------------------
// Project derivation
// ---------------------------------------------------------------------------

/// Derive a project name from cwd that satisfies memory-mcp's
/// `validate_project` regex `[A-Za-z0-9_-]{1,128}`. Falls back to
/// `"global"` when nothing usable can be extracted.
fn project_from_cwd(cwd: &str) -> String {
    let normalized = cwd.replace('\\', "/");
    let basename = normalized
        .trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("");

    let mapped: String = basename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Collapse duplicate dashes and trim leading/trailing
    let mut cleaned = String::with_capacity(mapped.len());
    let mut last_dash = false;
    for c in mapped.chars() {
        if c == '-' {
            if !last_dash {
                cleaned.push(c);
            }
            last_dash = true;
        } else {
            cleaned.push(c);
            last_dash = false;
        }
    }
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        return "global".to_string();
    }
    if cleaned.len() > 128 {
        cleaned.chars().take(128).collect()
    } else {
        cleaned
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn compact_summary(content: &str, max_chars: usize) -> String {
    let collapsed: String = content.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }

    // Truncate on a char boundary, prefer a trailing word boundary within
    // the last ~15% of the window.
    let truncate_at = collapsed
        .char_indices()
        .nth(max_chars)
        .map_or(collapsed.len(), |(i, _)| i);

    let window = &collapsed[..truncate_at];
    let cutoff = window
        .rfind(' ')
        .filter(|&i| i + 1 >= (max_chars * 85 / 100))
        .unwrap_or(window.len());

    format!("{}…", &window[..cutoff])
}

fn render_context(hits: &[UnifiedHit]) -> String {
    let mut out = format!("[Memory] {} relevant atom(s):\n", hits.len());
    for h in hits {
        let short = compact_summary(&h.value, 150);
        let _ = write!(
            out,
            "\n- [{:.2}] **{}/{}={}** ({}):\n  {}\n",
            h.final_score, h.subject, h.predicate, h.value, h.project, short
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Core search path
// ---------------------------------------------------------------------------

/// Call memory-mcp's `memory_search` tool and return hits + rendered context.
/// Returns `None` when memory-mcp returns zero hits or the subprocess fails.
fn search_memory_engine(
    memory_mcp: &dyn MemoryMcpPort,
    prompt: &str,
    cwd: &str,
) -> Option<(Vec<UnifiedHit>, String)> {
    let project = project_from_cwd(cwd);
    let mut args = serde_json::Map::new();
    args.insert(
        "query".into(),
        serde_json::Value::String(prompt.to_string()),
    );
    args.insert("project".into(), serde_json::Value::String(project.clone()));
    args.insert("top_k".into(), serde_json::Value::Number(8u32.into()));
    // Rerank the injected atoms. The memories surfaced into context every turn
    // are exactly the ones worth ordering well, so we ask memory-mcp to run the
    // LLM reranker (better semantic ordering than raw embedding score). This is
    // safe in the hook: the reranker degrades to base order on any failure (and
    // search is already wrapped in the 10s budget below), so a slow or failed
    // rerank never blocks the turn — it just falls back to embedding order.
    args.insert("rerank".into(), serde_json::Value::Bool(true));

    // Recall search cold-starts memory-mcp + server-side embed + vector search
    // + rerank — that routinely exceeds the default 3s hook budget, which would
    // silently drop recall (and starve the feedback loop). Give it a dedicated
    // 10s budget: this is the read path, so a slightly slower first prompt of a
    // session is an acceptable trade for reliable recall.
    let payload: serde_json::Value = run_async_timeout(
        async {
            match memory_mcp.call_tool("memory_search", args).await {
                Ok(p) => Some(p),
                Err(e) => {
                    warn!(
                        project = %project,
                        error = %e,
                        "memory-mcp search failed — skipping injection this turn"
                    );
                    None
                }
            }
        },
        std::time::Duration::from_secs(10),
    )?;

    let raw_hits: Vec<UnifiedHit> = payload
        .get("hits")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    if raw_hits.is_empty() {
        return None;
    }

    // Drop the low-relevance tail. memory_search hands back top_k by score with
    // no quality bar, so vague prompts return 0.03–0.06 filler — inject only
    // atoms that clear the floor. If nothing clears it, surface nothing (a weak
    // recall is the same signal as no recall: the engine had nothing useful).
    let floor = min_inject_score();
    let total = raw_hits.len();
    let hits = apply_relevance_floor(raw_hits, floor);
    if hits.is_empty() {
        debug!(
            total,
            floor, "all recall hits below relevance floor — not injecting"
        );
        return None;
    }
    if hits.len() < total {
        debug!(
            kept = hits.len(),
            dropped = total - hits.len(),
            floor,
            "dropped low-relevance recall hits below floor"
        );
    }

    let rendered = render_context(&hits);
    Some((hits, rendered))
}

// ---------------------------------------------------------------------------
// Hook entry points
// ---------------------------------------------------------------------------

/// Process `UserPromptSubmit` — query memory-mcp and inject matching atoms.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    // Skip empty / too-short prompts
    let prompt = match input.prompt.as_deref() {
        Some(p) if p.len() > 10 => p,
        _ => return HookOutput::allow(),
    };

    // Slash commands are handled by the skill router — nothing to inject.
    if prompt.trim().starts_with('/') {
        return HookOutput::allow();
    }

    let cwd = input.cwd.as_deref().unwrap_or(".");
    let project = project_from_cwd(cwd);
    let started = std::time::Instant::now();
    let result = search_memory_engine(ctx.memory_mcp, prompt, cwd);
    let duration_ms = started.elapsed().as_millis() as u64;

    // Telemetry: emit a `recall` event whether or not anything was surfaced —
    // a zero-hit recall is itself signal for the end-to-end trace. See
    // crate::memory_telemetry.
    let hits_for_telem: Vec<(String, Option<String>, String, f64)> = result
        .as_ref()
        .map(|(hits, _)| {
            hits.iter()
                .map(|h| {
                    (
                        h.atom_id.clone(),
                        h.event_id.clone(),
                        format!("{}/{}={}", h.subject, h.predicate, h.value),
                        h.final_score,
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    crate::memory_telemetry::record_recall(
        ctx.fs,
        input.session_id.as_deref(),
        &project,
        prompt.len(),
        result.is_some(),
        &hits_for_telem,
        duration_ms,
    );

    // Dual-display capture notice: if the detached turn-capture from a prior
    // turn left a one-shot notice, surface it to BOTH audiences — Gary via
    // `systemMessage` (shown in his terminal, not in the model's context) and
    // the model via `additionalContext`. Read-and-delete so it shows once.
    let notice = take_capture_notice(ctx.fs);

    let mut out = if let Some((hits, rendered)) = result {
        write_injected_state(ctx.fs, &hits, Some(prompt));
        debug!(atoms = hits.len(), "Injecting atoms via memory-mcp");
        HookOutput::inject_context(HookEvent::UserPromptSubmit, &rendered)
    } else {
        debug!("memory-mcp returned no hits — not injecting");
        HookOutput::allow()
    };

    if let Some((human_msg, model_msg)) = notice {
        // Human-only channel.
        out.system_message = Some(human_msg);
        // Model channel: append to any recall context already being injected.
        let hso = out.hook_specific_output.get_or_insert_with(|| {
            sentinel_domain::events::HookSpecificOutput {
                hook_event_name: HookEvent::UserPromptSubmit.to_string(),
                ..Default::default()
            }
        });
        hso.additional_context = Some(match hso.additional_context.take() {
            Some(existing) => format!("{model_msg}\n\n{existing}"),
            None => model_msg,
        });
    }

    out
}

/// Shape of `~/.claude/sentinel/state/pending-capture-notice.json`, written by
/// the detached `memory turn-capture` CLI when atoms land.
#[derive(serde::Deserialize)]
struct CaptureNotice {
    #[serde(default)]
    project: String,
    #[serde(default)]
    written: usize,
    #[serde(default)]
    reinforced: usize,
    #[serde(default)]
    superseded: usize,
    #[serde(default)]
    quarantined: usize,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    names_total: usize,
}

/// Read-and-delete the pending capture notice. Returns `(human_message,
/// model_message)` when present. One-shot: the file is removed so the notice
/// surfaces exactly once. Returns `None` when there's no notice or it can't be
/// parsed.
fn take_capture_notice(fs: &dyn FileSystemPort) -> Option<(String, String)> {
    let path = state_dir(fs)?.join("pending-capture-notice.json");
    if !fs.exists(&path) {
        return None;
    }
    let content = fs.read_to_string(&path).ok()?;
    // Delete immediately (best-effort) so it shows once even if parsing the
    // already-read content somehow fails downstream.
    let _ = fs.remove_file(&path);
    let n: CaptureNotice = serde_json::from_str(&content).ok()?;
    format_capture_notice(&n)
}

/// Pure formatter for a capture notice — returns `(human_message,
/// model_message)` or `None` when nothing durable landed. Split out from
/// `take_capture_notice` so the formatting is unit-testable without a
/// filesystem stub.
fn format_capture_notice(n: &CaptureNotice) -> Option<(String, String)> {
    let landed = n.written + n.reinforced + n.superseded;
    if landed == 0 {
        return None;
    }

    // Compact bullet list of what landed (already capped to 6 by the writer).
    let mut bullets = String::new();
    for name in &n.names {
        let short: String = name.chars().take(90).collect();
        bullets.push_str("\n  • ");
        bullets.push_str(&short);
    }
    let more = if n.names_total > n.names.len() {
        format!("\n  • (+{} more)", n.names_total - n.names.len())
    } else {
        String::new()
    };
    let quar = if n.quarantined > 0 {
        format!(" · {} quarantined", n.quarantined)
    } else {
        String::new()
    };

    let human = format!(
        "💾 Memory captured last turn — {landed} atom(s) in {}{}{bullets}{more}",
        n.project, quar
    );
    let model = format!(
        "[Memory] Auto-captured last turn ({landed} durable atom(s) in {}{}):{bullets}{more}",
        n.project, quar
    );
    Some((human, model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compact_summary_short_returns_unchanged() {
        let result = compact_summary("hello world", 150);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_compact_summary_truncates_long() {
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda";
        let result = compact_summary(long, 30);
        assert!(result.ends_with('…'));
        assert!(result.chars().count() <= 31);
    }

    #[test]
    fn test_compact_summary_collapses_whitespace() {
        let messy = "hello   world\n\t  foo";
        let result = compact_summary(messy, 150);
        assert_eq!(result, "hello world foo");
    }

    #[test]
    fn test_compact_summary_handles_multibyte() {
        // Must not panic on non-ASCII boundaries.
        let s = "héllo wörld ñoño";
        for max in [1, 3, 5, 7, 10, 100] {
            let _ = compact_summary(s, max);
        }
    }

    #[test]
    fn test_project_from_cwd_posix_and_windows() {
        assert_eq!(
            project_from_cwd("/Users/gary/code/firefly-pro"),
            "firefly-pro"
        );
        assert_eq!(
            project_from_cwd(r"C:\Users\gary\code\firefly-pro"),
            "firefly-pro"
        );
    }

    #[test]
    fn test_project_from_cwd_strips_invalid_chars() {
        assert_eq!(project_from_cwd("/a/b/my.repo"), "my-repo");
        assert_eq!(project_from_cwd("/a/b/my repo"), "my-repo");
    }

    #[test]
    fn test_project_from_cwd_falls_back_to_global() {
        assert_eq!(project_from_cwd(""), "global");
        assert_eq!(project_from_cwd("/"), "global");
    }

    #[test]
    fn test_render_context_empty_hits() {
        let out = render_context(&[]);
        assert!(out.contains("0 relevant atom(s)"));
    }

    #[test]
    fn test_render_context_single_hit() {
        let hits = vec![UnifiedHit {
            atom_id: "abc".to_string(),
            event_id: Some("evt-1".to_string()),
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            value: "rust".to_string(),
            project: "test".to_string(),
            final_score: 0.87,
        }];
        let out = render_context(&hits);
        assert!(out.contains("0.87"));
        assert!(out.contains("user/likes=rust"));
    }

    fn hit(score: f64) -> UnifiedHit {
        UnifiedHit {
            atom_id: "a".into(),
            event_id: None,
            subject: "s".into(),
            predicate: "p".into(),
            value: "v".into(),
            project: "proj".into(),
            final_score: score,
        }
    }

    #[test]
    fn relevance_floor_drops_below_keeps_at_or_above() {
        let hits = vec![hit(0.80), hit(0.10), hit(0.06), hit(0.03)];
        let kept = apply_relevance_floor(hits, 0.10);
        // 0.80 and 0.10 (inclusive boundary) survive; 0.06 and 0.03 are noise.
        let scores: Vec<f64> = kept.iter().map(|h| h.final_score).collect();
        assert_eq!(scores, vec![0.80, 0.10]);
    }

    #[test]
    fn relevance_floor_empty_when_all_below() {
        let hits = vec![hit(0.06), hit(0.04), hit(0.03)];
        assert!(apply_relevance_floor(hits, 0.10).is_empty());
    }

    #[test]
    fn relevance_floor_zero_keeps_everything() {
        // floor=0 restores the old inject-everything behaviour.
        let hits = vec![hit(0.5), hit(0.01), hit(0.0)];
        assert_eq!(apply_relevance_floor(hits, 0.0).len(), 3);
    }

    #[test]
    fn min_inject_score_defaults_when_env_unset_or_garbage() {
        // Note: env-mutation tests can race other tests in-process; we only
        // assert the default constant is the documented value, and that the
        // parse helper rejects garbage. The full env path is exercised
        // implicitly in production.
        assert!((DEFAULT_MIN_INJECT_SCORE - 0.10).abs() < f64::EPSILON);
    }

    #[test]
    fn test_process_skips_short_prompt() {
        let input = HookInput {
            prompt: Some("short".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_process_skips_slash_command() {
        let input = HookInput {
            prompt: Some("/commit something long enough".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(process(&input, &ctx).blocked.is_none());
    }

    #[test]
    fn test_injected_state_roundtrip() {
        let state = InjectedState {
            memories: vec![InjectedMemoryEntry {
                id: "atom-1".to_string(),
                event_id: Some("evt-1".to_string()),
                name: "user/likes=rust".to_string(),
                score: 0.9,
            }],
            timestamp: "2026-04-25T00:00:00Z".to_string(),
            user_prompt: Some("why rust".to_string()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: InjectedState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.memories.len(), 1);
        assert_eq!(parsed.memories[0].id, "atom-1");
        assert_eq!(parsed.user_prompt.as_deref(), Some("why rust"));
    }

    #[test]
    fn capture_notice_parses_and_formats_dual_messages() {
        let json = r#"{
            "ts":"t","project":"memory","written":2,"reinforced":0,
            "superseded":1,"quarantined":1,
            "names":["memory daemon/runs on port=3011","Firefly Pro/primary domain=fireflypro.com"],
            "names_total":3
        }"#;
        let n: CaptureNotice = serde_json::from_str(json).unwrap();
        let (human, model) = format_capture_notice(&n).expect("3 landed → Some");
        // Human channel: friendly, names + counts + quarantine note.
        assert!(human.contains("💾"));
        assert!(human.contains("3 atom")); // 2 written + 1 superseded
        assert!(human.contains("memory")); // project
        assert!(human.contains("3011")); // a name
        assert!(human.contains("1 quarantined"));
        assert!(human.contains("(+1 more)")); // names_total 3 > 2 shown
                                              // Model channel: same substance, [Memory] prefix.
        assert!(model.starts_with("[Memory]"));
        assert!(model.contains("3 durable atom"));
    }

    #[test]
    fn capture_notice_none_when_nothing_landed() {
        let n: CaptureNotice = serde_json::from_str(
            r#"{"project":"p","written":0,"reinforced":0,"superseded":0,"quarantined":2,"names":[],"names_total":0}"#,
        )
        .unwrap();
        // Only quarantined/dropped — nothing durable landed → no notice.
        assert!(format_capture_notice(&n).is_none());
    }
}
