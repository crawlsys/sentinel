//! Hookdeck webhook decoders.
//!
//! Turns raw JSON webhook payloads into short, human-readable one-line summaries
//! so the Claude Code session sees actionable notifications instead of 400-line
//! JSON dumps.
//!
//! # Design
//!
//! Each source (Linear, GitHub, Vercel, Railway, …) has its own sub-module with
//! per-event-type matchers. The public entry point is [`decode`], which dispatches
//! by `source` and either returns a typed summary or falls back to a generic
//! `[HOOKDECK:<source>] <event_type> on <resource_id>` line. Raw payload is
//! always preserved on the returned [`Decoded`] struct so callers can attach it
//! to channel events alongside the summary.
//!
//! # Wiring
//!
//! The actual channel bridge (in `vulcan-hookdeck`) can call [`decode`] to build
//! the `summary` field of each channel event. This module has no I/O of its own,
//! no hook handler — it's a pure transformation library.

use serde_json::Value;

pub mod github;
pub mod linear;
pub mod railway;
pub mod vercel;

/// The decoded form of a webhook event.
///
/// - `summary`: one-line human readable message for the session to see
/// - `raw`: the original JSON body, preserved so callers can attach it to
///   channel event meta (never drop detail; callers pick visibility)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub summary: String,
    pub raw: Value,
}

impl Decoded {
    fn new(summary: impl Into<String>, raw: &Value) -> Self {
        Self {
            summary: summary.into(),
            raw: raw.clone(),
        }
    }
}

/// Decode a webhook event from `source` into a one-line summary.
///
/// `event_type` is typically sourced from a webhook header (e.g. GitHub's
/// `X-GitHub-Event`) or the body's `action`/`type` fields. It's optional because
/// some sources only carry the event kind inside the body.
///
/// Returns a [`Decoded`] with a best-effort summary. Never drops, never panics,
/// never returns raw 400-line JSON in the summary — if all decoders fail, emits
/// a short `[HOOKDECK:<source>] <event_type> on <resource_id>` fallback.
pub fn decode(source: &str, event_type: Option<&str>, body: &Value) -> Decoded {
    let specific = match source.to_ascii_lowercase().as_str() {
        "linear" => linear::decode(body),
        "github" => github::decode(event_type, body),
        "vercel" => vercel::decode(body),
        "railway" => railway::decode(body),
        _ => None,
    };

    specific.unwrap_or_else(|| fallback(source, event_type, body))
}

/// Generic last-resort summary used when no decoder matches.
///
/// Produces `[HOOKDECK:<source>] <event_type> on <resource_id>` where
/// `<resource_id>` is best-guessed from common payload fields: `id`, `data.id`,
/// `data.identifier`, `event_id`.
pub fn fallback(source: &str, event_type: Option<&str>, body: &Value) -> Decoded {
    let et = event_type
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| body.get("type").and_then(Value::as_str).map(str::to_string))
        .or_else(|| {
            body.get("event")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            body.get("action")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "event".to_string());

    let resource = best_resource_id(body).unwrap_or_else(|| "<unknown>".to_string());

    let summary = format!("[HOOKDECK:{source}] {et} on {resource}");
    Decoded::new(summary, body)
}

/// Best-effort extraction of a resource identifier from a webhook body.
fn best_resource_id(body: &Value) -> Option<String> {
    // Order matters: prefer stable human-readable identifiers before opaque IDs.
    for ptr in [
        "/data/identifier",
        "/data/id",
        "/identifier",
        "/id",
        "/event_id",
        "/delivery_id",
    ] {
        if let Some(s) = body.pointer(ptr).and_then(Value::as_str) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Truncate a free-text field (comment body, PR title) to a safe inline length,
/// adding an ellipsis if truncated. Strips newlines so the summary stays a
/// single line.
pub(crate) fn truncate_inline(s: &str, max: usize) -> String {
    let single = s.replace(['\n', '\r'], " ");
    let compressed = compress_whitespace(&single);
    if compressed.chars().count() > max {
        let taken: String = compressed.chars().take(max).collect();
        format!("{taken}…")
    } else {
        compressed
    }
}

fn compress_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fallback_uses_event_type_and_resource() {
        let body = json!({
            "data": { "id": "abc123", "identifier": "COR-42" }
        });
        let decoded = decode("unknown_source", Some("widget.update"), &body);
        assert_eq!(
            decoded.summary,
            "[HOOKDECK:unknown_source] widget.update on COR-42"
        );
    }

    #[test]
    fn fallback_falls_through_event_type_sources() {
        let body = json!({ "type": "FooBar", "data": { "id": "id1" } });
        let decoded = decode("mysource", None, &body);
        assert_eq!(decoded.summary, "[HOOKDECK:mysource] FooBar on id1");
    }

    #[test]
    fn fallback_handles_missing_everything() {
        let body = json!({});
        let decoded = decode("weird", None, &body);
        assert_eq!(decoded.summary, "[HOOKDECK:weird] event on <unknown>");
    }

    #[test]
    fn raw_is_always_preserved() {
        let body = json!({ "foo": "bar" });
        let decoded = decode("unknown", None, &body);
        assert_eq!(decoded.raw, body);
    }

    #[test]
    fn truncate_inline_strips_newlines_and_truncates() {
        let s = "line one\nline two\r\n  line    three";
        assert_eq!(truncate_inline(s, 100), "line one line two line three");
        assert_eq!(truncate_inline(s, 10), "line one l…");
    }

    #[test]
    fn source_dispatch_is_case_insensitive() {
        let body = json!({
            "action": "create",
            "type": "Issue",
            "data": { "identifier": "COR-1", "title": "t" }
        });
        let a = decode("LINEAR", None, &body);
        let b = decode("linear", None, &body);
        assert_eq!(a.summary, b.summary);
        assert!(a.summary.starts_with("[LINEAR]"));
    }
}
