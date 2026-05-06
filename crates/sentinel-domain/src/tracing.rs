//! OpenTelemetry trace context — pure domain types.
//!
//! Sits *alongside* the proof chain, not in place of it. The proof chain is
//! the audit substrate — what happened, signed, immutable, queryable as
//! evidence. OTEL is the operational substrate — how long, what called what,
//! where the latency lives. They link via the [`TraceContext`] field on each
//! [`StepProof`](crate::step_proof::StepProof): a corpus query for "show me
//! every step that took >5s" can pivot directly to the OTEL trace in
//! Grafana / Tempo / Honeycomb without re-deriving timing from proof
//! timestamps.
//!
//! # W3C Trace Context (the wire format)
//!
//! Two HTTP headers carry trace state across process boundaries:
//!
//! - `traceparent: 00-{trace_id}-{span_id}-{flags}` — required.
//!   `trace_id` is 32 lowercase hex chars (16 bytes), `span_id` is 16
//!   lowercase hex chars (8 bytes), `flags` is 2 hex chars (the only bit
//!   anyone uses is `01` = sampled).
//! - `tracestate: vendor1=value1,vendor2=value2` — optional vendor-specific
//!   key-value list, max 32 entries per spec.
//!
//! When future MCP transports (M2.12 streamable HTTP) carry these headers,
//! sentinel parses them on PreToolUse and propagates them into the StepProof
//! that gets emitted on PostToolUse. Outbound calls (when sentinel is the
//! caller, not the callee) format the context back into headers via
//! [`TraceContext::to_traceparent`] and [`TraceContext::to_tracestate`].
//!
//! # Why this lives in sentinel-domain
//!
//! Pure data + parsing — no I/O, no async runtime, no exporter dependency.
//! The [`TracingPort`](crate::ports) trait belongs in `sentinel-application`
//! (it's a use-case concern), and the OTLP exporter belongs in
//! `sentinel-infrastructure` (heavy `opentelemetry-otlp` dep). This module
//! is the lingua franca that lets all three layers agree on what a trace
//! span looks like without any of them depending on each other.

use serde::{Deserialize, Serialize};

/// Length of a W3C trace ID in hex characters (16 bytes × 2).
pub const TRACE_ID_HEX_LEN: usize = 32;

/// Length of a W3C span ID in hex characters (8 bytes × 2).
pub const SPAN_ID_HEX_LEN: usize = 16;

/// W3C `traceparent` version — only `00` is defined as of the 2020 W3C
/// recommendation. Newer versions are forward-compatible (parsers ignore
/// unknown trailing fields), but we emit `00` for any context we author.
pub const TRACEPARENT_VERSION: &str = "00";

/// Sampled flag bit in the `traceparent` flags byte. Set when the trace
/// is being recorded by some collector.
pub const FLAG_SAMPLED: u8 = 0x01;

/// W3C-compliant trace context.
///
/// Carries enough state to (a) emit OTEL spans from the current process,
/// (b) propagate to downstream MCP calls via traceparent / tracestate
/// headers, and (c) link a [`StepProof`](crate::step_proof::StepProof) back
/// to the trace it was emitted under so corpus queries can pivot to OTEL
/// dashboards.
///
/// Stored in StepProof as `Option<TraceContext>` — `None` for proofs that
/// ran without OTEL configured (the common case until the OTLP exporter
/// lands), `Some(ctx)` once tracing is wired up. Never affects the
/// combined hash: trace context is operational metadata, not part of the
/// audit contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TraceContext {
    /// 32 hex chars (16 bytes). All-zero is invalid per W3C spec.
    pub trace_id: String,

    /// 16 hex chars (8 bytes). All-zero is invalid per W3C spec. This is
    /// the span the proof was emitted *inside* — the parent of any spans
    /// the step itself opened, not the parent of the trace.
    pub span_id: String,

    /// 16 hex chars when set, `None` for the trace's root span. Threading
    /// this through StepProofs means a corpus search can reconstruct the
    /// span tree even if the OTEL collector dropped events for retention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,

    /// W3C flags byte. Only the `0x01` "sampled" bit is widely supported;
    /// other bits are reserved and we preserve them on round-trip.
    #[serde(default)]
    pub flags: u8,

    /// Optional `tracestate` entries (vendor-specific routing data).
    /// Vec rather than Map because the W3C spec gives left-to-right order
    /// semantic meaning ("most recent vendor leftmost").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracestate: Vec<(String, String)>,
}

