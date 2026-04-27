//! Railway deployment webhook decoders.
//!
//! Railway's webhook JSON isn't a public schema, but their project-event
//! webhook (configured per-project in the Railway dashboard) carries fields
//! like:
//!
//! ```json
//! { "type": "DEPLOY" | "ALERT",
//!   "status": "BUILDING" | "DEPLOYING" | "SUCCESS" | "FAILED" | "CRASHED" | "REMOVED",
//!   "environment": { "name": "production" | "staging" },
//!   "project": { "name": "..." },
//!   "service": { "name": "..." },
//!   "deployment": { "id": "...", "meta": { "commitSha": "...", "commitMessage": "..." } },
//!   "timestamp": "..." }
//! ```
//!
//! We also accept `eventType`/`event` synonyms since Railway has shipped
//! multiple webhook formats over time.

use serde_json::Value;

use super::{truncate_inline, Decoded};

pub fn decode(body: &Value) -> Option<Decoded> {
    let event_type = body
        .get("type")
        .or_else(|| body.get("eventType"))
        .or_else(|| body.get("event"))
        .and_then(Value::as_str)
        .unwrap_or("event");

    // Only surface deploy/alert events; silently fall through to the fallback
    // for service/project CRUD we don't care about.
    if !matches!(event_type.to_ascii_uppercase().as_str(), "DEPLOY" | "ALERT") {
        return None;
    }

    let status = body
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let env = body
        .pointer("/environment/name")
        .and_then(Value::as_str)
        .unwrap_or("<env>");
    let project = body
        .pointer("/project/name")
        .and_then(Value::as_str)
        .unwrap_or("<project>");
    let service = body.pointer("/service/name").and_then(Value::as_str);
    let commit = body
        .pointer("/deployment/meta/commitSha")
        .and_then(Value::as_str)
        .map(|s| s.chars().take(7).collect::<String>());
    let commit_msg = body
        .pointer("/deployment/meta/commitMessage")
        .and_then(Value::as_str);

    let phase = match status.to_ascii_uppercase().as_str() {
        "BUILDING" | "INITIALIZING" => "started",
        "DEPLOYING" => "deploying",
        "SUCCESS" | "DEPLOYED" => "succeeded",
        "FAILED" | "CRASHED" => "failed",
        "REMOVED" => "removed",
        "SKIPPED" => "skipped",
        _ => status,
    };

    let svc_suffix = service.map(|s| format!(" ({s})")).unwrap_or_default();
    let commit_suffix = match (commit, commit_msg) {
        (Some(sha), Some(msg)) => format!(" — {sha} {}", truncate_inline(msg, 60)),
        (Some(sha), None) => format!(" — commit {sha}"),
        _ => String::new(),
    };

    let summary = format!("Deploy of {project}{svc_suffix} to {env} {phase}{commit_suffix}");
    Some(Decoded::new(format!("[RAILWAY] {summary}"), body))
}

#[cfg(test)]
mod tests {
    use super::super::decode;
    use serde_json::json;

    #[test]
    fn deploy_started_on_staging() {
        let body = json!({
            "type": "DEPLOY",
            "status": "BUILDING",
            "environment": { "name": "staging" },
            "project": { "name": "firefly-pro-crm" },
            "service": { "name": "web" },
            "deployment": { "meta": { "commitSha": "5a0201f0abcdef" } }
        });
        let d = decode("railway", None, &body);
        assert_eq!(
            d.summary,
            "[RAILWAY] Deploy of firefly-pro-crm (web) to staging started — commit 5a0201f"
        );
    }

    #[test]
    fn deploy_succeeded_with_commit_message() {
        let body = json!({
            "type": "DEPLOY",
            "status": "SUCCESS",
            "environment": { "name": "production" },
            "project": { "name": "svc" },
            "deployment": {
                "meta": {
                    "commitSha": "abcdef1234567",
                    "commitMessage": "fix: bug"
                }
            }
        });
        let d = decode("railway", None, &body);
        assert!(d.summary.contains("succeeded"));
        assert!(d.summary.contains("abcdef1 fix: bug"));
    }

    #[test]
    fn deploy_failed() {
        let body = json!({
            "type": "DEPLOY",
            "status": "FAILED",
            "environment": { "name": "staging" },
            "project": { "name": "p" }
        });
        let d = decode("railway", None, &body);
        assert!(d.summary.contains("failed"));
    }

    #[test]
    fn non_deploy_event_falls_back() {
        let body = json!({
            "type": "SERVICE_CREATED",
            "id": "svc_1"
        });
        let d = decode("railway", None, &body);
        assert_eq!(d.summary, "[HOOKDECK:railway] SERVICE_CREATED on svc_1");
    }
}
