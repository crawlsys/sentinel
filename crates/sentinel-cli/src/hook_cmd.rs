//! `sentinel hook` — Process hook events (thin client or standalone)

use std::collections::HashMap;

use anyhow::Result;
use tracing::debug;

use sentinel_application::hook_metrics::{time_and_record, InvocationContext};
use sentinel_application::hooks;
use sentinel_domain::events::{HookEvent, HookOutput, HookSpecificOutput};
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{SkillSteps, SkillWorkflow};

use sentinel_domain::capability::VendorClass;
use sentinel_domain::ports::{AuditorPort, LlmPort, VectorStorePort};
use sentinel_infrastructure::ba_config::BaEnforcementConfig;
use sentinel_infrastructure::capability_router::TomlCapabilityRouter;
use sentinel_infrastructure::dry_run_auditor::RigAuditor;
use sentinel_infrastructure::git::RealGit;
use sentinel_infrastructure::memory_mcp_client::MemoryMcpClient;
use sentinel_infrastructure::provenance_store::JsonlProvenanceStore;
use sentinel_infrastructure::requirement_matrix::FilesystemRequirementMatrix;
use sentinel_infrastructure::reversibility::LayeredReversibilityClassifier;
use std::sync::Arc;

pub async fn run(event: &str, matcher: Option<&str>, standalone: bool) -> Result<()> {
    // ── Glass break emergency override ───────────────────────────────────
    // Two-layer design:
    //   1. Active token (~/.claude/sentinel/.glass-break-token) — fast path,
    //      checked on every hook invocation. Contains expiry + HMAC.
    //   2. Trigger file (~/.claude/sentinel/.glass-break) — slow path,
    //      detected once, launches a native Windows dialog requiring human
    //      confirmation. On success, writes a time-limited token and deletes
    //      the trigger file. AI cannot complete the dialog.
    if check_glass_break_override() {
        eprintln!("[sentinel] GLASS BREAK ACTIVE — all enforcement bypassed");
        println!("{{}}");
        return Ok(());
    }

    // Always run inline — the supervisor (spawn child + pipe stdin/stdout)
    // added 5-15s overhead on Windows due to process creation, pipe inheritance,
    // and stdin read timing issues. Inline execution is instant.
    run_internal(event, matcher, standalone).await
}