/// Errors when parsing W3C `traceparent` / `tracestate` headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceParseError {
    /// Wrong number of hyphen-delimited fields. W3C v00 = exactly 4.
    WrongFieldCount(usize),
    /// Version field unsupported (we accept `00` and the future
    /// "ignore extra fields" forward-compat rule, but reject things
    /// that are clearly not hex version bytes).
    UnsupportedVersion(String),
    /// `trace_id` is the wrong length or all-zero (both invalid).
    InvalidTraceId,
    /// `span_id` is the wrong length or all-zero (both invalid).
    InvalidSpanId,
    /// `flags` not 2 hex chars.
    InvalidFlags,
    /// A character in trace_id / span_id / flags wasn't lowercase hex.
    NonHexCharacter,
    /// Tracestate entry malformed (not `key=value`).
    MalformedTracestateEntry(String),
    /// Tracestate has more than 32 entries (W3C spec cap).
    TracestateTooManyEntries(usize),
}

impl std::fmt::Display for TraceParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongFieldCount(n) => write!(
                f,
                "traceparent must have 4 hyphen-delimited fields, got {n}",
            ),
            Self::UnsupportedVersion(v) => {
                write!(f, "unsupported traceparent version '{v}' (expected '00')")
            }
            Self::InvalidTraceId => write!(
                f,
                "trace_id must be 32 lowercase hex chars and not all-zero",
            ),
            Self::InvalidSpanId => write!(
                f,
                "span_id must be 16 lowercase hex chars and not all-zero",
            ),
            Self::InvalidFlags => write!(f, "flags must be exactly 2 hex chars"),
            Self::NonHexCharacter => {
                write!(f, "trace_id / span_id / flags contained a non-hex character")
            }
            Self::MalformedTracestateEntry(s) => {
                write!(f, "malformed tracestate entry '{s}' (expected key=value)")
            }
            Self::TracestateTooManyEntries(n) => {
                write!(f, "tracestate has {n} entries, W3C spec caps at 32")
            }
        }
    }
}

impl std::error::Error for TraceParseError {}

impl TraceContext {
    /// True when the `sampled` bit is set in `flags`.
    #[must_use]
    pub fn is_sampled(&self) -> bool {
        self.flags & FLAG_SAMPLED != 0
    }

    /// Set or clear the `sampled` bit.
    pub fn set_sampled(&mut self, sampled: bool) {
        if sampled {
            self.flags |= FLAG_SAMPLED;
        } else {
            self.flags &= !FLAG_SAMPLED;
        }
    }

    /// Parse a W3C `traceparent` header value.
    ///
    /// Strict on the v00 grammar:
    /// - Exactly 4 hyphen-delimited fields.
    /// - Field 1 = `00` (we accept newer versions but only by reading the
    ///   first 4 fields and ignoring tail; non-hex versions are rejected).
    /// - Field 2 = trace_id (32 lowercase hex, not all-zero).
    /// - Field 3 = span_id (16 lowercase hex, not all-zero).
    /// - Field 4 = flags (2 hex chars).
    pub fn parse_traceparent(s: &str) -> Result<Self, TraceParseError> {
        let parts: Vec<&str> = s.trim().split('-').collect();
        // Spec is forward-compatible: future versions append fields. We
        // require at least 4 (the v00 minimum) and ignore the rest.
        if parts.len() < 4 {
            return Err(TraceParseError::WrongFieldCount(parts.len()));
        }

        let version = parts[0];
        if version.len() != 2 || !version.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(TraceParseError::UnsupportedVersion(version.into()));
        }
        if version != TRACEPARENT_VERSION && parts.len() == 4 {
            // Per spec: future versions MAY add fields. A 4-field non-v00
            // payload is ambiguous — could be invalid v00 or a stripped
            // newer version. Treat as supported but log via the error
            // type if we ever add diagnostics.
        }

