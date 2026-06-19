//! LangGraph checkpoint thread identity helpers.
//!
//! These helpers are storage-agnostic: they do not know about SQLite,
//! Postgres, or `langgraph-core`. They only define Sentinel's durable thread-id
//! namespace so CLI, MCP, proof validation, and graph execution use the same
//! tenant-aware identity contract.

/// Optional env var used by runtime crates to supply hosted tenant scope.
pub const LANGGRAPH_TENANT_ENV: &str = "SENTINEL_LANGGRAPH_TENANT";

/// Validate an operator-supplied tenant namespace.
///
/// Tenant ids deliberately exclude `:` because Sentinel uses `tenant:<id>:` as
/// a structural prefix in LangGraph `thread_id` values.
pub fn validate_tenant_scope(tenant: &str) -> Result<(), String> {
    if tenant.is_empty() {
        return Err(format!("{LANGGRAPH_TENANT_ENV} is set but empty"));
    }
    if tenant.len() > 128 {
        return Err(format!(
            "{LANGGRAPH_TENANT_ENV} exceeds 128 characters: {tenant}"
        ));
    }
    if !tenant
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(format!(
            "{LANGGRAPH_TENANT_ENV} contains invalid characters; use ASCII letters, digits, '-' or '_'"
        ));
    }
    Ok(())
}

/// Prefix a base LangGraph thread id with `tenant:<scope>:` when configured.
pub fn tenant_scoped_thread_id(base: String, tenant: Option<&str>) -> Result<String, String> {
    match tenant {
        Some(tenant) => {
            validate_tenant_scope(tenant)?;
            Ok(format!("tenant:{tenant}:{base}"))
        }
        None => Ok(base),
    }
}

/// Derive the durable thread id for a Sentinel phase workflow.
pub fn phase_thread_id(
    skill: &str,
    session_id: &str,
    tenant: Option<&str>,
) -> Result<String, String> {
    let base = format!("sentinel.phase.{skill}.{session_id}");
    tenant_scoped_thread_id(base, tenant)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_thread_id_is_unscoped_without_tenant() {
        assert_eq!(
            phase_thread_id("linear", "session-123", None).expect("thread id"),
            "sentinel.phase.linear.session-123"
        );
    }

    #[test]
    fn phase_thread_id_is_tenant_scoped_when_configured() {
        assert_eq!(
            phase_thread_id("linear", "session-123", Some("legatus_ai")).expect("thread id"),
            "tenant:legatus_ai:sentinel.phase.linear.session-123"
        );
    }

    #[test]
    fn tenant_scope_rejects_delimiter_injection() {
        let err = phase_thread_id("linear", "session-123", Some("tenant:escape"))
            .expect_err("tenant delimiter injection must fail");
        assert!(err.contains(LANGGRAPH_TENANT_ENV));
        assert!(err.contains("invalid characters"));
    }
}
