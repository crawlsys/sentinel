//! OpenTelemetry tracing service — port for emitting spans.
//!
//! The use-case-layer trait that hooks call to record spans around
//! step execution. Infrastructure plugs in the actual exporter (the
//! follow-up brings `opentelemetry-otlp` wiring); this layer stays
//! exporter-agnostic.
//!
//! # Why a port?
//!
//! Same reason `JudgeService` is a port: hooks shouldn't care whether
//! tracing is on, off, batched, sampled, or routed to OTLP/Jaeger/Tempo.
//! They call `start_span` and pretend something's listening — `NoOpTracer`
//! makes that true even when it isn't.
//!
//! # Span lifecycle
//!
//! 1. **`start_span`** at PreToolUse — returns a [`SpanHandle`] that the
//!    caller threads through the step. Sets the active span so child
//!    spans correlate.
//! 2. **`record_event`** any time during the span — bag of structured
//!    attributes (e.g. `verdict.sufficient = true`, `evidence.token_count = 42`).
//! 3. **`end_span`** at PostToolUse — closes the span with optional
//!    status. Idempotent.
//!
//! # Correlation with proof chain
//!
//! [`SpanHandle::trace_context`] returns the W3C [`TraceContext`] for
//! the active span. The hook stamps that context onto the StepProof it
//! emits — making the proof chain queryable by trace_id, and giving
//! Tempo / Honeycomb dashboards a back-link via `step_id` / `phase_id`
//! attributes.

use sentinel_domain::tracing::TraceContext;

/// Span kind — same enum OTEL defines, lifted into the domain so the
/// trait doesn't pull in an opentelemetry crate dep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    /// Server / inbound span — sentinel handling a hook event.
    Server,
    /// Client / outbound span — sentinel calling out (judge, MCP).
    Client,
    /// Internal / step execution span. The default for step-level work.
    Internal,
    /// Producer span — emitting an event (channel push, webhook fire).
    Producer,
    /// Consumer span — receiving an event.
    Consumer,
}

/// Span status — terminal state the caller reports at `end_span`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanStatus {
    /// Default — neither explicit success nor failure.
    Unset,
    /// Step / call completed as intended.
    Ok,
    /// Step / call failed; the string is a free-form description.
    Error(String),
}