        let trace_id = parts[1];
        if trace_id.len() != TRACE_ID_HEX_LEN
            || trace_id.chars().any(|c| !c.is_ascii_hexdigit() || c.is_ascii_uppercase())
        {
            // Distinguish "wrong length" from "non-hex" for clearer errors.
            if trace_id.chars().any(|c| !c.is_ascii_hexdigit()) {
                return Err(TraceParseError::NonHexCharacter);
            }
            return Err(TraceParseError::InvalidTraceId);
        }
        if trace_id.chars().all(|c| c == '0') {
            return Err(TraceParseError::InvalidTraceId);
        }

        let span_id = parts[2];
        if span_id.len() != SPAN_ID_HEX_LEN
            || span_id.chars().any(|c| !c.is_ascii_hexdigit() || c.is_ascii_uppercase())
        {
            if span_id.chars().any(|c| !c.is_ascii_hexdigit()) {
                return Err(TraceParseError::NonHexCharacter);
            }
            return Err(TraceParseError::InvalidSpanId);
        }
        if span_id.chars().all(|c| c == '0') {
            return Err(TraceParseError::InvalidSpanId);
        }

        let flags_str = parts[3];
        if flags_str.len() != 2 || flags_str.chars().any(|c| !c.is_ascii_hexdigit()) {
            return Err(TraceParseError::InvalidFlags);
        }
        let flags = u8::from_str_radix(flags_str, 16)
            .map_err(|_| TraceParseError::InvalidFlags)?;