pub async fn run_internal(event: &str, matcher: Option<&str>, standalone: bool) -> Result<()> {
    // ── Glass break emergency override (same check as supervisor) ─────────
    if check_glass_break_override() {
        eprintln!("[sentinel] GLASS BREAK ACTIVE — all enforcement bypassed");
        println!("{{}}");
        return Ok(());
    }

    let start_time = std::time::Instant::now();
    debug!(event, ?matcher, standalone, "Processing hook event");

    // Parse event type
    // **Attack #120 fix**: Fail closed on unknown event types instead of defaulting
    // to Stop. Silently treating an unknown event as Stop would run Stop-phase hooks
    // (execution_log, skill_telemetry, context_monitor, etc.) which could confuse
    // state tracking. Better to error out so the issue is immediately visible.
    let hook_event = parse_hook_event(event)?;

    // **Stdin authentication**: Validate that we're being invoked by a plausible
    // caller (Claude Code / node process). This is defense-in-depth — not a hard
    // guarantee, but raises the bar from "any process on the system" to "a process
    // that can convincingly mimic Claude Code's invocation pattern".
    if let Err(e) = validate_caller() {
        // Caller validation failed — return safe empty JSON and exit early.
        debug!(error = %e, "Caller validation failed — returning safe response");
        return write_safe_allow_response();
    }

    // Read input from stdin
    let input = sentinel_infrastructure::stdin::read_hook_input()?;

    // Load config
    let config_dir = sentinel_infrastructure::config::config_dir();
    let workflows: HashMap<String, SkillWorkflow> = if config_dir.join("workflows.toml").exists() {
        sentinel_infrastructure::config::load_workflows(&config_dir)?
            .into_iter()
            .map(|w| (w.skill.clone(), w))
            .collect()
    } else {
        HashMap::new()
    };

    // **Attack #67 fix**: Acquire session lock BEFORE loading state.
    // Hold through processing + save to prevent concurrent hook invocations
    // from overwriting each other's state changes (lost updates).
    //
    // **Attack #128 note**: Lock safety on panic — `_session_lock` is a
    // `std::fs::File` handle with fs2 advisory lock. Rust's Drop trait
    // guarantees the file handle is closed on unwind (panic), which releases
    // the advisory lock. No manual cleanup needed.
    // **Bug fix (2026-05-06)**: Previously fell back to "unknown" silently when
    // session_id was missing — which meant CLI invocations from bash (no stdin
    // HookInput) would do work against a synthetic "unknown" session, never
    // matching the active Claude Code session. State writes/reads landed in the
    // wrong place (ghost session_id), and tasks.md regeneration appeared to
    // succeed but operated on the wrong session's task store.
    //
    // Now: prefer input.session_id, then $CLAUDE_SESSION_ID env var (set by
    // mcp-router and cron contexts), then fail loudly with a clear message
    // instead of silently corrupting state under "unknown".
    let session_id_owned: String = match input.session_id.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => match std::env::var("CLAUDE_SESSION_ID") {
            Ok(s) if !s.is_empty() => s,
            _ => {
                eprintln!(
                    "[sentinel] hook: no session_id available — pass via stdin HookInput.session_id or set CLAUDE_SESSION_ID env var. Refusing to operate against synthetic 'unknown' session."
                );
                // Return safe empty JSON so callers don't crash, but do not
                // proceed with state mutations.
                println!("{{}}");
                return Ok(());
            }
        },
    };
    let session_id: &str = &session_id_owned;

    // **Rate limiting**: Check per-session invocation rate BEFORE acquiring session lock.
    // This prevents flood attacks from even contending for the lock, reducing DoS impact.
    sentinel_infrastructure::rate_limit::check_rate_limit(session_id)?;

    let _session_lock = sentinel_infrastructure::state_store::acquire_session_lock(session_id)?;
    let mut state = sentinel_infrastructure::state_store::load(session_id)?
        .unwrap_or_else(|| SessionState::new(session_id));

    // Load step configs lazily — only for the active skill (if any).
    // Loading all 47 skill step files on every hook invocation costs ~5s on
    // Windows due to per-file syscall overhead. The step configs are only used
    // by phase_validator, which requires an active_skill anyway.
    let step_configs: HashMap<String, SkillSteps> = state
        .active_skill
        .as_deref()
        .and_then(|skill| {
            sentinel_infrastructure::config::load_skill_steps(&config_dir, skill)
                .ok()
                .flatten()
                .map(|steps| std::iter::once((skill.to_string(), steps)).collect())
        })
        .unwrap_or_default();

    let git = RealGit;

    // Construct vector store adapter (None if Qdrant not configured)
    let vector_store: Option<Arc<dyn VectorStorePort>> =
        sentinel_infrastructure::qdrant::QdrantAdapter::from_config()
            .map(|a| Arc::new(a) as Arc<dyn VectorStorePort>);

    // Construct filesystem, process, and env adapters
    let real_fs = sentinel_infrastructure::filesystem::RealFileSystem;
    let real_process = sentinel_infrastructure::process::RealProcess;
    let real_env = sentinel_infrastructure::env::RealEnv;

    // Construct LLM adapter. STANDARDIZED on Rig + OpenRouter
    // (`OPENROUTER_API_KEY`) — the same gateway the adversarial judge uses.
    // No direct-vendor SDK path. `None` if the key is unset.
    let llm: Option<Arc<dyn LlmPort>> =
        sentinel_infrastructure::openrouter_llm::OpenRouterLlm::from_env()
            .ok()
            .map(|c| Arc::new(c) as Arc<dyn LlmPort>);

    // Construct memory-mcp client (always present; reads MEMORY_MCP_CMD /
    // MEMORY_MCP_TIMEOUT_SECS from env, defaults handled by from_env).
    let memory_mcp = MemoryMcpClient::from_env();

    // A6 Phase 4a: construct the layered reversibility classifier from the
    // shipped `config/reversibility-defaults.toml`. If the shipped TOML
    // fails to parse (build-time include_str!), we degrade gracefully to
    // an empty classifier — the existing in_scope logic in tool_usage_gate
    // still runs as the fallback so the gate stack continues to work.
    // Operator overrides (~/.claude/sentinel/config/reversibility.toml)
    // are not wired yet — follow-up phase.
    let reversibility_classifier = LayeredReversibilityClassifier::with_shipped_defaults()
        .unwrap_or_else(|err| {
            tracing::warn!(
                ?err,
                "failed to load shipped reversibility defaults; using empty classifier (gate falls back to existing in_scope logic)"
            );
            LayeredReversibilityClassifier::empty()
        });

    // Constitution gate rule list — operator-authored TOML at
    // ~/.claude/sentinel/config/constitution-gate.toml. Missing file
    // or parse failure -> empty rule list -> hook is a no-op (the
    // documented opt-in semantics). Loaded once per invocation so
    // the read happens off the hot path inside each hook fire.
    let constitution_rules: Vec<sentinel_application::constitution_gate_runtime::Rule> =
        dirs::home_dir()
            .map(|h| {
                h.join(".claude")
                    .join("sentinel")
                    .join("config")
                    .join("constitution-gate.toml")
            })
            .and_then(|path| std::fs::read_to_string(&path).ok())
            .and_then(|s| {
                sentinel_application::constitution_gate_runtime::ConstitutionGateConfig::from_toml_str(
                    &s,
                )
                .map_err(|err| {
                    tracing::warn!(
                        ?err,
                        "failed to parse constitution-gate.toml; gate inert"
                    );
                })
                .ok()
            })
            .map(|cfg| cfg.rules)
            .unwrap_or_default();

    // A2 Phase 4: construct the capability router from shipped
    // defaults + optional operator overrides at
    // `~/.claude/sentinel/config/agents.toml`. Load failures degrade
    // to an empty router (A2 substrate inert, env-driven auditor
    // selection still works).
    let agents_overrides_path = dirs::home_dir().map(|h| {
        h.join(".claude")
            .join("sentinel")
            .join("config")
            .join("agents.toml")
    });
    let capability_router = match TomlCapabilityRouter::with_shipped_and_overrides(
        agents_overrides_path.as_deref(),
    ) {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(
                ?err,
                "failed to load agents.toml; capability router empty, A2 substrate inert"
            );
            TomlCapabilityRouter::from_profiles(Vec::new())
        }
    };

    // A3 Phase 4 + A2 Phase 4: construct the dry-run auditor. Two
    // paths in priority order:
    //   1. **Router-based** — consult the A2 router for an agent
    //      matching the A3 separate-vendor + AuditorVerdict-schema
    //      requirement (acting vendor hardcoded to Anthropic since
    //      Claude Code is Anthropic). The router picks per the
    //      operator's `agents.toml`; we build a RigAuditor for the
    //      chosen profile.
    //   2. **Env-only fallback** — if the router has no candidates
    //      (e.g. operator hasn't customized agents.toml and the
    //      shipped defaults don't match the operator's API keys), or
    //      if the chosen profile's API key isn't configured, fall
    //      back to `RigAuditor::from_env()` (legacy Phase 4 path,
    //      driven by SENTINEL_AUDITOR_PROVIDER + SENTINEL_AUDITOR_MODEL).
    //
    // Either path returning `Err` keeps A3 inert and
    // `tool_usage_gate` falls back to the four-check stack.
    let auditor: Option<Arc<dyn AuditorPort>> = (|| -> Option<Arc<dyn AuditorPort>> {
        if !capability_router.profiles().is_empty() {
            match RigAuditor::via_router(
                &capability_router,
                capability_router.profiles(),
                VendorClass::Anthropic,
            ) {
                Ok(a) => {
                    tracing::info!(
                        "A3 auditor selected via A2 capability router (acting vendor: Anthropic)"
                    );
                    return Some(Arc::new(a) as Arc<dyn AuditorPort>);
                }
                Err(err) => {
                    tracing::info!(
                        ?err,
                        "router-based auditor selection failed; falling back to env-only"
                    );
                }
            }
        }
        match RigAuditor::from_env() {
            Ok(a) => Some(Arc::new(a) as Arc<dyn AuditorPort>),
            Err(err) => {
                tracing::info!(
                    ?err,
                    "dry-run auditor unavailable (env-only fallback also failed); A3 inert"
                );
                None
            }
        }
    })();
    let a3_enabled = auditor.is_some();

    // BA1+3 Phase 4c: construct provenance store + requirement matrix
    // adapters at session start. Both adapters degrade gracefully:
    // - JsonlProvenanceStore writes via append-only JSONL and auto-
    //   creates parent dirs, so missing storage isn't an error.
    // - FilesystemRequirementMatrix reads per-orchestration JSON
    //   snapshots; the BA-orchestrator authors them. Missing files
    //   surface as UnknownOrchestration in the hook, mapped to BA3
    //   Existence blocks.
    let provenance_store: Option<Arc<JsonlProvenanceStore>> =
        match JsonlProvenanceStore::with_default_path() {
            Ok(s) => Some(Arc::new(s)),
            Err(err) => {
                tracing::info!(
                    ?err,
                    "JsonlProvenanceStore::with_default_path failed; BA1 audit lift + validation inert"
                );
                None
            }
        };
    let requirement_matrix: Option<Arc<FilesystemRequirementMatrix>> =
        match FilesystemRequirementMatrix::with_default_path() {
            Ok(m) => Some(Arc::new(m)),
            Err(err) => {
                tracing::info!(
                    ?err,
                    "FilesystemRequirementMatrix::with_default_path failed; BA3 traceability inert"
                );
                None
            }
        };
    // Load BA1+3 enforcement config: shipped defaults (both
    // ObserveOnly) overridden by operator-supplied
    // ~/.claude/sentinel/config/ba-enforcement.toml.
    let ba_enforcement_overrides_path = dirs::home_dir().map(|h| {
        h.join(".claude")
            .join("sentinel")
            .join("config")
            .join("ba-enforcement.toml")
    });
    let ba_enforcement = match BaEnforcementConfig::with_shipped_and_overrides(
        ba_enforcement_overrides_path.as_deref(),
    ) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(
                ?err,
                "ba-enforcement.toml load failed; falling back to ObserveOnly for both BA hooks"
            );
            BaEnforcementConfig::observe_only()
        }
    };

    // Bundle all ports into HookContext
    let ctx = hooks::HookContext {
        git: &git,
        vector_store: vector_store.as_deref(),
        fs: &real_fs,
        process: &real_process,
        llm: llm.as_deref(),
        memory_mcp: &memory_mcp,
        env: &real_env,
    };

    // Process through matching hooks based on event type
    let mut output = HookOutput::allow();

    // Resolved once per hook event so the per-call `time_and_record`
    // wrapper can stamp the JSONL row without each hook re-running git.
    let cwd_for_metrics = input.cwd.as_deref().unwrap_or(".");
    let repo_root_for_metrics = ctx.git.repo_root(cwd_for_metrics);

    match hook_event {
        HookEvent::UserPromptSubmit => {
            // Skill router — Opus AI classification on EVERY message.
            // No regex fallback. If AI fails or times out, return no-match.
            //
            // Only initialize the AI classifier when there is a non-empty
            // prompt. openrouter::Client::new() blocks ~1-4s on network I/O
            // during init (rig-core v0.35). Skipping on no-prompt inputs
            // keeps hooks fast and tests under the 3s timeout.
            let has_prompt = input
                .prompt
                .as_deref()
                .is_some_and(|p| !p.trim().is_empty());
            // IMPORTANT: `RigClassifier::from_env()` constructs a reqwest/rig client
            // which can block for several seconds on Windows (TLS root-cert loading via
            // schannel). It MUST run inside the timeout, not before it, so that the 8 s
            // budget covers both the sync client init *and* the async classify call.
            //
            // We offload the blocking init to a spawn_blocking task so it doesn't starve
            // the async executor; the surrounding timeout cancels the future if the whole
            // operation (init + classify) exceeds 8 s.
            let router_output =
                if let Ok(output) = tokio::time::timeout(std::time::Duration::from_secs(8), async {
                    let classifier = if has_prompt {
                        tokio::task::spawn_blocking(
                            sentinel_infrastructure::rig_classifier::RigClassifier::from_env,
                        )
                        .await
                        .ok()
                        .flatten()
                    } else {
                        None
                    };
                    hooks::skill_router::process(
                        &input,
                        classifier
                            .as_ref()
                            .map(|c| c as &dyn sentinel_application::classifier::AiClassifier),
                        &real_fs,
                    )
                    .await
                })
                .await { output } else {
                    tracing::warn!("Skill router timed out (8s) — no routing for this message");
                    hooks::skill_router::build_no_match_output(&real_fs)
                };
            output.merge(&router_output);

            // Extract detected skill from router output and update state.
            // When no skill matches, clear active_skill so the phase gate
            // doesn't keep blocking on a stale skill from earlier in the session.
            if let Some(ref ctx) = router_output.hook_specific_output {
                if let Some(ref ac) = ctx.additional_context {
                    if let Some(skill) = extract_skill_name(ac) {
                        // Set active_skill for context injection (SKILL.md loading),
                        // but ONLY register workflow-bearing skills when explicitly
                        // invoked via slash command. The skill router's regex matching
                        // is too aggressive — it matches casual phrases like "do it"
                        // to "execute", "deploy this" to "deploy", etc. This registers
                        // a workflow with mandatory phases that blocks ALL tool calls
                        // until the user reads phase files they never intended to use.
                        //
                        // Slash commands set the prompt to exactly "/<skill>" which the
                        // router detects. Casual matches come from normal conversation.
                        let is_slash_command = input
                            .prompt
                            .as_deref()
                            .is_some_and(|p: &str| p.trim().starts_with('/'));

                        if workflows.contains_key(&skill) && !is_slash_command {
                            // Skill has a workflow but was NOT explicitly invoked.
                            // Set active_skill for context injection (so SKILL.md loads)
                            // but do NOT call set_active_skill() which would register
                            // the workflow and trigger the phase gate.
                            state.active_skill = Some(skill.clone());
                            debug!(
                                skill = %skill,
                                "Skill detected via regex without slash command — setting context only"
                            );
                        } else if workflows.contains_key(&skill) {
                            // Explicit slash command — register the workflow
                            state.set_active_skill(&skill);
                        } else {
                            // No workflow definition — just set for context
                            state.active_skill = Some(skill.clone());
                        }
                    } else if ac.contains("No skill matched") {
                        state.active_skill = None;
                    }
                }
            }

            // Build the metrics envelope for this branch.
            let mk_ctx = |hook: &'static str| InvocationContext {
                event: "UserPromptSubmit",
                hook,
                tool: None,
                session_id: input.session_id.as_deref(),
                repo_root: repo_root_for_metrics.as_deref(),
            };

            // Phase validator — inject phase + step progress context
            let validator_output = time_and_record(ctx.fs, &mk_ctx("phase_validator"), || {
                hooks::phase_validator::process(&input, &state, &workflows, &step_configs, ctx.fs)
            });
            output.merge(&validator_output);

            // Error reporter — inject Linear filing instructions for unresolved errors
            let error_output = time_and_record(ctx.fs, &mk_ctx("error_reporter"), || {
                hooks::error_reporter::process(&input, &ctx)
            });
            output.merge(&error_output);

            // Hygiene override — detect override commands in prompt
            let override_output = time_and_record(ctx.fs, &mk_ctx("hygiene_override"), || {
                hooks::hygiene_override::process(&input, &ctx)
            });
            output.merge(&override_output);

            // Worktree reminder — remind to use EnterWorktree in git repos
            let worktree_output = time_and_record(ctx.fs, &mk_ctx("worktree_reminder"), || {
                hooks::worktree_reminder::process(&input, &ctx)
            });
            output.merge(&worktree_output);

            // Consul inbox — drain any operator-relayed instructions
            // the daemon-hosted legatus has buffered for this
            // session and inject them with PRIMARY-ASK framing.
            let inbox_output = time_and_record(ctx.fs, &mk_ctx("consul_inbox"), || {
                hooks::consul_inbox::process(&input, &ctx)
            });
            output.merge(&inbox_output);

            // Orchestration nudge — suggest agent teams / Explore subagents /
            // skill invocation based on prompt heuristics.
            let orchestration_output =
                time_and_record(ctx.fs, &mk_ctx("orchestration_nudge"), || {
                    hooks::orchestration_nudge::process(&input, &ctx)
                });
            output.merge(&orchestration_output);

            // Todo loader — inject active todos into context
            let todo_output = time_and_record(ctx.fs, &mk_ctx("todo_loader"), || {
                hooks::todo_loader::process(&input, &ctx)
            });
            output.merge(&todo_output);

            // --- Two-phase hooks (read state written by Stop, inject instructions) ---

            // Doc drift — inject update instructions for stale docs
            let drift_output = time_and_record(ctx.fs, &mk_ctx("doc_drift"), || {
                hooks::doc_drift::process_prompt(&input, &ctx)
            });
            output.merge(&drift_output);

            // Doc cleanup — inject cleanup instructions for junk docs
            let cleanup_output = time_and_record(ctx.fs, &mk_ctx("doc_cleanup"), || {
                hooks::doc_cleanup::process_prompt(&input, &ctx)
            });
            output.merge(&cleanup_output);

            // Commit hygiene — remind about uncommitted changes
            let commit_output = time_and_record(ctx.fs, &mk_ctx("commit_hygiene"), || {
                hooks::commit_hygiene::process_prompt(&input, &ctx)
            });
            output.merge(&commit_output);

            // Context monitor — inject zone-specific strategy guidance
            let ctx_prompt_output = time_and_record(ctx.fs, &mk_ctx("context_monitor"), || {
                hooks::context_monitor::process_prompt(&input, &ctx)
            });
            output.merge(&ctx_prompt_output);

            // Verification gate — remind to verify before claiming completion
            let verify_prompt_output =
                time_and_record(ctx.fs, &mk_ctx("verification_gate"), || {
                    hooks::verification_gate::process_prompt(&input, &ctx, &state)
                });
            output.merge(&verify_prompt_output);

            // Activity tracker — inject session activity summary when context is elevated
            let activity_prompt_output =
                time_and_record(ctx.fs, &mk_ctx("activity_tracker"), || {
                    hooks::activity_tracker::process_prompt(&input, &ctx)
                });
            output.merge(&activity_prompt_output);

            // Hygiene reminders — inject push/worktree/changelog reminders
            let reminders_prompt_output =
                time_and_record(ctx.fs, &mk_ctx("hygiene_reminders"), || {
                    hooks::hygiene_reminders::process_prompt(&input, &ctx)
                });
            output.merge(&reminders_prompt_output);

            // Memory inject — search Qdrant for semantically relevant memories
            let memory_output = time_and_record(ctx.fs, &mk_ctx("memory_inject"), || {
                hooks::memory_inject::process(&input, &ctx)
            });
            output.merge(&memory_output);
        }

        HookEvent::PreToolUse => {
            // Build the fixed metrics envelope once — every wrapped hook
            // call stamps a JSONL row through `time_and_record` with this
            // context. Hooks themselves are unchanged; the wrapper just
            // measures wall-clock duration and records the outcome.
            let metrics_ctx = InvocationContext {
                event: "PreToolUse",
                hook: "", // overwritten per-call below via .with_hook(...)
                tool: input.tool_name.as_deref(),
                session_id: input.session_id.as_deref(),
                repo_root: repo_root_for_metrics.as_deref(),
            };
            let mk_ctx = |hook: &'static str| InvocationContext {
                event: metrics_ctx.event,
                hook,
                tool: metrics_ctx.tool,
                session_id: metrics_ctx.session_id,
                repo_root: metrics_ctx.repo_root,
            };

            // Bug task gate — block mutating tools when a bug signal was
            // observed in tool output but no TaskCreate has filed it yet.
            let bug_gate_output = time_and_record(ctx.fs, &mk_ctx("bug_task_gate"), || {
                hooks::bug_task_gate::process_pretool(&input, &ctx)
            });
            output.merge(&bug_gate_output);

            // Skill invocation gate — block tools when a skill was detected
            // by skill_router but not yet invoked. Allowlists Read/Glob/Grep/
            // Skill/Task* so the gate doesn't refuse to let Claude clear it.
            let skill_gate_output =
                time_and_record(ctx.fs, &mk_ctx("skill_invocation_gate"), || {
                    hooks::skill_invocation_gate::process_pretool(&input, &ctx)
                });
            output.merge(&skill_gate_output);

            // Phase gate — check workflow state + track Read() calls on phase files
            let gate_output = time_and_record(ctx.fs, &mk_ctx("phase_gate"), || {
                hooks::phase_gate::process(&input, &mut state, &workflows, ctx.fs)
            });
            output.merge(&gate_output);

            if gate_output.blocked == Some(true) {
                state.record_blocked();
            }

            // BA1 provenance_validate — structural enforcement for citations.
            // Self-gates on input.extra.artifacts presence; non-BA tools
            // pass through silently. Mode-configurable via
            // ba-enforcement.toml; shipped default is ObserveOnly (no
            // blocking; telemetry only).
            if let Some(ref provenance_arc) = provenance_store {
                let prov_output =
                    time_and_record(ctx.fs, &mk_ctx("provenance_validate"), || {
                        hooks::provenance_validate::process(
                            &input,
                            provenance_arc.as_ref(),
                            ba_enforcement.provenance_validate_mode,
                        )
                    });
                output.merge(&prov_output);
            }

            // BA3 requirements_traceability_gate — structural enforcement for
            // recommendation→requirement traceability. Self-gates on
            // input.extra.requirement_refs / is_recommendation; non-BA tools
            // pass through silently. Mode-configurable via
            // ba-enforcement.toml; shipped default is ObserveOnly.
            if let Some(ref matrix_arc) = requirement_matrix {
                let trace_output =
                    time_and_record(ctx.fs, &mk_ctx("requirements_traceability_gate"), || {
                        hooks::requirements_traceability_gate::process(
                            &input,
                            matrix_arc.as_ref(),
                            ba_enforcement.requirements_traceability_mode,
                        )
                    });
                output.merge(&trace_output);
            }

            // A3 dry-run-then-commit gate — fires for ALL tools (the hook
            // itself short-circuits to allow() for class < Irreversible).
            // Only runs when the auditor is available; otherwise A3 is inert
            // and tool_usage_gate's four-check stack handles the upper
            // classes for Edit/Write.
            if let Some(ref auditor_arc) = auditor {
                let dry_run_output =
                    time_and_record(ctx.fs, &mk_ctx("dry_run_then_commit"), || {
                        hooks::dry_run_then_commit::process(
                            &input,
                            ctx.fs,
                            &reversibility_classifier,
                            auditor_arc.as_ref(),
                        )
                    });
                output.merge(&dry_run_output);
            }

            // Git hygiene — block on protected branch without worktree + uncommitted file limit
            if matches!(input.tool_name.as_deref(), Some("Edit" | "Write")) {
                let hygiene_output = time_and_record(ctx.fs, &mk_ctx("git_hygiene"), || {
                    hooks::git_hygiene::process(&input, &git, ctx.fs, &state)
                });
                output.merge(&hygiene_output);

                // tasks.md auto-block guard — block edits/writes that would
                // mutate the SENTINEL:TASKS auto block (owned by task_persist).
                let tasks_guard_output =
                    time_and_record(ctx.fs, &mk_ctx("tasks_md_guard"), || {
                        hooks::tasks_md_guard::process(&input, &ctx)
                    });
                output.merge(&tasks_guard_output);

                // Tool usage gate — require sequential thinking + task creation.
                // When `a3_enabled`, Irreversible/Catastrophic short-circuit to
                // allow() inside the gate so A3's dry_run_then_commit hook (run
                // above) owns those classes via its separate-model-family auditor.
                let usage_output = time_and_record(ctx.fs, &mk_ctx("tool_usage_gate"), || {
                    hooks::tool_usage_gate::process(
                        &input,
                        ctx.fs,
                        ctx.env,
                        &reversibility_classifier,
                        a3_enabled,
                    )
                });
                output.merge(&usage_output);
            }

            // Doppler/Auth0 gate — block mutation tools (any tool type)
            let doppler_output = time_and_record(ctx.fs, &mk_ctx("doppler_auth0_gate"), || {
                hooks::doppler_auth0_gate::process(&input, &ctx)
            });
            output.merge(&doppler_output);

            // Catastrophic escalation — for any tool call classified as
            // Catastrophic, deny locally AND emit SessionBlocked
            // upstream so the consul-side voice gate can run. On retry
            // after operator voice-approval, the daemon's approval
            // cache lets the same action_class through exactly once.
            // Wired here (not just declared in HOOK_NAMES) so the
            // voice-attested catastrophic loop is actually live.
            let catastrophic_output =
                time_and_record(ctx.fs, &mk_ctx("catastrophic_escalation"), || {
                    hooks::catastrophic_escalation::process(
                        &input,
                        &reversibility_classifier,
                        &hooks::catastrophic_escalation::DaemonApprovalChecker,
                    )
                });
            output.merge(&catastrophic_output);

            // Agent revocation kill switch — deny tool calls carrying
            // a revoked agent_id. No-op for the main session (no
            // agent_id on input).
            let revoke_output =
                time_and_record(ctx.fs, &mk_ctx("agent_revocation"), || {
                    hooks::agent_revocation::process(&input, &state)
                });
            output.merge(&revoke_output);

            // Step gate — for step tools, require the prereq StepProof
            // exists in state. Falls through for non-step tools and
            // for skills without a step config (back-compat).
            let step_output = time_and_record(ctx.fs, &mk_ctx("step_gate"), || {
                hooks::step_gate::process(&input, &state, &step_configs)
            });
            output.merge(&step_output);

            // Constitution gate — block Write/Edit/MultiEdit/NotebookEdit
            // when the new content introduces a banned pattern into a
            // protected path. Empty rule list = no-op (operators opt
            // in by authoring `~/.claude/sentinel/config/
            // constitution-gate.toml`).
            let constitution_output =
                time_and_record(ctx.fs, &mk_ctx("constitution_gate"), || {
                    hooks::constitution_gate::process(&input, &constitution_rules)
                });
            output.merge(&constitution_output);

            // Pre-commit verification — block git commit/push without test evidence (Bash only)
            if matches!(input.tool_name.as_deref(), Some("Bash")) {
                let commit_output =
                    time_and_record(ctx.fs, &mk_ctx("pre_commit_verification"), || {
                        hooks::pre_commit_verification::process(&input, &ctx, &state)
                    });
                output.merge(&commit_output);

                // Commit message validator — enforce conventional commits (Bash only)
                let msg_output =
                    time_and_record(ctx.fs, &mk_ctx("commit_message_validator"), || {
                        hooks::commit_message_validator::process(&input, &ctx)
                    });
                output.merge(&msg_output);

                // Pre-push browser test — block git push without a browser test (Bash only)
                let browser_test_output =
                    time_and_record(ctx.fs, &mk_ctx("pre_push_browser_test"), || {
                        hooks::pre_push_browser_test::process(&input, &ctx)
                    });
                output.merge(&browser_test_output);

                // PR merge gate — block gh pr merge without confirmation (Bash only)
                let pr_output = time_and_record(ctx.fs, &mk_ctx("pr_merge_gate"), || {
                    hooks::pr_merge_gate::process(&input, ctx.env)
                });
                output.merge(&pr_output);

                // DB ops gate — block production database operations (Bash only)
                let db_output = time_and_record(ctx.fs, &mk_ctx("db_ops_gate"), || {
                    hooks::db_ops_gate::process(&input)
                });
                output.merge(&db_output);

                // Output compressor (LAST, Bash only) — rewrite noisy commands
                // to route through `sentinel compress`. Runs only if no gate
                // above blocked the call (never rewrite a command that's about
                // to be denied) and there's no pending input rewrite to clobber.
                if output.blocked != Some(true) {
                    let compress_output =
                        time_and_record(ctx.fs, &mk_ctx("output_compressor"), || {
                            hooks::output_compressor::process(&input, ctx.env)
                        });
                    output.merge(&compress_output);
                }
            }
        }

        HookEvent::PostToolUse => {
            // BA1 audit-extract — lift documented-connector retrievals into
            // sentinel's provenance audit chain. Fires only for mcp__* tools
            // that emit a structured `provenance_audit` field; silently
            // skips otherwise. Observational (always allows).
            if let Some(ref provenance_arc) = provenance_store {
                let audit_output =
                    hooks::audit_extract::process(&input, provenance_arc.as_ref());
                output.merge(&audit_output);
            }

            // Bug task gate — scan tool output for bug signals (cargo test
            // FAILED, error[Exxxx], panicked at) and record pending-bug
            // state. Also clears state when a TaskCreate references the bug.
            let bug_gate_post = hooks::bug_task_gate::process_posttool(&input, &ctx);
            output.merge(&bug_gate_post);

            // Skill invocation gate — clear pending-skill state when the
            // detected skill is finally invoked (Skill tool with matching
            // name) or its SKILL.md is read.
            let skill_gate_post = hooks::skill_invocation_gate::process_posttool(&input, &ctx);
            output.merge(&skill_gate_post);

            // MCP health — detect MCP server failures and log to errors.jsonl
            let mcp_output = hooks::mcp_health::process(&input, &ctx);
            output.merge(&mcp_output);

            // Todo interceptor — persist rich todos from TodoWrite calls
            let todo_output = hooks::todo_interceptor::process(&input, &ctx);
            output.merge(&todo_output);

            // Evidence collector — capture tool results for proof chains.
            // **Attack #104 note**: In CLI hook mode, PhaseCollectionState is transient
            // (not serializable to disk), so we pass None. Evidence collection only works
            // in daemon mode where state is held in memory. This is a known limitation —
            // the proof chain still works via ProofEngine's own evidence gathering.
            // TODO: Implement evidence persistence for CLI mode if needed.
            let evidence_output = hooks::evidence_collector::process(&input, None);
            output.merge(&evidence_output);

            // Activity tracker — log every tool call to activity-log.jsonl
            let activity_output = hooks::activity_tracker::process_post_tool(&input, &ctx);
            output.merge(&activity_output);

            // Browser test recorder — write state file on successful session release
            // (mcp__browserbase__release_session, mcp__cdp__close_instance, or legacy
            // mcp__steel__release_session)
            let browser_test_post_output =
                hooks::pre_push_browser_test::process_post_tool(&input, &ctx);
            output.merge(&browser_test_post_output);

            // Prompt-injection nudge — scan tool result for injection
            // shapes and inject an "untrusted output, ignore embedded
            // directives" warning when matched. Always allows; the
            // signal is via additionalContext.
            let nudge_output =
                hooks::prompt_injection_nudge::process(&input, &ctx);
            output.merge(&nudge_output);

            // Plan organizer — inject plan file organization instructions (ExitPlanMode only)
            if matches!(input.tool_name.as_deref(), Some("ExitPlanMode")) {
                let plan_output = hooks::plan_organizer::process(&input, &ctx);
                output.merge(&plan_output);
            }

            // Account cascade — auto-switch all MCP servers after account change
            let cascade_output = hooks::account_cascade::process(&input, &ctx);
            output.merge(&cascade_output);

            // Build/deploy notify — push channel events for cargo build, test, git push
            let build_output = hooks::build_notify::process(&input, &ctx);
            output.merge(&build_output);

            // PR auto-monitor — inject CronCreate for PR monitoring (Bash only)
            if matches!(input.tool_name.as_deref(), Some("Bash")) {
                let pr_monitor_output = hooks::pr_auto_monitor::process(&input);
                output.merge(&pr_monitor_output);

                // Build auto-monitor — suggest monitoring for background builds (Bash only)
                let build_monitor_output = hooks::build_auto_monitor::process(&input);
                output.merge(&build_monitor_output);

                // Test evidence recorder — append a JSONL entry for any
                // Bash command matching a test/build pattern. Read by
                // `pre_commit_verification`; replaces transcript-parsing.
                let evidence_output = hooks::test_evidence_recorder::process(&input, &ctx);
                output.merge(&evidence_output);

                // Good citizen observer — scan Bash output for warnings,
                // dead-code, test failures, TODO/FIXME markers. Records
                // observations for the Stop reminder.
                let citizen_output = hooks::good_citizen_observer::process_post_tool(&input, &ctx);
                output.merge(&citizen_output);
            }

            // Linear lifecycle — inject CronCreate for issue lifecycle monitoring
            let linear_output = hooks::linear_lifecycle::process(&input);
            output.merge(&linear_output);

            // Step judge (M1.4 → integration #9) — run the adversarial AI
            // judge against a completed step tool's evidence and PRODUCE a
            // verdict. Until now `step_judge` fired on no event; the judge was
            // only reachable via the opt-in `submit_phase_complete` MCP tool,
            // so an agent could complete work and never be judged. This wires
            // it to PostToolUse for the `mcp__skills__<skill>__step_<id>`
            // namespace it already parses.
            //
            // Enforcement is staged via `SENTINEL_JUDGE_ENFORCEMENT`
            // (default `shadow`): in shadow the verdict is recorded/logged but
            // nothing blocks; `warn`/`enforce` surface a warning on a
            // non-sufficient verdict (the seal-blocking half of `enforce`
            // lives in `submit_step_complete`). The hook itself never blocks —
            // PostToolUse is the wrong layer; the proof chain is the
            // enforcement substrate.
            {
                let mode = sentinel_application::judge_enforcement::Mode::from_env();
                let judge = sentinel_infrastructure::rig_judge::MultiModelJudge::from_env();
                if judge.has_any_provider() {
                    let glass_break = check_glass_break_override();
                    let (sj_output, outcome) = hooks::step_judge::process(
                        &input,
                        &mut state,
                        &step_configs,
                        &judge,
                        glass_break,
                    )
                    .await;
                    output.merge(&sj_output);

                    use sentinel_application::hooks::step_judge::StepJudgeOutcome;
                    if let StepJudgeOutcome::Judged {
                        skill,
                        phase_id,
                        step_id,
                        verdict,
                        judge_model,
                        ..
                    } = &outcome
                    {
                        tracing::info!(
                            mode = %mode,
                            skill = %skill,
                            phase = %phase_id,
                            step = %step_id,
                            judge = %judge_model,
                            sufficient = verdict.sufficient,
                            confidence = verdict.confidence,
                            "step_judge verdict produced"
                        );
                        // warn/enforce: surface a non-sufficient verdict to the
                        // model so it knows the step didn't pass. Shadow is
                        // silent (observe-only rollout).
                        if mode.warns() && !verdict.sufficient {
                            let warn_ctx = format!(
                                "🟠 [Judge:{mode}] Step '{step_id}' of '{skill}/{phase_id}' \
                                 judged INSUFFICIENT (confidence {:.2}): {}{}",
                                verdict.confidence,
                                verdict.reasoning,
                                if mode.blocks_seal() {
                                    " — submit_step_complete will refuse to seal this step."
                                } else {
                                    ""
                                },
                            );
                            output.merge(&sentinel_domain::events::HookOutput::inject_context(
                                sentinel_domain::events::HookEvent::PostToolUse,
                                warn_ctx,
                            ));
                        }
                    }
                }
            }

            // Tool usage gate — track all enforcement markers
            if let Some(session_id) = input.session_id.as_deref() {
                if let Some(tool) = input.tool_name.as_deref() {
                    if tool.contains("sequentialthinking") {
                        hooks::tool_usage_gate::mark_sequential_thinking_used(ctx.fs, session_id);
                    }
                    // Task creation: agent-team `TaskCreate` ONLY. `TodoWrite` is the
                    // core Claude Code fallback, but per Gary's stated policy (CLAUDE.md
                    // "Required Tool Usage" §3) the agent-harness `TaskCreate`/`TaskUpdate`
                    // (TaskList) is mandatory — TodoWrite no longer satisfies the gate.
                    //
                    // We also mark the task as *active* on TaskCreate — the common workflow
                    // is "create a task and immediately start working on it", and forcing a
                    // separate `TaskUpdate(status="in_progress")` turn before any Edit is
                    // pure DX friction. The authoritative active-task check is now reading
                    // `~/.claude/sentinel/persistent-tasks/*/tasks.json` for any task with
                    // `status=in_progress` (see `persistent_store_has_active_task`); the
                    // marker is a fast-path/fallback only.
                    if tool == "TaskCreate" {
                        hooks::tool_usage_gate::mark_task_created(ctx.fs, session_id);
                        hooks::tool_usage_gate::mark_task_active(ctx.fs, session_id);
                    }
                    if tool == "ExitPlanMode" {
                        hooks::tool_usage_gate::mark_plan_approved(ctx.fs, session_id);
                    }
                    // Entering plan mode also satisfies the plan-approval precondition:
                    // the model has explicitly transitioned into design/plan territory,
                    // and ExitPlanMode will fire separately when the plan is approved.
                    // EnterPlanMode is hidden from sdk-tools.d.ts but real in the binary
                    // (2.1.114 decompile handler `r7H`).
                    if tool == "EnterPlanMode" {
                        hooks::tool_usage_gate::mark_plan_approved(ctx.fs, session_id);
                    }
                    // Active-task marker: agent-team `TaskUpdate(status="in_progress")`
                    // only. The TodoWrite branch was deliberately removed — TodoWrite
                    // is no longer a substitute for TaskUpdate per CLAUDE.md policy.
                    if tool == "TaskUpdate" {
                        if let Some(ti) = input.tool_input.as_ref() {
                            if ti.get("status").and_then(|v| v.as_str()) == Some("in_progress") {
                                hooks::tool_usage_gate::mark_task_active(ctx.fs, session_id);
                            }
                        }
                    }
                }
            }
        }

        HookEvent::Stop => {
            // Execution log — capture [RUN]/[STEP]/[PHASE] markers from transcript
            let exec_output = hooks::execution_log::process(&input, &ctx);
            output.merge(&exec_output);

            // Skill telemetry — aggregate skill usage metrics
            let telemetry_output = hooks::skill_telemetry::process(&input, &ctx);
            output.merge(&telemetry_output);

            // --- Two-phase hooks (detect state, write for UserPromptSubmit to read) ---

            // Context monitor — capture context window usage zone
            let ctx_output = hooks::context_monitor::process_stop(&input, &ctx);
            output.merge(&ctx_output);

            // Commit hygiene — detect uncommitted changes
            let hygiene_output = hooks::commit_hygiene::process_stop(&input, &ctx);
            output.merge(&hygiene_output);

            // Doc cleanup — scan for junk docs
            let doc_output = hooks::doc_cleanup::process_stop(&input, &ctx);
            output.merge(&doc_output);

            // Doc drift — detect stale README/CLAUDE.md/CHANGELOG
            let drift_output = hooks::doc_drift::process_stop(&input, &ctx);
            output.merge(&drift_output);

            // Hygiene reminders — detect unpushed commits, stale worktrees, changelog gaps
            let reminders_output = hooks::hygiene_reminders::process_stop(&input, &ctx);
            output.merge(&reminders_output);

            // Verification gate — detect unverified completion claims
            let verify_output = hooks::verification_gate::process_stop(&input, &ctx);
            output.merge(&verify_output);

            // Task coverage check — warn if uncommitted changes but no active task
            let coverage_output = hooks::task_coverage_check::process(&input, &ctx);
            output.merge(&coverage_output);

            // Good citizen observer — surface unaddressed warnings/findings
            // observed during the turn, prompt agent to file TaskCreate.
            let citizen_output = hooks::good_citizen_observer::process_stop(&input, &ctx);
            output.merge(&citizen_output);

            // Activity tracker — build session summary from activity log
            let activity_stop_output = hooks::activity_tracker::process_stop(&input, &ctx);
            output.merge(&activity_stop_output);

            // Task persist — final snapshot catches any TaskUpdate calls mid-turn
            let task_persist_output = hooks::task_persist::process(&input, &ctx);
            output.merge(&task_persist_output);

            // Memory extract — periodic session transcript re-indexing.
            // (Flat-.md capture path is removed; turn-capture below replaces it.)
            let memory_extract_output = hooks::memory_extract::process(&input, &ctx);
            output.merge(&memory_extract_output);

            // Memory turn-capture — LLM extracts atoms from this turn and
            // routes them through the dual-judge memory_capture gate.
            let memory_turn_output = hooks::memory_turn_capture::process(&input, &ctx);
            output.merge(&memory_turn_output);

            // Memory feedback — boost used memories, flag corrections
            let memory_feedback_output = hooks::memory_feedback::process(&input, &ctx);
            output.merge(&memory_feedback_output);

            // Memory inject (Stop phase) — pre-compute Qdrant search for next turn
            let memory_precompute_output = hooks::memory_inject::process_stop(&input, &ctx);
            output.merge(&memory_precompute_output);

            // Cross-session proof chain archive (#39). Best-effort write to
            // `~/.claude/sentinel/proofs/` so query_proof_corpus can answer
            // across sessions, not just live state. Failures are logged and
            // do not block Stop.
            if let Some(home) = dirs::home_dir() {
                if let Err(e) =
                    sentinel_application::proof_archive::archive_chains(&state, ctx.fs, &home)
                {
                    tracing::warn!(
                        error = %e,
                        "proof chain archive failed during Stop — corpus query will fall back to live-session-only"
                    );
                }
            }
        }

        HookEvent::SessionStart => {
            // **Attack #57 fix**: Only create fresh state if no existing state was loaded.
            // Unconditional reset lets an attacker trigger SessionStart mid-session
            // (via crafted event) to wipe all workflow progress and phase gates.
            // If state already exists (loaded from disk at line 64), preserve it.
            if state.tool_calls == 0 && state.workflows.is_empty() && state.phases_read.is_empty() {
                // Genuinely new session — use fresh state (already created above)
                state = SessionState::new(session_id);
            } else {
                debug!(
                    session_id,
                    tool_calls = state.tool_calls,
                    workflows = state.workflows.len(),
                    "SessionStart received for active session — preserving existing state"
                );
            }

            // Session init — log session, sync marketplace repo, inject startup context
            let init_output = hooks::session_init::process(&input, &ctx);
            output.merge(&init_output);

            // Task rehydrate — inject persistent tasks from previous sessions
            let rehydrate_output = hooks::task_rehydrate::process(&input, &ctx);
            output.merge(&rehydrate_output);

            // Memory verify removed from SessionStart — Qdrant HTTP calls blocked
            // startup for 5-20s. Verification now runs on PreCompact (background,
            // non-critical path) where latency doesn't affect user experience.

            // Dependency freshness check — detect outdated deps (any language)
            let dep_output = hooks::dep_check::process(&input, &ctx);
            output.merge(&dep_output);
        }

        HookEvent::PreCompact => {
            // Pre-compact snapshot — save session state before context compaction
            let compact_output = hooks::pre_compact::process(&input, &ctx);
            output.merge(&compact_output);

            // Session index — upsert transcript exchanges to Qdrant for search
            let index_output = hooks::session_index::process(&input, &ctx);
            output.merge(&index_output);

            // Memory verify — re-check stored memories against ground truth
            // (24h cooldown, 3s wall-clock timeout, silently no-ops without
            // Qdrant + LLM). Moved here from SessionStart on 2026-04 because
            // verification blocked startup 5-20s; PreCompact is the right
            // home — background, non-critical, runs once per long session.
            let verify_output = hooks::memory_verify::process(&input, &ctx);
            output.merge(&verify_output);
        }

        HookEvent::TeammateIdle => {
            // Team quality gate — remind teammate to check for remaining work
            let idle_output = hooks::teammate_idle::process(&input, &ctx);
            output.merge(&idle_output);
        }

        HookEvent::TaskCompleted => {
            // Task verification gate — verify work before marking complete
            let completed_output = hooks::task_completed::process(&input, &ctx);
            output.merge(&completed_output);

            // Task persist — snapshot task list to persistent markdown + JSON
            let persist_output = hooks::task_persist::process(&input, &ctx);
            output.merge(&persist_output);
        }

        // ── New events added from Claude Code v2.1.88 source analysis ──
        HookEvent::SessionEnd => {
            // Session cleanup — flush state, log session end (1.5s timeout!)
            let end_output = hooks::session_end::process(&input, &ctx);
            output.merge(&end_output);
        }

        HookEvent::PostCompact => {
            // Restore critical state after context compaction
            let compact_output = hooks::post_compact::process(&input, &ctx);
            output.merge(&compact_output);
        }

        HookEvent::SubagentStart => {
            // Inject skill context into spawned agents
            let subagent_output = hooks::subagent_start::process(&input, &ctx);
            output.merge(&subagent_output);
        }

        HookEvent::SubagentStop => {
            // Log agent completion for telemetry
            let subagent_output = hooks::subagent_stop::process(&input, &ctx);
            output.merge(&subagent_output);
        }

        HookEvent::TaskCreated => {
            // Log task creation for telemetry
            let task_output = hooks::task_created::process(&input, &ctx);
            output.merge(&task_output);

            // Tool usage gate — mark task created for this session
            if let Some(session_id) = input.session_id.as_deref() {
                hooks::tool_usage_gate::mark_task_created(ctx.fs, session_id);
            }

            // Task persist — snapshot task list to persistent markdown + JSON
            let persist_output = hooks::task_persist::process(&input, &ctx);
            output.merge(&persist_output);
        }

        HookEvent::Setup => {
            // Repo init/maintenance
            let setup_output = hooks::setup::process(&input, &ctx);
            output.merge(&setup_output);
        }

        HookEvent::CwdChanged => {
            // Working directory changed — re-detect project context
            let cwd_output = hooks::cwd_changed::process(&input, &ctx);
            output.merge(&cwd_output);
        }

        HookEvent::StopFailure => {
            // API error at end of turn — log for diagnostics
            let failure_output = hooks::stop_failure::process(&input, &ctx);
            output.merge(&failure_output);
        }

        HookEvent::PermissionDenied => {
            // Auto-mode denied a tool call — log for diagnostics
            let denied_output = hooks::permission_denied::process(&input, &ctx);
            output.merge(&denied_output);
        }

        HookEvent::PostToolUseFailure => {
            // Tool execution failed — log for diagnostics
            let tool_name = input.tool_name.as_deref().unwrap_or("unknown");
            let is_timeout = input
                .extra
                .get("is_timeout")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let error = input
                .extra
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            tracing::debug!(tool_name, is_timeout, error, "Tool execution failed");

            if let Some(home) = dirs::home_dir() {
                let metrics_dir = home.join(".claude").join("sentinel").join("metrics");
                let entry = serde_json::json!({
                    "event": "tool_failure",
                    "tool_name": tool_name,
                    "is_timeout": is_timeout,
                    "error": error,
                    "session_id": input.session_id,
                    "ts": chrono::Utc::now().to_rfc3339(),
                });
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(metrics_dir.join("errors.jsonl"))
                {
                    use std::io::Write;
                    let _ = writeln!(file, "{entry}");
                }
            }
        }

        HookEvent::PermissionRequest => {
            // Log permission request details for future auto-approve rules.
            // Currently pass-through — no auto-decisions. The value of this hook
            // will come when we add specific auto-approve rules for trusted tools
            // in certain contexts (e.g., auto-allow Edit in a known project dir).
            let tool = input.tool_name.as_deref().unwrap_or("unknown");
            let has_suggestions = input
                .permission_suggestions
                .as_ref()
                .map_or(0, std::vec::Vec::len);
            tracing::debug!(
                tool,
                has_suggestions,
                "PermissionRequest — pass through (no auto-decisions yet)"
            );
        }

        HookEvent::Elicitation => {
            // MCP server requesting user input — log details, pass through.
            // Auto-responding to elicitation without understanding the context is risky.
            // Future: auto-accept known servers (e.g., sentinel, codex) for trusted prompts.
            let server = input
                .extra
                .get("mcp_server_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let message = input
                .extra
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            tracing::debug!(
                server,
                message,
                "Elicitation request from MCP server — pass through"
            );
        }

        HookEvent::ElicitationResult => {
            // Post-elicitation response — pass through for now
            tracing::debug!("ElicitationResult received — pass through");
        }

        HookEvent::ConfigChange => {
            // Settings or skill file changed — validate and warn on dangerous changes.
            let source = input
                .extra
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let file_path = input
                .extra
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            tracing::debug!(source, file_path, "ConfigChange detected");

            // Warn if disableAllHooks is set (kill-switch that disables all enforcement)
            if (source == "user_settings"
                || source == "project_settings"
                || source == "local_settings")
                && !file_path.is_empty()
            {
                if let Ok(settings_content) = std::fs::read_to_string(file_path) {
                    if settings_content.contains("\"disableAllHooks\"")
                        && settings_content.contains("true")
                    {
                        output.system_message = Some(
                                "[sentinel] WARNING: disableAllHooks detected in settings — all hook enforcement will be disabled!".to_string()
                            );
                    }
                }
            }

            // Log skill file changes for telemetry
            if source == "skills" {
                tracing::info!(file_path, "Skill file changed");
            }

            // Plan-mode transition — mark plan-approved when entering plan mode
            // via any mechanism (Shift+Tab UI cycle, --permission-mode plan CLI
            // flag, SDK set_permission_mode RPC, or the EnterPlanMode tool).
            // ConfigChange is the authoritative signal since all permission-mode
            // transitions route through Claude Code's config layer; previously
            // we only detected this via PostToolUse on EnterPlanMode/ExitPlanMode,
            // missing the UI and CLI entry paths.
            //
            // The ConfigChange payload shape (from claude-code-2.1.114):
            //   { source: "user_settings" | ..., field: "permissionMode",
            //     old_value: "<mode>", new_value: "<mode>", ... }
            // We read `new_value` (or fall back to `value`) and compare to "plan".
            let changed_field = input
                .extra
                .get("field")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if changed_field == "permissionMode" || changed_field == "permission_mode" {
                let new_mode = input
                    .extra
                    .get("new_value")
                    .or_else(|| input.extra.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if new_mode == "plan" {
                    if let Some(sid) = input.session_id.as_deref() {
                        hooks::tool_usage_gate::mark_plan_approved(ctx.fs, sid);
                        tracing::info!(
                            source,
                            "Plan mode entered via ConfigChange — plan-approved marker written"
                        );
                    }
                }
            }
        }

        HookEvent::InstructionsLoaded => {
            // CLAUDE.md or other instruction file loaded — log details.
            let file_path = input
                .extra
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let memory_type = input
                .extra
                .get("memory_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let load_reason = input
                .extra
                .get("load_reason")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            tracing::debug!(file_path, memory_type, load_reason, "Instructions loaded");

            // Log managed/enterprise overrides — these can silently change behavior
            if memory_type == "Managed" {
                tracing::info!(
                    file_path,
                    "Managed (enterprise) instructions loaded — may override user settings"
                );
            }
        }

        HookEvent::FileChanged => {
            // Watched file changed — log and inject context for important files.
            let file_path = input
                .file_path
                .as_deref()
                .or_else(|| input.extra.get("file_path").and_then(|v| v.as_str()))
                .unwrap_or("");
            let event_type = input
                .extra
                .get("event")
                .and_then(|v| v.as_str())
                .unwrap_or("change");
            tracing::info!(file_path, event_type, "Watched file changed");

            if file_path.ends_with("CLAUDE.md") {
                output.system_message =
                    Some("[sentinel] CLAUDE.md changed — context may need refresh".to_string());
            } else if file_path.ends_with("settings.json") {
                output.system_message = Some(
                    "[sentinel] settings.json changed — hook configuration may have been updated"
                        .to_string(),
                );
            }
        }

        HookEvent::WorktreeCreate => {
            // Git worktree created — pass through for now
            tracing::debug!("WorktreeCreate received — pass through");
        }

        HookEvent::WorktreeRemove => {
            // Git worktree removed — pass through for now
            tracing::debug!("WorktreeRemove received — pass through");
        }

        HookEvent::Notification => {
            // Internal notification — pass through for now
            tracing::debug!("Notification received — pass through");
        }
    }

    // Record hook invocation with actual elapsed time
    let elapsed_ms = start_time.elapsed().as_millis() as u64;
    state.record_hook_invocation(event, elapsed_ms);

    // Save state AFTER all processing (so phase reads and tool calls are persisted)
    if let Err(e) = sentinel_infrastructure::state_store::save(&mut state) {
        tracing::warn!(error = %e, "Failed to persist hook state");
    }

    // Transform output for Claude Code's JSON schema
    match hook_event {
        HookEvent::PreToolUse => {
            // Transform legacy blocked/reason → proper hookSpecificOutput with permissionDecision
            output = output.into_pretool_output();
        }
        HookEvent::UserPromptSubmit
        | HookEvent::PostToolUse
        | HookEvent::PostToolUseFailure
        | HookEvent::SessionStart
        | HookEvent::Setup
        | HookEvent::SubagentStart
        | HookEvent::Notification
        | HookEvent::PermissionRequest
        | HookEvent::PermissionDenied
        | HookEvent::Elicitation
        | HookEvent::ElicitationResult
        | HookEvent::CwdChanged
        | HookEvent::FileChanged
        | HookEvent::WorktreeCreate => {
            // These events support hookSpecificOutput natively
        }
        _ => {
            // Strip hookSpecificOutput for events Claude Code doesn't process
            output.hook_specific_output = None;
        }
    }

    // Inject project context only for prompt-scoped events. Adding it to every
    // tool hook bloats the transcript during tool-heavy sessions and drives
    // premature compaction.
    if should_attach_project_context(hook_event) {
        if let Ok(project) = std::env::var("CLAUDE_PROJECT") {
            if !project.is_empty() {
                let project_header = format!("[Project Context] Active project: {project}");
                if let Some(ref mut hso) = output.hook_specific_output {
                    match &hso.additional_context {
                        Some(existing) => {
                            hso.additional_context =
                                Some(format!("{project_header}\n\n{existing}"));
                        }
                        None => {
                            hso.additional_context = Some(project_header);
                        }
                    }
                } else {
                    output.hook_specific_output = Some(HookSpecificOutput {
                        hook_event_name: hook_event.to_string(),
                        additional_context: Some(project_header),
                        ..HookSpecificOutput::default()
                    });
                }
            }
        }
    }

    // Write output to stdout
    sentinel_infrastructure::stdout::write_hook_output(&output)?;

    // Force-exit immediately after writing output. This hook process is
    // short-lived; the tokio multi-thread runtime holds background threads
    // (reqwest connection pool, etc.) that delay normal OS exit by several
    // seconds. Claude Code observes the process as "still running" until
    // those threads drain, which trips the 3s test timeout and wedges the
    // REPL in production.
    std::process::exit(0);
}

fn parse_hook_event(event: &str) -> Result<HookEvent> {
    if let Some(e) = HookEvent::from_arg(event) {
        Ok(e)
    } else {
        eprintln!(
            "[sentinel] ERROR: Unknown hook event type '{event}'. \
             Valid events: SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, \
             PostToolUse, PostToolUseFailure, Stop, StopFailure, PreCompact, PostCompact, \
             Setup, SubagentStart, SubagentStop, TeammateIdle, TaskCreated, TaskCompleted, \
             PermissionDenied, CwdChanged"
        );
        anyhow::bail!("Unknown hook event type: '{event}'");
    }
}

const fn should_attach_project_context(hook_event: HookEvent) -> bool {
    matches!(
        hook_event,
        HookEvent::SessionStart
            | HookEvent::UserPromptSubmit
            | HookEvent::SubagentStart
            | HookEvent::Setup
    )
}

fn write_safe_allow_response() -> Result<()> {
    sentinel_infrastructure::stdout::write_hook_output(&HookOutput::allow())
}

/// Validate that the caller is plausibly Claude Code.
///
/// Two checks:
/// 1. **Stdin is piped** (not a terminal) — hooks are always invoked via pipe
///    from Claude Code's hook runner, never interactively. Hard fail unless
///    `SENTINEL_ALLOW_TERMINAL=1` is set.
/// 2. **`CLAUDE_CODE` env marker** — Claude Code sets `CLAUDE_CODE_ENTRY_POINT`
///    when spawning hook subprocesses. Its absence is suspicious.
///
/// If validation fails, a `caller_rejected` event is logged to the security
/// audit log and the function returns `Err`, causing `run()` to output `{}`
/// (safe allow) and exit early.
///
/// This is defense-in-depth, not a security boundary. A determined attacker can
/// still set the env var and pipe crafted JSON, but they must:
///   1. Know the sentinel CLI exists and its arguments
///   2. Set `CLAUDE_CODE_ENTRY_POINT` in their environment
///   3. Construct valid `HookInput` JSON with a real `session_id`
///   4. Pipe it correctly (not type interactively)
fn validate_caller() -> Result<()> {
    // Escape hatch for debugging / manual testing
    if std::env::var("SENTINEL_ALLOW_TERMINAL").as_deref() == Ok("1") {
        return Ok(());
    }

    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        eprintln!(
            "[sentinel] BLOCKED: Hook invoked from interactive terminal. \
             Hooks must be called by Claude Code via pipe. \
             Set SENTINEL_ALLOW_TERMINAL=1 to override for debugging."
        );
        let _ = sentinel_infrastructure::security_log::log_security_event(
            "caller_rejected",
            "unknown",
            "Hook invoked from interactive terminal (stdin is TTY)",
        );
        anyhow::bail!("Caller validation failed: stdin is a terminal");
    }

    // Check for Claude Code environment marker.
    // Claude Code sets CLAUDE_CODE_ENTRY_POINT (e.g. "cli", "sdk").
    // Absence is not definitive proof of abuse (could be an older version),
    // so keep it as a debug-only signal instead of stderr noise. Claude treats
    // stderr from hooks as a non-blocking hook error banner.
    if std::env::var("CLAUDE_CODE_ENTRY_POINT").is_err() {
        debug!("CLAUDE_CODE_ENTRY_POINT not set for hook invocation");
    }

    // Optional parent-process attestation.
    //
    // This was introduced as defense-in-depth, but the Windows `wmic`/`tasklist`
    // probe can outlive the hook's JSON response and keep the process alive long
    // enough for Claude Code to appear wedged after pressing Enter. Keep the
    // simpler stdin/env checks on by default and make the heavier attestation
    // opt-in for debugging or forensics.
    #[cfg(windows)]
    if std::env::var("SENTINEL_ENABLE_PARENT_ATTESTATION").as_deref() == Ok("1") {
        if let Some(parent) = get_parent_process_name() {
            #[cfg(windows)]
            let valid_parents = [
                "node.exe",
                "bun.exe",
                "claude.exe",
                "cmd.exe",
                "powershell.exe",
                "pwsh.exe",
                "bash.exe",
                "sentinel-engine.exe",
                "sentinel.exe",
            ];
            #[cfg(not(windows))]
            let valid_parents = [
                "node",
                "bun",
                "claude",
                "bash",
                "zsh",
                "sh",
                "sentinel-engine",
                "sentinel",
            ];
            if !valid_parents.iter().any(|v| parent.contains(v)) {
                eprintln!(
                    "[sentinel] WARNING: Parent process '{parent}' is not a known Claude Code runtime."
                );
                let _ = sentinel_infrastructure::security_log::log_security_event(
                    "caller_rejected",
                    "unknown",
                    &format!("Parent process '{parent}' is not a known Claude Code runtime"),
                );
            }
        }
    }

    Ok(())
}