/// Attribute value — what `record_event` and `set_attribute` accept.
/// String / int / float / bool covers ~all OTEL attribute use cases;
/// nested structures get flattened via dotted keys at the call site.
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl From<String> for AttributeValue {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&str> for AttributeValue {
    fn from(s: &str) -> Self {
        Self::String(s.to_string())
    }
}

impl From<i64> for AttributeValue {
    fn from(i: i64) -> Self {
        Self::Int(i)
    }
}

impl From<u64> for AttributeValue {
    fn from(u: u64) -> Self {
        // Lossy on values > i64::MAX; OTEL has the same limitation
        // (signed 64-bit attribute integers).
        #[allow(clippy::cast_possible_wrap)]
        Self::Int(u as i64)
    }
}

impl From<f64> for AttributeValue {
    fn from(f: f64) -> Self {
        Self::Float(f)
    }
}

impl From<bool> for AttributeValue {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

/// Handle for an active span. Threading this from `start_span` to
/// `end_span` ensures the exporter sees both halves of the lifecycle.
///
/// `Clone` because in the hook architecture the same span is referenced
/// by both the dispatcher and the per-hook closures — cloning is
/// cheap (the trace context is small) and avoids `Arc<Mutex>`.
#[derive(Debug, Clone)]
pub struct SpanHandle {
    /// Stable identifier for this span within the process. The exporter
    /// uses this to correlate the start / record / end events; it's
    /// distinct from the W3C span_id (which is part of `trace_context`)
    /// because exporters may choose their own internal IDs for
    /// efficiency.
    pub local_id: u64,

    /// W3C trace context for this span. The `span_id` field is the
    /// W3C identifier downstream callers use to nest their spans.
    pub trace_context: TraceContext,

    /// Span kind. Recorded once at start, immutable thereafter.
    pub kind: SpanKind,

    /// Span name — usually `"step.{skill}.{phase}.{step_id}"` for
    /// step-level spans. Free-form; exporters use it as the dashboard
    /// row label.
    pub name: String,
}

impl SpanHandle {
    /// Build a fresh `SpanHandle` for in-process span construction.
    /// The trace context's `span_id` becomes the W3C identifier child
    /// callers will reference.
    #[must_use]
    pub fn new(
        local_id: u64,
        trace_context: TraceContext,
        kind: SpanKind,
        name: impl Into<String>,
    ) -> Self {
        Self {
            local_id,
            trace_context,
            kind,
            name: name.into(),
        }
    }
}

/// Trait the hook layer calls. Infrastructure provides the real impl
/// (OTLP / stdout / file exporter); tests use [`NoOpTracer`] or a
/// `RecordingTracer` for assertions.
///
/// **Async or sync?** Sync. OTEL exporters typically buffer to a
/// background queue, so `start_span` / `end_span` are non-blocking
/// even on the real exporter. Forcing async would require every hook
/// to be `async`, which they aren't all. The exporter does its IO
/// off-thread.
pub trait TracingPort: Send + Sync {
    /// Open a new span. The returned handle MUST be passed to
    /// `end_span` eventually — implementations may leak resources if
    /// spans are dropped without ending. (`NoOpTracer` is safe by
    /// definition; real exporters log a warning on drop.)
    ///
    /// `parent` is the inbound trace context (e.g. parsed from a
    /// `traceparent` header). `None` makes this a root span — the
    /// implementation generates fresh trace + span IDs.
    fn start_span(&self, name: &str, kind: SpanKind, parent: Option<&TraceContext>) -> SpanHandle;

    /// Set a single key-value attribute on the active span.
    /// No-op if `span` was already ended (idempotent behavior).
    fn set_attribute(&self, span: &SpanHandle, key: &str, value: AttributeValue);

    /// Record a structured event within the span. Events are points
    /// in time during the span (`evidence.gathered`, `judge.dispatched`).
    /// `attributes` is an optional bag of key-values for the event.
    fn record_event(&self, span: &SpanHandle, name: &str, attributes: &[(String, AttributeValue)]);

    /// Close the span with the given status. Implementations should
    /// be idempotent — calling `end_span` twice on the same handle
    /// must not panic and should not double-emit.
    fn end_span(&self, span: SpanHandle, status: SpanStatus);
}

/// No-op tracer — the default when OTEL isn't configured. Every method
/// is a fast return path; `start_span` synthesizes a deterministic
/// handle so callers downstream don't need to None-check.
pub struct NoOpTracer;

impl TracingPort for NoOpTracer {
    fn start_span(&self, name: &str, kind: SpanKind, parent: Option<&TraceContext>) -> SpanHandle {
        // Synthesize a context so callers get something to thread —
        // even with tracing off, the StepProof can still record a
        // (fake-but-stable) trace_context for shape consistency.
        // Real implementations generate cryptographically random IDs;
        // the no-op uses a deterministic value so tests can assert.
        let trace_context = parent.cloned().unwrap_or_else(|| {
            TraceContext::new_root("00000000000000000000000000000001", "0000000000000001")
        });
        SpanHandle::new(0, trace_context, kind, name)
    }

    fn set_attribute(&self, _span: &SpanHandle, _key: &str, _value: AttributeValue) {}

    fn record_event(
        &self,
        _span: &SpanHandle,
        _name: &str,
        _attributes: &[(String, AttributeValue)],
    ) {
    }

    fn end_span(&self, _span: SpanHandle, _status: SpanStatus) {}
}

/// Recording tracer for tests — captures every call so assertions
/// can pin behavior without running a real exporter. Thread-safe
/// because hooks can fire concurrently.
///
/// Gated on `cfg(test)` so production builds don't ship the recorder.
/// Add a `test-support` Cargo feature later if integration tests in
/// other crates need to share the recorder.
#[cfg(test)]
pub mod recording {
    use super::*;
    use std::sync::Mutex;

    /// One captured operation, with enough context for tests to assert.
    #[derive(Debug, Clone, PartialEq)]
    pub enum RecordedOp {
        Started {
            local_id: u64,
            name: String,
            kind: SpanKind,
            parent_span_id: Option<String>,
        },
        Attribute {
            local_id: u64,
            key: String,
            value: AttributeValue,
        },
        Event {
            local_id: u64,
            name: String,
            attributes: Vec<(String, AttributeValue)>,
        },
        Ended {
            local_id: u64,
            status: SpanStatus,
        },
    }

    /// Records every TracingPort call. Tests inspect `ops()` after
    /// the SUT runs to assert what spans / events / attributes fired.
    pub struct RecordingTracer {
        next_id: Mutex<u64>,
        ops: Mutex<Vec<RecordedOp>>,
    }

    impl RecordingTracer {
        #[must_use]
        pub fn new() -> Self {
            Self {
                next_id: Mutex::new(1),
                ops: Mutex::new(Vec::new()),
            }
        }

        /// Snapshot of recorded operations. Each call returns a
        /// fresh clone so the test holds no lock.
        pub fn ops(&self) -> Vec<RecordedOp> {
            self.ops.lock().expect("ops lock poisoned").clone()
        }
    }

    impl Default for RecordingTracer {
        fn default() -> Self {
            Self::new()
        }
    }

    impl TracingPort for RecordingTracer {
        fn start_span(
            &self,
            name: &str,
            kind: SpanKind,
            parent: Option<&TraceContext>,
        ) -> SpanHandle {
            let local_id = {
                let mut next = self.next_id.lock().expect("next_id lock poisoned");
                let id = *next;
                *next += 1;
                id
            };
            // Synthesize a context that links to the parent if one was
            // supplied — real exporters do the same, just with random
            // IDs. Deterministic IDs in the recorder make assertions
            // straightforward.
            let trace_context = parent.cloned().unwrap_or_else(|| {
                TraceContext::new_root(format!("{local_id:032x}"), format!("{local_id:016x}"))
            });
            self.ops
                .lock()
                .expect("ops lock poisoned")
                .push(RecordedOp::Started {
                    local_id,
                    name: name.to_string(),
                    kind,
                    parent_span_id: parent.map(|c| c.span_id.clone()),
                });
            SpanHandle::new(local_id, trace_context, kind, name)
        }

        fn set_attribute(&self, span: &SpanHandle, key: &str, value: AttributeValue) {
            self.ops
                .lock()
                .expect("ops lock poisoned")
                .push(RecordedOp::Attribute {
                    local_id: span.local_id,
                    key: key.to_string(),
                    value,
                });
        }

        fn record_event(
            &self,
            span: &SpanHandle,
            name: &str,
            attributes: &[(String, AttributeValue)],
        ) {
            self.ops
                .lock()
                .expect("ops lock poisoned")
                .push(RecordedOp::Event {
                    local_id: span.local_id,
                    name: name.to_string(),
                    attributes: attributes.to_vec(),
                });
        }

        fn end_span(&self, span: SpanHandle, status: SpanStatus) {
            self.ops
                .lock()
                .expect("ops lock poisoned")
                .push(RecordedOp::Ended {
                    local_id: span.local_id,
                    status,
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_ctx() -> TraceContext {
        TraceContext::new_root("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331")
    }

    // ── NoOpTracer ────────────────────────────────────────────────────

    #[test]
    fn no_op_start_span_synthesizes_root_when_no_parent() {
        let t = NoOpTracer;
        let h = t.start_span("step.linear.claim.1", SpanKind::Internal, None);
        // Some context is always returned — callers shouldn't have to
        // None-check; that's the whole point of the no-op tracer.
        assert!(!h.trace_context.trace_id.is_empty());
        assert!(!h.trace_context.span_id.is_empty());
        assert_eq!(h.name, "step.linear.claim.1");
        assert_eq!(h.kind, SpanKind::Internal);
    }

    #[test]
    fn no_op_start_span_inherits_parent_context() {
        // Inbound traceparent → outbound StepProof must carry the same
        // trace_id so dashboards correlate. NoOp upholds the contract
        // by cloning the parent ctx.
        let t = NoOpTracer;
        let parent = valid_ctx();
        let h = t.start_span("step.x", SpanKind::Server, Some(&parent));
        assert_eq!(h.trace_context.trace_id, parent.trace_id);
    }

    #[test]
    fn no_op_methods_dont_panic_after_end() {
        // Idempotency contract: ending then setting attributes / events
        // is allowed. NoOp meets it trivially; real impls should too.
        let t = NoOpTracer;
        let h = t.start_span("x", SpanKind::Internal, None);
        let h2 = h.clone();
        t.end_span(h, SpanStatus::Ok);
        // Attributes after end — must not panic.
        t.set_attribute(&h2, "key", AttributeValue::Bool(true));
        t.record_event(&h2, "after_end", &[]);
    }

    // ── RecordingTracer ──────────────────────────────────────────────

    #[test]
    fn recording_tracer_captures_full_lifecycle() {
        use recording::{RecordedOp, RecordingTracer};

        let t = RecordingTracer::new();
        let h = t.start_span("step.linear.claim.1", SpanKind::Internal, None);
        t.set_attribute(&h, "skill", "linear".into());
        t.set_attribute(&h, "step_id", "1".into());
        t.record_event(
            &h,
            "evidence.gathered",
            &[("token_count".into(), 42_i64.into())],
        );
        t.end_span(h, SpanStatus::Ok);

        // 1 start + 2 attributes + 1 event + 1 end = 5 ops captured.
        let ops = t.ops();
        assert_eq!(ops.len(), 5);
        match &ops[0] {
            RecordedOp::Started { name, kind, .. } => {
                assert_eq!(name, "step.linear.claim.1");
                assert_eq!(*kind, SpanKind::Internal);
            }
            other => panic!("expected Started, got {other:?}"),
        }
        match &ops[1] {
            RecordedOp::Attribute { key, value, .. } => {
                assert_eq!(key, "skill");
                assert_eq!(*value, AttributeValue::String("linear".into()));
            }
            other => panic!("expected Attribute, got {other:?}"),
        }
        match &ops[2] {
            RecordedOp::Attribute { key, value, .. } => {
                assert_eq!(key, "step_id");
                assert_eq!(*value, AttributeValue::String("1".into()));
            }
            other => panic!("expected Attribute, got {other:?}"),
        }
        match &ops[3] {
            RecordedOp::Event {
                name, attributes, ..
            } => {
                assert_eq!(name, "evidence.gathered");
                assert_eq!(attributes.len(), 1);
                assert_eq!(attributes[0].0, "token_count");
                assert_eq!(attributes[0].1, AttributeValue::Int(42));
            }
            other => panic!("expected Event, got {other:?}"),
        }
        match &ops[4] {
            RecordedOp::Ended { status, .. } => assert_eq!(*status, SpanStatus::Ok),
            other => panic!("expected Ended, got {other:?}"),
        }
    }

    #[test]
    fn recording_tracer_propagates_parent_span_id() {
        // The Started op must include the parent's span_id when one
        // was supplied — that's the link tests assert to verify hook
        // code is propagating context, not dropping it.
        use recording::{RecordedOp, RecordingTracer};
        let t = RecordingTracer::new();
        let parent = valid_ctx();
        let _h = t.start_span("child", SpanKind::Internal, Some(&parent));
        let ops = t.ops();
        match &ops[0] {
            RecordedOp::Started { parent_span_id, .. } => {
                assert_eq!(parent_span_id.as_deref(), Some(parent.span_id.as_str()));
            }
            other => panic!("expected Started, got {other:?}"),
        }
    }

    #[test]
    fn recording_tracer_assigns_unique_local_ids() {
        // Two spans in flight at once must get distinct local_ids so
        // attribute / event ops route to the right span.
        use recording::RecordingTracer;
        let t = RecordingTracer::new();
        let a = t.start_span("a", SpanKind::Internal, None);
        let b = t.start_span("b", SpanKind::Internal, None);
        assert_ne!(a.local_id, b.local_id);
    }

    // ── AttributeValue conversions ───────────────────────────────────

    #[test]
    fn attribute_value_from_str_makes_string() {
        let v: AttributeValue = "hi".into();
        assert_eq!(v, AttributeValue::String("hi".into()));
    }

    #[test]
    fn attribute_value_from_i64_makes_int() {
        let v: AttributeValue = 42_i64.into();
        assert_eq!(v, AttributeValue::Int(42));
    }

    #[test]
    fn attribute_value_from_bool_makes_bool() {
        let v: AttributeValue = true.into();
        assert_eq!(v, AttributeValue::Bool(true));
    }

    #[test]
    fn attribute_value_from_f64_makes_float() {
        let v: AttributeValue = 0.5_f64.into();
        assert_eq!(v, AttributeValue::Float(0.5));
    }

    #[test]
    fn span_handle_clone_preserves_local_id() {
        // Hooks fan a span out across closures — Clone must preserve
        // the local_id so all clones address the same logical span.
        let h = SpanHandle::new(7, valid_ctx(), SpanKind::Internal, "x");
        let h2 = h.clone();
        assert_eq!(h.local_id, h2.local_id);
        assert_eq!(h.trace_context, h2.trace_context);
    }
}
