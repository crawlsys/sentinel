//! Worker-delegation service.
//!
//! The orchestrator hands a discrete unit of work to a *worker* model and
//! gets a structured result back. Two worker roles are exposed:
//!
//! - **Codex** (`openai/gpt-5.5-pro`) — adversarial / code reasoning. Use for
//!   "poke holes in this", "review this diff", "is this approach sound".
//! - **Kimi** (`moonshotai/kimi-k2.6`) — cheap, large-context scanning. Use for
//!   "scan this blob and answer X", "summarize what's relevant to Y".
//!
//! This is the application-layer use case behind the `delegate_codex` and
//! `delegate_kimi_context_scan` MCP tools. It reuses the standardized
//! [`LlmPort`] (`OpenRouter` via Rig) — the same gateway the judge uses — so
//! there is no second model-calling path to maintain. The worker role maps to
//! an [`LlmModel`] tier; the adapter resolves the pinned `OpenRouter` id.

use anyhow::Result;

use sentinel_domain::ports::{LlmModel, LlmPort, LlmRequest};

/// Which worker model to delegate to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Worker {
    /// Adversarial / code reasoning — `openai/gpt-5.5-pro`.
    Codex,
    /// Cheap large-context scanning — `moonshotai/kimi-k2.6`.
    Kimi,
}

impl Worker {
    /// The `LlmModel` tier this worker maps to. The infrastructure adapter
    /// ([`OpenRouterLlm`](sentinel_infrastructure)) resolves the tier to the
    /// pinned `OpenRouter` model id.
    #[must_use]
    pub const fn model(self) -> LlmModel {
        match self {
            Self::Codex => LlmModel::Codex,
            Self::Kimi => LlmModel::Kimi,
        }
    }

    /// Human-readable label for logs / result metadata.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Kimi => "kimi",
        }
    }
}

/// A unit of work handed to a worker.
#[derive(Debug, Clone)]
pub struct DelegationRequest {
    /// Which worker to use.
    pub worker: Worker,
    /// The task / question for the worker. For Codex this is the thing to
    /// reason about adversarially; for Kimi it's the question to answer
    /// against `context`.
    pub task: String,
    /// Supporting material the worker reads (a diff, a file, a blob of
    /// context). May be empty when the task is self-contained.
    pub context: String,
    /// Response token cap.
    pub max_tokens: u32,
}

/// The worker's structured response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationResult {
    /// Worker label (`"codex"` / `"kimi"`).
    pub worker: String,
    /// The worker's text output.
    pub output: String,
}

/// Default response token cap for delegated work.
pub const DEFAULT_MAX_TOKENS: u32 = 2048;

/// Build the prompt for a [`Worker::Codex`] delegation — adversarial / code
/// framing. Kept as a free function so it's unit-testable without an LLM.
#[must_use]
pub fn build_codex_prompt(task: &str, context: &str) -> String {
    let mut p = String::from(
        "You are a senior engineer doing focused, adversarial review. Reason \
carefully about the TASK below. Be concrete and specific: point to exact \
problems, edge cases, and failure modes; propose the minimal correct fix. \
Do not hedge or pad. If the approach is sound, say so plainly and state why.\n\n\
TASK:\n",
    );
    p.push_str(task);
    if !context.trim().is_empty() {
        p.push_str("\n\nCONTEXT:\n");
        p.push_str(context);
    }
    p
}

/// Build the prompt for a [`Worker::Kimi`] context-scan delegation — answer a
/// question against a (potentially large) context blob, cheaply.
#[must_use]
pub fn build_kimi_scan_prompt(task: &str, context: &str) -> String {
    let mut p = String::from(
        "You are scanning the CONTENT below to answer a specific QUESTION. \
Answer ONLY from the content; if the answer isn't present, say so. Be \
concise — extract the relevant facts, don't summarize everything.\n\n\
QUESTION:\n",
    );
    p.push_str(task);
    p.push_str("\n\nCONTENT:\n");
    p.push_str(context);
    p
}