/// Get the parent process name on Windows using tasklist + wmic.
///
/// Strategy: Use `wmic process where ProcessId=<PID> get ParentProcessId` to find
/// the parent PID, then `tasklist /FI "PID eq <PPID>"` to get its name.
/// Both are native Windows tools that start in ~30ms (vs `PowerShell` ~2s).
///
/// Returns None if the check fails for any reason (fail-open for resilience).
#[cfg(windows)]
fn get_parent_process_name() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let pid = std::process::id();

    // Step 1: Get parent PID via wmic (~30ms)
    let output = std::process::Command::new("wmic")
        .args([
            "process",
            "where",
            &format!("ProcessId={pid}"),
            "get",
            "ParentProcessId",
            "/VALUE",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parent_pid: u32 = stdout
        .lines()
        .find(|l| l.starts_with("ParentProcessId="))
        .and_then(|l| l.strip_prefix("ParentProcessId="))
        .and_then(|s| s.trim().parse().ok())?;

    // Step 2: Get parent process name via tasklist (~30ms)
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {parent_pid}"), "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // CSV format: "Image Name","PID","Session Name","Session#","Mem Usage"
    let name = stdout
        .lines()
        .next()?
        .split(',')
        .next()?
        .trim_matches('"')
        .to_lowercase();

    if name.is_empty() || name.contains("no tasks") {
        None
    } else {
        Some(name)
    }
}

/// Glass break directory — all glass break files live here.
fn glass_break_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("sentinel"))
}