        Ok(Self {
            trace_id: trace_id.to_string(),
            span_id: span_id.to_string(),
            parent_span_id: None,
            flags,
            tracestate: Vec::new(),
        })
    }

    /// Parse a W3C `tracestate` header value into the `(key, value)`
    /// list. Empty input yields an empty list (legal per spec).
    pub fn parse_tracestate(s: &str) -> Result<Vec<(String, String)>, TraceParseError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let entries: Vec<&str> = trimmed.split(',').collect();
        if entries.len() > 32 {
            return Err(TraceParseError::TracestateTooManyEntries(entries.len()));
        }
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let Some((k, v)) = entry.split_once('=') else {
                return Err(TraceParseError::MalformedTracestateEntry(entry.into()));
            };
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
        Ok(out)
    }

    /// Format `self` as a W3C `traceparent` header value.
    ///
    /// Always emits version `00`. Round-trip property: parsing then
    /// formatting yields the same string for any v00-shaped input.
    #[must_use]
    pub fn to_traceparent(&self) -> String {
        format!(
            "{}-{}-{}-{:02x}",
            TRACEPARENT_VERSION, self.trace_id, self.span_id, self.flags,
        )
    }

    /// Format `tracestate` as a W3C header value. Empty list yields an
    /// empty string (the convention is then to omit the header entirely).
    #[must_use]
    pub fn to_tracestate(&self) -> String {
        self.tracestate
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Build a fresh root context with the given trace + span IDs, no
    /// parent, sampled bit set. Convenience for in-process span starts
    /// when no inbound traceparent exists.
    #[must_use]
    pub fn new_root(trace_id: impl Into<String>, span_id: impl Into<String>) -> Self {
        Self {
            trace_id: trace_id.into(),
            span_id: span_id.into(),
            parent_span_id: None,
            flags: FLAG_SAMPLED,
            tracestate: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_trace_id() -> &'static str {
        "0af7651916cd43dd8448eb211c80319c"
    }

    fn valid_span_id() -> &'static str {
        "b7ad6b7169203331"
    }

    // ── parse_traceparent ────────────────────────────────────────────

    #[test]
    fn parses_canonical_v00_traceparent() {
        // The spec example, verbatim. If this breaks the parser is wrong.
        let h = format!("00-{}-{}-01", valid_trace_id(), valid_span_id());
        let ctx = TraceContext::parse_traceparent(&h).unwrap();
        assert_eq!(ctx.trace_id, valid_trace_id());
        assert_eq!(ctx.span_id, valid_span_id());
        assert_eq!(ctx.flags, 0x01);
        assert!(ctx.is_sampled());
        assert!(ctx.parent_span_id.is_none());
        assert!(ctx.tracestate.is_empty());
    }

    #[test]
    fn parses_unsampled_traceparent() {
        let h = format!("00-{}-{}-00", valid_trace_id(), valid_span_id());
        let ctx = TraceContext::parse_traceparent(&h).unwrap();
        assert_eq!(ctx.flags, 0);
        assert!(!ctx.is_sampled());
    }

    #[test]
    fn rejects_traceparent_with_too_few_fields() {
        let h = format!("00-{}-{}", valid_trace_id(), valid_span_id());
        let err = TraceContext::parse_traceparent(&h).unwrap_err();
        assert_eq!(err, TraceParseError::WrongFieldCount(3));
    }

    #[test]
    fn accepts_future_version_with_extra_trailing_fields() {
        // W3C forward-compat: future versions may append fields. Parsers
        // should read the first 4 and ignore the rest. We accept this.
        let h = format!("01-{}-{}-01-extra", valid_trace_id(), valid_span_id());
        let ctx = TraceContext::parse_traceparent(&h).unwrap();
        assert_eq!(ctx.trace_id, valid_trace_id());
    }

    #[test]
    fn rejects_non_hex_version() {
        // Non-hex version is a clear error, not a forward-compat case.
        let h = format!("zz-{}-{}-01", valid_trace_id(), valid_span_id());
        let err = TraceContext::parse_traceparent(&h).unwrap_err();
        assert!(matches!(err, TraceParseError::UnsupportedVersion(_)));
    }

    #[test]
    fn rejects_all_zero_trace_id() {
        // Per W3C: all-zero trace_id is invalid. Nullish IDs don't
        // identify anything and create false-correlation hazards.
        let zero = "0".repeat(32);
        let h = format!("00-{}-{}-01", zero, valid_span_id());
        let err = TraceContext::parse_traceparent(&h).unwrap_err();
        assert_eq!(err, TraceParseError::InvalidTraceId);
    }

    #[test]
    fn rejects_all_zero_span_id() {
        let zero = "0".repeat(16);
        let h = format!("00-{}-{}-01", valid_trace_id(), zero);
        let err = TraceContext::parse_traceparent(&h).unwrap_err();
        assert_eq!(err, TraceParseError::InvalidSpanId);
    }

    #[test]
    fn rejects_short_trace_id() {
        let short = "0af7651916cd43dd8448eb211c80319"; // 31 chars
        let h = format!("00-{}-{}-01", short, valid_span_id());
        let err = TraceContext::parse_traceparent(&h).unwrap_err();
        assert_eq!(err, TraceParseError::InvalidTraceId);
    }

    #[test]
    fn rejects_uppercase_hex_in_trace_id() {
        // W3C requires lowercase. Uppercase is a frequent compatibility
        // bug from naive `format!("{:X}")` callers. Reject loudly.
        let upper = "0AF7651916CD43DD8448EB211C80319C";
        let h = format!("00-{}-{}-01", upper, valid_span_id());
        let err = TraceContext::parse_traceparent(&h).unwrap_err();
        assert_eq!(err, TraceParseError::InvalidTraceId);
    }

    #[test]
    fn rejects_non_hex_character_in_span_id() {
        let bad = "b7ad6b7169zzzzzz"; // non-hex chars
        let h = format!("00-{}-{}-01", valid_trace_id(), bad);
        let err = TraceContext::parse_traceparent(&h).unwrap_err();
        assert_eq!(err, TraceParseError::NonHexCharacter);
    }

    #[test]
    fn rejects_short_flags() {
        let h = format!("00-{}-{}-1", valid_trace_id(), valid_span_id());
        let err = TraceContext::parse_traceparent(&h).unwrap_err();
        assert_eq!(err, TraceParseError::InvalidFlags);
    }

    // ── parse_tracestate ─────────────────────────────────────────────

    #[test]
    fn parses_empty_tracestate() {
        let v = TraceContext::parse_tracestate("").unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn parses_single_tracestate_entry() {
        let v = TraceContext::parse_tracestate("vendor1=abc").unwrap();
        assert_eq!(v, vec![("vendor1".to_string(), "abc".to_string())]);
    }

    #[test]
    fn parses_multiple_tracestate_entries_preserving_order() {
        // Order matters per W3C — most recent vendor leftmost.
        let v = TraceContext::parse_tracestate("a=1,b=2,c=3").unwrap();
        assert_eq!(
            v,
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "2".into()),
                ("c".into(), "3".into()),
            ],
        );
    }

    #[test]
    fn skips_empty_tracestate_entries_from_trailing_commas() {
        // Real-world: some clients emit trailing commas. Don't reject.
        let v = TraceContext::parse_tracestate("a=1,,b=2,").unwrap();
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn rejects_malformed_tracestate_entry() {
        let err = TraceContext::parse_tracestate("noequalshere").unwrap_err();
        assert!(matches!(
            err,
            TraceParseError::MalformedTracestateEntry(_)
        ));
    }

    #[test]
    fn rejects_tracestate_with_too_many_entries() {
        // W3C caps at 32. Anything more is a likely abuse / fuzz vector.
        let many: Vec<String> = (0..40).map(|i| format!("k{i}=v{i}")).collect();
        let s = many.join(",");
        let err = TraceContext::parse_tracestate(&s).unwrap_err();
        assert!(matches!(
            err,
            TraceParseError::TracestateTooManyEntries(40)
        ));
    }

    // ── to_traceparent / to_tracestate round-trip ────────────────────

    #[test]
    fn traceparent_round_trips() {
        // Parse → format → parse should be identity for any v00 input.
        let original = format!("00-{}-{}-01", valid_trace_id(), valid_span_id());
        let parsed = TraceContext::parse_traceparent(&original).unwrap();
        let emitted = parsed.to_traceparent();
        assert_eq!(emitted, original);
        let reparsed = TraceContext::parse_traceparent(&emitted).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn flags_byte_round_trips_with_unknown_bits() {
        // Per spec: bits other than `01` are reserved. Preserve them
        // on round-trip — future bit definitions shouldn't break us.
        let original = format!("00-{}-{}-ff", valid_trace_id(), valid_span_id());
        let parsed = TraceContext::parse_traceparent(&original).unwrap();
        assert_eq!(parsed.flags, 0xff);
        assert_eq!(parsed.to_traceparent(), original);
    }

    #[test]
    fn tracestate_round_trips() {
        let entries = vec![
            ("congo".to_string(), "ucfJifl5GOE".to_string()),
            ("rojo".to_string(), "00f067aa0ba902b7".to_string()),
        ];
        let ctx = TraceContext {
            trace_id: valid_trace_id().into(),
            span_id: valid_span_id().into(),
            parent_span_id: None,
            flags: 1,
            tracestate: entries.clone(),
        };
        let s = ctx.to_tracestate();
        let reparsed = TraceContext::parse_tracestate(&s).unwrap();
        assert_eq!(reparsed, entries);
    }

    // ── helpers + flag manipulation ──────────────────────────────────

    #[test]
    fn new_root_sets_sampled_by_default() {
        let ctx = TraceContext::new_root(valid_trace_id(), valid_span_id());
        assert!(ctx.is_sampled());
        assert!(ctx.parent_span_id.is_none());
        assert!(ctx.tracestate.is_empty());
    }

    #[test]
    fn set_sampled_toggles_only_bit_zero() {
        let mut ctx = TraceContext::new_root(valid_trace_id(), valid_span_id());
        ctx.flags = 0xfe; // sampled cleared, all other bits set
        ctx.set_sampled(true);
        assert_eq!(ctx.flags, 0xff, "set should preserve other bits");
        ctx.set_sampled(false);
        assert_eq!(ctx.flags, 0xfe, "clear should preserve other bits");
    }

    #[test]
    fn serde_round_trip_through_json() {
        // Stored on disk via StepProof; round-trip through JSON must
        // preserve everything including parent_span_id and tracestate.
        let ctx = TraceContext {
            trace_id: valid_trace_id().into(),
            span_id: valid_span_id().into(),
            parent_span_id: Some("a".repeat(16)),
            flags: FLAG_SAMPLED,
            tracestate: vec![("vendor".into(), "value".into())],
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let restored: TraceContext = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, ctx);
    }

    #[test]
    fn serde_skips_empty_tracestate_and_none_parent() {
        // Storage hygiene: empty fields shouldn't bloat the on-disk
        // representation. The skip_serializing_if attributes we set
        // are load-bearing.
        let ctx = TraceContext::new_root(valid_trace_id(), valid_span_id());
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(!json.contains("parent_span_id"));
        assert!(!json.contains("tracestate"));
    }
}
