//! Hook Engine
//!
//! Core orchestrator: resolves dependencies, executes hooks in parallel batches,
//! merges outputs. Single entry point for all hook events.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use sentinel_domain::dependency::{self, ExecutionPlan};
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sentinel_domain::hooks::{HookId, HookSpec};
use sentinel_domain::state::SessionState;

/// Port for hook execution — infrastructure implements this
#[async_trait::async_trait]
pub trait HookExecutor: Send + Sync {
    /// Execute a single hook and return its output
    async fn execute(&self, hook_id: &HookId, input: &HookInput) -> Result<HookOutput>;
}

/// The hook engine — resolves deps, runs hooks, merges outputs
pub struct HookEngine {
    /// Registered hook specs
    specs: Vec<HookSpec>,

    /// Resolved execution plan (computed once on startup)
    plan: ExecutionPlan,

    /// Hook executor (infrastructure layer)
    executor: Arc<dyn HookExecutor>,

    /// Shared session state
    state: Arc<RwLock<SessionState>>,
}

impl HookEngine {
    /// Create a new engine from hook specs
    pub fn new(
        specs: Vec<HookSpec>,
        executor: Arc<dyn HookExecutor>,
        state: Arc<RwLock<SessionState>>,
    ) -> Result<Self> {
        let plan = dependency::resolve(&specs)?;
        info!(
            "Hook engine initialized: {} hooks, {} batches",
            specs.len(),
            plan.batches.len()
        );
        Ok(Self {
            specs,
            plan,
            executor,
            state,
        })
    }

    /// Process a hook event — runs all matching hooks in dependency order
    pub async fn process(&self, event: HookEvent, input: &HookInput) -> Result<HookOutput> {
        let start = std::time::Instant::now();

        // Filter specs that match this event
        let matching: Vec<&HookSpec> = self
            .specs
            .iter()
            .filter(|s| s.matches(event, input.tool_name.as_deref()))
            .collect();

        if matching.is_empty() {
            debug!(?event, "No hooks match event");
            return Ok(HookOutput::allow());
        }

        let matching_ids: std::collections::HashSet<&HookId> =
            matching.iter().map(|s| &s.id).collect();

        // Execute batches in order, hooks within a batch in parallel
        let mut merged = HookOutput::allow();

        for batch in &self.plan.batches {
            let batch_hooks: Vec<&HookId> = batch
                .iter()
                .filter(|id| matching_ids.contains(id))
                .collect();

            if batch_hooks.is_empty() {
                continue;
            }

            // Run batch hooks in parallel
            let mut handles = Vec::new();
            for hook_id in batch_hooks {
                let executor = self.executor.clone();
                let id = hook_id.clone();
                let inp = input.clone();
                handles.push(tokio::spawn(async move {
                    let result = executor.execute(&id, &inp).await;
                    (id, result)
                }));
            }

            // Collect results
            for handle in handles {
                let (id, result) = handle.await?;
                match result {
                    Ok(output) => {
                        let duration = start.elapsed().as_millis() as u64;
                        self.state
                            .write()
                            .await
                            .record_hook_invocation(id.as_str(), duration);

                        let is_blocked = output.blocked == Some(true);
                        merged.merge(&output);

                        // If any hook blocks, stop processing
                        if is_blocked {
                            self.state.write().await.record_blocked();
                            warn!(hook = id.as_str(), "Hook blocked tool call");
                            return Ok(merged);
                        }
                    }
                    Err(e) => {
                        warn!(hook = id.as_str(), error = %e, "Hook execution failed");
                        // Hooks fail open — don't block on errors
                    }
                }
            }
        }

        let elapsed = start.elapsed();
        debug!(?event, elapsed_ms = elapsed.as_millis(), "Event processed");

        Ok(merged)
    }
}