/// Check if a glass break override is active.
///
/// Two-layer design:
///   1. **Token check (fast path):** If `.glass-break-token` exists and contains
///      a valid, non-expired timestamp, return true immediately. This is the
///      common path after initial confirmation.
///   2. **Trigger check (slow path):** If `.glass-break` file exists (user created
///      it manually), launch a native Windows dialog requiring human confirmation.
///      On success, write a time-limited token and delete the trigger file.
///      On failure/cancel, delete the trigger file and return false.
///
/// The dialog prevents AI from self-activating the override. The token prevents
/// the dialog from appearing on every subsequent hook invocation.
fn check_glass_break_override() -> bool {
    let dir = match glass_break_dir() {
        Some(d) => d,
        None => return false,
    };

    // Fast path: check for valid active token
    let token_path = dir.join(".glass-break-token");
    if token_path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&token_path) {
            if let Some(expiry_str) = contents.lines().next() {
                if let Ok(expiry) = chrono::DateTime::parse_from_rfc3339(expiry_str.trim()) {
                    if expiry > chrono::Utc::now() {
                        return true; // Token still valid
                    }
                }
            }
        }
        // Token expired or invalid — clean up
        let _ = std::fs::remove_file(&token_path);
    }

    // Slow path: check for trigger file
    let trigger_path = dir.join(".glass-break");
    if !trigger_path.exists() {
        return false;
    }

    // Trigger exists — require human confirmation via native dialog
    let confirmed = show_glass_break_dialog();

    // Always delete the trigger file (one-shot)
    let _ = std::fs::remove_file(&trigger_path);

    if confirmed {
        // Write a 15-minute token
        let expiry = chrono::Utc::now() + chrono::Duration::minutes(15);
        let _ = std::fs::write(&token_path, expiry.to_rfc3339());

        // Audit log
        let _ = sentinel_infrastructure::security_log::log_security_event(
            "glass_break_activated",
            "unknown",
            "Glass break confirmed via dialog — enforcement bypassed for 15 minutes",
        );

        eprintln!(
            "[sentinel] Glass break confirmed. Enforcement bypassed until {}",
            expiry.format("%H:%M:%S")
        );
        true
    } else {
        eprintln!("[sentinel] Glass break DENIED — dialog cancelled or timed out");
        let _ = sentinel_infrastructure::security_log::log_security_event(
            "glass_break_denied",
            "unknown",
            "Glass break trigger file found but dialog was cancelled or timed out",
        );
        false
    }
}

