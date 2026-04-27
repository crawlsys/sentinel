//! Vercel deployment webhook decoders.
//!
//! Payload shape (Vercel docs):
//!
//! ```json
//! { "id": "evt_...",
//!   "type": "deployment.created" | "deployment.ready" | "deployment.error" |
//!           "deployment.canceled" | "deployment.succeeded" | ...,
//!   "payload": {
//!     "deployment": { "id": "dpl_...", "name": "...", "url": "...", "inspectorUrl": "..." },
//!     "target": "production" | "preview",
//!     "project": { "id": "...", "name": "..." }
//!   }
//! }
//! ```

use serde_json::Value;

use super::Decoded;

pub fn decode(body: &Value) -> Option<Decoded> {
    let event_type = body.get("type").and_then(Value::as_str)?;
    if !event_type.starts_with("deployment.") {
        return None;
    }

    let payload = body.get("payload").unwrap_or(body);
    let target = payload
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("preview");
    let project_name = payload
        .pointer("/project/name")
        .or_else(|| payload.pointer("/deployment/name"))
        .or_else(|| payload.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("<project>");
    let dep_url = payload
        .pointer("/deployment/url")
        .and_then(Value::as_str)
        .map(|u| {
            if u.starts_with("http") {
                u.to_string()
            } else {
                format!("https://{u}")
            }
        });

    let verb = match event_type {
        "deployment.created" => "started",
        "deployment.ready" | "deployment.succeeded" => "succeeded",
        "deployment.error" | "deployment.failed" => "failed",
        "deployment.canceled" => "canceled",
        "deployment.promoted" => "promoted",
        other => other,
    };

    let summary = match dep_url {
        Some(url) => format!("Deploy of {project_name} to {target} {verb} ({url})"),
        None => format!("Deploy of {project_name} to {target} {verb}"),
    };
    Some(Decoded::new(format!("[VERCEL] {summary}"), body))
}

#[cfg(test)]
mod tests {
    use super::super::decode;
    use serde_json::json;

    #[test]
    fn deployment_ready_matches_spec_format() {
        let body = json!({
            "id": "evt_1",
            "type": "deployment.ready",
            "payload": {
                "deployment": {
                    "id": "dpl_1",
                    "name": "firefly-pro-crm",
                    "url": "firefly-pro-crm-preview-xyz.vercel.app"
                },
                "target": "preview",
                "project": { "name": "firefly-pro-crm" }
            }
        });
        let d = decode("vercel", None, &body);
        assert_eq!(
            d.summary,
            "[VERCEL] Deploy of firefly-pro-crm to preview succeeded (https://firefly-pro-crm-preview-xyz.vercel.app)"
        );
    }

    #[test]
    fn deployment_error() {
        let body = json!({
            "type": "deployment.error",
            "payload": {
                "deployment": { "name": "foo", "url": "foo-abc.vercel.app" },
                "target": "production"
            }
        });
        let d = decode("vercel", None, &body);
        assert!(d.summary.contains("failed"));
        assert!(d.summary.contains("production"));
    }

    #[test]
    fn non_deployment_event_falls_back() {
        let body = json!({
            "type": "project.created",
            "id": "x"
        });
        let d = decode("vercel", None, &body);
        assert_eq!(d.summary, "[HOOKDECK:vercel] project.created on x");
    }
}
