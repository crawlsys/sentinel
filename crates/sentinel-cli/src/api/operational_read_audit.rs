use std::io::Write as _;

use sentinel_infrastructure::operational_api_read_graph::{
    OperationalApiReadGraph, OperationalApiReadSurface,
};

pub(super) async fn attach_operational_api_read_graph_audit(
    surface: OperationalApiReadSurface,
    response: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let graph = build_audit_graph()
        .await
        .map_err(|e| format!("build operational API read graph: {e}"))?;
    attach_operational_api_read_graph_audit_with_graph(&graph, surface, response).await
}

/// The durable audit-trail sink for API read decisions.
///
/// Test builds write to a per-process temp file instead: `sentinel_root()`
/// follows `SENTINEL_HOME`, and other tests in this binary point that at
/// short-lived tempdirs — racing their `Drop` turns this append into a
/// spurious audit failure (surfacing as a 500 from an otherwise-correct
/// handler). It also keeps unit tests from appending rows to the operator's
/// live metrics JSONL.
#[cfg(not(test))]
fn graph_runs_path() -> std::path::PathBuf {
    sentinel_infrastructure::paths::sentinel_root()
        .join("metrics")
        .join("operational-api-read.graph-runs.jsonl")
}

#[cfg(test)]
fn graph_runs_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "sentinel-test-{}-operational-api-read.graph-runs.jsonl",
        std::process::id()
    ))
}

/// Test seam: unit tests audit through an ephemeral in-memory checkpointer so
/// they never contend on the operator's live decision-graph sqlite (a running
/// daemon/session shares it; parallel openers flake with lock errors, turning
/// e.g. a 503-mapping assertion into a spurious 500).
#[cfg(not(test))]
async fn build_audit_graph() -> std::result::Result<OperationalApiReadGraph, String> {
    sentinel_infrastructure::operational_api_read_graph::build_operational_api_read_graph().await
}

#[cfg(test)]
async fn build_audit_graph() -> std::result::Result<OperationalApiReadGraph, String> {
    sentinel_infrastructure::operational_api_read_graph::build_operational_api_read_graph_ephemeral(
    )
    .await
}

async fn attach_operational_api_read_graph_audit_with_graph(
    graph: &OperationalApiReadGraph,
    surface: OperationalApiReadSurface,
    mut response: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let graph_audit = run_operational_api_read_graph_audit(graph, surface, &response).await?;
    let obj = response.as_object_mut().ok_or_else(|| {
        "operational API read graph audit can only attach to object responses".to_string()
    })?;
    obj.insert(
        "graph_authority".to_string(),
        serde_json::json!("langgraph"),
    );
    obj.insert("graph_audit".to_string(), graph_audit);
    Ok(response)
}

async fn run_operational_api_read_graph_audit(
    graph: &OperationalApiReadGraph,
    surface: OperationalApiReadSurface,
    response: &serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let response_hash = sentinel_infrastructure::operational_api_read_graph::sha256_json(response);
    let surface_label =
        sentinel_infrastructure::operational_api_read_graph::operational_api_read_surface_label(
            surface,
        );
    let identifier = operational_api_read_identifier(surface_label, response, &response_hash);
    let state =
        sentinel_infrastructure::operational_api_read_graph::OperationalApiReadState::from_response(
            surface, identifier, response,
        );
    let run =
        sentinel_infrastructure::operational_api_read_graph::run_operational_api_read_decision_report(
            graph, state,
        )
        .await
        .map_err(|e| format!("run operational API read graph: {e}"))?;
    let authorization = run
        .operational_api_read_authorization()
        .map_err(|e| format!("operational API read graph authorization failed: {e}"))?
        .ok_or_else(|| "operational API read graph produced no terminal checkpoint".to_string())?;
    let graph_runs = graph_runs_path();
    if let Some(parent) = graph_runs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "create operational API read graph audit dir {}: {e}",
                parent.display()
            )
        })?;
    }
    let row = serde_json::json!({
        "graph_authority": "langgraph",
        "graph": "operational_api_read",
        "surface": surface_label,
        "response_sha256": response_hash,
        "decision": sentinel_infrastructure::operational_api_read_graph::
            operational_api_read_decision_label(authorization.decision()),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": run.thread_id.clone(),
        "run": run,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&graph_runs)
        .map_err(|e| {
            format!(
                "open operational API read graph audit {}: {e}",
                graph_runs.display()
            )
        })?;
    serde_json::to_writer(&mut file, &row).map_err(|e| {
        format!(
            "write operational API read graph audit {}: {e}",
            graph_runs.display()
        )
    })?;
    file.write_all(b"\n").map_err(|e| {
        format!(
            "terminate operational API read graph audit {}: {e}",
            graph_runs.display()
        )
    })?;
    Ok(serde_json::json!({
        "graph_authority": "langgraph",
        "graph": "operational_api_read",
        "surface": surface_label,
        "graph_runs_path": graph_runs,
        "response_sha256": row["response_sha256"].clone(),
        "decision": row["decision"].clone(),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": row["thread_id"].clone(),
        "run": row["run"].clone(),
    }))
}

fn operational_api_read_identifier(
    surface_label: &str,
    response: &serde_json::Value,
    response_hash: &str,
) -> String {
    let id = if let Some(session_id) = response
        .get("session_id")
        .and_then(serde_json::Value::as_str)
    {
        format!("session-id-present-true:{session_id}")
    } else if let Some(version) = response.get("version").and_then(serde_json::Value::as_str) {
        format!("session-id-present-false:version-present-true:{version}")
    } else if let Some(state_file) = response
        .get("state_file")
        .and_then(serde_json::Value::as_str)
    {
        format!("session-id-present-false:state-file-present-true:{state_file}")
    } else if let Some(daemon_url) = response
        .get("daemon_url")
        .and_then(serde_json::Value::as_str)
    {
        format!("session-id-present-false:daemon-url-present-true:{daemon_url}")
    } else if let (Some(owner), Some(repo)) = (
        response.get("owner").and_then(serde_json::Value::as_str),
        response.get("repo").and_then(serde_json::Value::as_str),
    ) {
        format!("session-id-present-false:repo-present-true:{owner}-{repo}")
    } else {
        "session-id-present-false:operational-key-present-false".to_string()
    };
    format!("{surface_label}-{id}:response-{response_hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operational_api_identifier_records_absent_keys_explicitly() {
        let identifier = operational_api_read_identifier(
            "error",
            &serde_json::json!({"error": "operational read failed"}),
            "abc123",
        );

        assert_eq!(
            identifier,
            "error-session-id-present-false:operational-key-present-false:response-abc123"
        );
        assert!(!identifier.contains("operational-api"));
    }
}