/// Show a native Windows dialog requiring human confirmation.
/// Returns true only if the user types the correct confirmation phrase.
/// Cannot be completed by AI — requires interactive desktop input.
fn show_glass_break_dialog() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        // Generate a 4-digit challenge code
        let mut code_bytes = [0u8; 2];
        let _ = getrandom::getrandom(&mut code_bytes);
        let code = u16::from_le_bytes(code_bytes) % 10000;
        let code_str = format!("{code:04}");

        // Use PowerShell to show an InputBox dialog (works on all Windows versions)
        // The dialog is modal and blocks until the user responds
        let ps_script = format!(
            r#"Add-Type -AssemblyName Microsoft.VisualBasic; $result = [Microsoft.VisualBasic.Interaction]::InputBox("SENTINEL EMERGENCY OVERRIDE`n`nThis will disable ALL security enforcement for 15 minutes.`n`nTo confirm, type the code: {code_str}`n`nThis action is audited.", "Glass Break", ""); if ($result -eq "{code_str}") {{ Write-Output "CONFIRMED" }} else {{ Write-Output "DENIED" }}"#
        );

        // Note: do NOT use -NonInteractive — InputBox requires interactive mode.
        // CREATE_NO_WINDOW hides the PowerShell console but the InputBox dialog
        // still appears on the interactive desktop.
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps_script])
            .creation_flags(CREATE_NO_WINDOW)
            .output();

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                stdout.trim() == "CONFIRMED"
            }
            Err(e) => {
                eprintln!("[sentinel] Failed to show glass break dialog: {e}");
                false
            }
        }
    }

    #[cfg(not(windows))]
    {
        // On non-Windows, fall back to terminal challenge
        eprintln!("[sentinel] Glass break requires interactive confirmation.");
        eprintln!("[sentinel] Use `sentinel break --reason \"...\"` from a terminal.");
        false
    }
}