/// Delegate a unit of work to a worker model. Borrows an [`LlmPort`] so the
/// caller owns the (shared) `OpenRouter` client.
///
/// # Errors
/// Propagates the underlying `LlmPort::complete` error (network / auth /
/// provider failure).
pub async fn delegate(llm: &dyn LlmPort, request: &DelegationRequest) -> Result<DelegationResult> {
    let prompt = match request.worker {
        Worker::Codex => build_codex_prompt(&request.task, &request.context),
        Worker::Kimi => build_kimi_scan_prompt(&request.task, &request.context),
    };
    let output = llm
        .complete(LlmRequest {
            model: request.worker.model(),
            prompt,
            max_tokens: request.max_tokens,
        })
        .await?;
    Ok(DelegationResult {
        worker: request.worker.label().to_string(),
        output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Records the last request and returns a canned response.
    struct SpyLlm {
        last: Mutex<Option<LlmRequest>>,
        reply: String,
    }

    #[async_trait]
    impl LlmPort for SpyLlm {
        async fn complete(&self, request: LlmRequest) -> Result<String> {
            *self.last.lock().unwrap() = Some(request);
            Ok(self.reply.clone())
        }
    }

    #[test]
    fn worker_maps_to_expected_tier() {
        assert_eq!(Worker::Codex.model(), LlmModel::Codex);
        assert_eq!(Worker::Kimi.model(), LlmModel::Kimi);
        assert_eq!(Worker::Codex.label(), "codex");
        assert_eq!(Worker::Kimi.label(), "kimi");
    }

    #[test]
    fn codex_prompt_includes_task_and_context_and_adversarial_framing() {
        let p = build_codex_prompt("review this fn", "fn foo() {}");
        assert!(p.contains("adversarial"));
        assert!(p.contains("review this fn"));
        assert!(p.contains("CONTEXT:"));
        assert!(p.contains("fn foo() {}"));
    }

    #[test]
    fn codex_prompt_omits_empty_context_block() {
        let p = build_codex_prompt("is this sound?", "   ");
        assert!(p.contains("is this sound?"));
        assert!(!p.contains("CONTEXT:"));
    }

    #[test]
    fn kimi_prompt_frames_question_against_content() {
        let p = build_kimi_scan_prompt("where is the auth check?", "big blob");
        assert!(p.contains("QUESTION:"));
        assert!(p.contains("where is the auth check?"));
        assert!(p.contains("CONTENT:"));
        assert!(p.contains("big blob"));
    }

    #[tokio::test]
    async fn delegate_codex_routes_to_codex_tier_and_returns_output() {
        let llm = SpyLlm {
            last: Mutex::new(None),
            reply: "looks fine".to_string(),
        };
        let req = DelegationRequest {
            worker: Worker::Codex,
            task: "check X".to_string(),
            context: "ctx".to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
        };
        let res = delegate(&llm, &req).await.unwrap();
        assert_eq!(res.worker, "codex");
        assert_eq!(res.output, "looks fine");
        let sent = llm.last.lock().unwrap().clone().unwrap();
        assert_eq!(sent.model, LlmModel::Codex);
        assert_eq!(sent.max_tokens, DEFAULT_MAX_TOKENS);
        assert!(sent.prompt.contains("check X"));
    }

    #[tokio::test]
    async fn delegate_kimi_routes_to_kimi_tier() {
        let llm = SpyLlm {
            last: Mutex::new(None),
            reply: "found it at line 12".to_string(),
        };
        let req = DelegationRequest {
            worker: Worker::Kimi,
            task: "find the lock".to_string(),
            context: "lots of code".to_string(),
            max_tokens: 512,
        };
        let res = delegate(&llm, &req).await.unwrap();
        assert_eq!(res.worker, "kimi");
        assert_eq!(res.output, "found it at line 12");
        let sent = llm.last.lock().unwrap().clone().unwrap();
        assert_eq!(sent.model, LlmModel::Kimi);
        assert!(sent.prompt.contains("find the lock"));
    }
}