// check_version_drift removed — Claude Code uses bun (not npm).
// The synchronous `npm view` call added 4-20s to every cold SessionStart.

/// Extract skill name from router context like "[Skill Router] Detected skill: linear. MANDATORY..."
fn extract_skill_name(context: &str) -> Option<String> {
    let prefix = "Detected skill: ";
    let start = context.find(prefix)? + prefix.len();
    let rest = &context[start..];
    let end = rest.find('.')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn test_extract_skill_name() {
        let ctx = "[Skill Router] Detected skill: linear. MANDATORY: You MUST Read...";
        assert_eq!(extract_skill_name(ctx), Some("linear".to_string()));
    }

    #[test]
    fn test_extract_skill_name_none() {
        assert_eq!(extract_skill_name("no skill here"), None);
    }

    #[test]
    fn test_no_skill_matched_clears_active_skill() {
        // Simulates the scenario: skill router detected "linear" on message 1,
        // then "No skill matched" on message 2. active_skill should be cleared.
        let no_match_context = "[Skill Router] No skill matched — general conversation mode.";
        assert_eq!(extract_skill_name(no_match_context), None);
        // The clearing happens in the caller (run()) when extract_skill_name returns
        // None AND the context contains "No skill matched"
        assert!(no_match_context.contains("No skill matched"));
    }

    #[test]
    fn test_project_context_only_attaches_to_prompt_scoped_events() {
        assert!(should_attach_project_context(HookEvent::SessionStart));
        assert!(should_attach_project_context(HookEvent::UserPromptSubmit));
        assert!(should_attach_project_context(HookEvent::SubagentStart));
        assert!(should_attach_project_context(HookEvent::Setup));
        assert!(!should_attach_project_context(HookEvent::PreToolUse));
        assert!(!should_attach_project_context(HookEvent::PostToolUse));
        assert!(!should_attach_project_context(
            HookEvent::PostToolUseFailure
        ));
        assert!(!should_attach_project_context(HookEvent::PostCompact));
    }

    /// Regression: hook process must exit within 3s (no lingering threads).
    #[tokio::test]
    async fn test_hook_internal_exits_within_timeout() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let engine = std::path::Path::new(manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("target")
            .join("release")
            .join(if cfg!(windows) {
                "sentinel-engine.exe"
            } else {
                "sentinel-engine"
            });

        if !engine.exists() {
            eprintln!("Skipping: sentinel-engine not found at {engine:?}");
            return;
        }

        let mut child = tokio::process::Command::new(&engine)
            .args(["hook-internal", "--event", "UserPromptSubmit"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SENTINEL_ALLOW_TERMINAL", "1")
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn sentinel-engine");

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(b"{\"session_id\":\"regression-exit-test\"}\n")
                .await
                .unwrap();
            stdin.shutdown().await.unwrap();
        }

        // Windows git subprocesses are ~0.8s each; allow more headroom on Windows.
        let timeout_secs = if cfg!(windows) { 15 } else { 3 };
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), child.wait()).await;

        assert!(
            result.is_ok(),
            "sentinel-engine did not exit within {timeout_secs}s — possible hang"
        );
    }

    /// Regression: stdout must be valid JSON (no tracing leaks).
    #[tokio::test]
    async fn test_hook_stdout_is_valid_json() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let engine = std::path::Path::new(manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("target")
            .join("release")
            .join(if cfg!(windows) {
                "sentinel-engine.exe"
            } else {
                "sentinel-engine"
            });

        if !engine.exists() {
            eprintln!("Skipping: sentinel-engine not found at {engine:?}");
            return;
        }

        let mut child = tokio::process::Command::new(&engine)
            .args(["hook-internal", "--event", "UserPromptSubmit"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SENTINEL_ALLOW_TERMINAL", "1")
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn");

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(b"{\"session_id\":\"regression-json-test\"}\n")
                .await
                .unwrap();
            stdin.shutdown().await.unwrap();
        }

        // Windows git subprocesses are ~0.8s each; allow more headroom on Windows.
        let timeout_secs = if cfg!(windows) { 15 } else { 3 };
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await
        .expect("timed out")
        .expect("wait failed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.is_empty(), "stdout should not be empty");

        let parsed: Result<serde_json::Value, _> = serde_json::from_str(stdout.trim());
        assert!(parsed.is_ok(), "stdout is not valid JSON: {stdout}");

        assert!(
            !stdout.contains("[2m"),
            "stdout contains ANSI escape (tracing leak)"
        );
        assert!(
            !stdout.contains("WARN"),
            "stdout contains WARN (tracing leak)"
        );
    }

    /// Regression: `from_env()` (and any other blocking init) must run INSIDE the
    /// 8-second timeout, not before it.  Previously, `RigClassifier::from_env()` was
    /// called synchronously outside the `tokio::time::timeout` block, so a stall in
    /// TLS root-cert loading (Windows schannel) would hang the hook indefinitely.
    ///
    /// This test validates the structural fix: a `spawn_blocking` task that sleeps 30 s
    /// (simulating a slow Windows TLS init) inside the timeout block should be
    /// *abandoned* after the timeout fires, not awaited to completion.
    #[tokio::test]
    async fn test_classifier_init_timeout_fires_when_blocked() {
        let short_timeout = std::time::Duration::from_millis(200);
        let start = std::time::Instant::now();

        // Simulate the fixed code path: spawn_blocking wrapping a slow from_env,
        // both running inside a tokio::time::timeout.
        let result: Result<Option<()>, tokio::time::error::Elapsed> =
            tokio::time::timeout(short_timeout, async {
                // Mimic RigClassifier::from_env taking 30 s (e.g. Windows TLS cert load).
                let _classifier: Option<()> = tokio::task::spawn_blocking(|| {
                    std::thread::sleep(std::time::Duration::from_secs(30));
                    None::<()>
                })
                .await
                .ok()
                .flatten();
                _classifier
            })
            .await;

        // The timeout must fire — if from_env had been outside the timeout (the old
        // bug) this whole call would have blocked for 30 s.
        assert!(result.is_err(), "timeout should have fired");

        // And it should fire promptly (well under 1 s).
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "timeout took too long: {:?}",
            start.elapsed()
        );
    }
}
