//! Durable storage paths for LangGraph-powered infrastructure decision graphs.

use std::sync::Arc;

use langgraph_core::ports::CheckpointSaver;
use sentinel_domain::langgraph_thread::{
    tenant_scoped_thread_id as tenant_scoped_langgraph_thread_id, validate_tenant_scope,
    LANGGRAPH_TENANT_ENV,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

#[cfg(feature = "postgres")]
use langgraph_core::PostgresCheckpointer;
#[cfg(feature = "redis")]
use langgraph_core::RedisCheckpointer;
#[cfg(feature = "sqlite")]
use langgraph_core::SqliteCheckpointer;

/// Runtime backend selector for durable infrastructure decision-graph checkpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecisionGraphCheckpointerConfig {
    /// Durable local SQLite database path.
    Sqlite { database_path: String },
    /// Durable Postgres database URL plus optional schema.
    Postgres {
        database_url: String,
        schema: Option<String>,
    },
    /// Durable Redis connection URL plus optional checkpoint TTL.
    Redis {
        redis_url: String,
        ttl_seconds: Option<u64>,
    },
}

/// A LangGraph checkpointer plus the backend identity used to build it.
pub(crate) struct DecisionGraphCheckpointer {
    saver: Arc<dyn CheckpointSaver>,
    backend: &'static str,
    scope: String,
}

impl DecisionGraphCheckpointer {
    #[must_use]
    pub(crate) fn backend(&self) -> &'static str {
        self.backend
    }

    #[must_use]
    pub(crate) fn scope(&self) -> &str {
        &self.scope
    }

    #[must_use]
    pub(crate) fn into_saver(self) -> Arc<dyn CheckpointSaver> {
        self.saver
    }
}

impl DecisionGraphCheckpointerConfig {
    /// Env var selecting the checkpoint backend for infrastructure decision graphs.
    pub(crate) const BACKEND_ENV: &'static str = "SENTINEL_DECISION_GRAPH_CHECKPOINTER";
    /// Env var providing the Postgres database URL when backend is `postgres`.
    pub(crate) const POSTGRES_URL_ENV: &'static str = "SENTINEL_DECISION_GRAPH_POSTGRES_URL";
    /// Optional schema for Postgres checkpoints.
    pub(crate) const POSTGRES_SCHEMA_ENV: &'static str = "SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA";
    /// Env var providing the Redis URL when backend is `redis`.
    pub(crate) const REDIS_URL_ENV: &'static str = "SENTINEL_DECISION_GRAPH_REDIS_URL";
    /// Optional Redis checkpoint TTL in seconds.
    pub(crate) const REDIS_TTL_SECS_ENV: &'static str = "SENTINEL_DECISION_GRAPH_REDIS_TTL_SECS";

    /// Build config from process environment.
    ///
    /// No backend variable means the graph-specific SQLite path is the explicit
    /// local default. If `postgres` or `redis` is selected, the backend URL and tenant
    /// scope are mandatory; Sentinel never switches back to SQLite after an
    /// enterprise backend is requested.
    pub(crate) fn from_env(graph_name: &str) -> Result<Self, String> {
        let backend = match std::env::var(Self::BACKEND_ENV) {
            Ok(value) => {
                let backend = value.trim();
                if backend.is_empty() {
                    return Err(format!(
                        "{} is set but empty; expected sqlite, postgres, or redis",
                        Self::BACKEND_ENV
                    ));
                }
                backend.to_ascii_lowercase()
            }
            Err(std::env::VarError::NotPresent) => "sqlite".to_string(),
            Err(err) => return Err(format!("failed to read {}: {err}", Self::BACKEND_ENV)),
        };
        match backend.as_str() {
            "sqlite" => Ok(Self::Sqlite {
                database_path: sqlite_path(graph_name)?,
            }),
            "postgres" | "postgresql" => {
                let database_url = std::env::var(Self::POSTGRES_URL_ENV)
                    .map_err(|_| {
                        format!(
                            "{}=postgres requires {}",
                            Self::BACKEND_ENV,
                            Self::POSTGRES_URL_ENV
                        )
                    })?
                    .trim()
                    .to_string();
                if database_url.is_empty() {
                    return Err(format!(
                        "{}=postgres requires non-empty {}",
                        Self::BACKEND_ENV,
                        Self::POSTGRES_URL_ENV
                    ));
                }
                let schema = optional_non_empty_env(Self::POSTGRES_SCHEMA_ENV)?;
                require_enterprise_tenant_scope(Self::BACKEND_ENV, "postgres")?;
                Ok(Self::Postgres {
                    database_url,
                    schema,
                })
            }
            "redis" | "redis-checkpoint" => {
                let redis_url = std::env::var(Self::REDIS_URL_ENV)
                    .map_err(|_| {
                        format!(
                            "{}=redis requires {}",
                            Self::BACKEND_ENV,
                            Self::REDIS_URL_ENV
                        )
                    })?
                    .trim()
                    .to_string();
                if redis_url.is_empty() {
                    return Err(format!(
                        "{}=redis requires non-empty {}",
                        Self::BACKEND_ENV,
                        Self::REDIS_URL_ENV
                    ));
                }
                let ttl_seconds = optional_positive_u64_env(Self::REDIS_TTL_SECS_ENV)?;
                require_enterprise_tenant_scope(Self::BACKEND_ENV, "redis")?;
                Ok(Self::Redis {
                    redis_url,
                    ttl_seconds,
                })
            }
            other => Err(format!(
                "unsupported decision graph checkpointer backend '{other}' from {}; expected sqlite, postgres, or redis",
                Self::BACKEND_ENV
            )),
        }
    }

    #[must_use]
    pub(crate) fn backend_name(&self) -> &'static str {
        match self {
            Self::Sqlite { .. } => "sqlite",
            Self::Postgres { .. } => "postgres",
            Self::Redis { .. } => "redis",
        }
    }

    #[must_use]
    pub(crate) fn scope_name(&self) -> String {
        match self {
            Self::Sqlite { database_path } => format!("database_path:{database_path}"),
            Self::Postgres { schema, .. } => {
                format!("schema:{}", schema.as_deref().unwrap_or("public"))
            }
            Self::Redis { ttl_seconds, .. } => match ttl_seconds {
                Some(ttl_seconds) => format!("ttl_seconds:{ttl_seconds}"),
                None => "ttl_seconds:none".to_string(),
            },
        }
    }
}

fn optional_non_empty_env(name: &str) -> Result<Option<String>, String> {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Err(format!("{name} is set but empty"));
            }
            Ok(Some(value.to_string()))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(format!("failed to read {name}: {err}")),
    }
}

fn optional_positive_u64_env(name: &str) -> Result<Option<u64>, String> {
    let value = match std::env::var(name) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(err) => return Err(format!("failed to read {name}: {err}")),
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{name} is set but empty"));
    }
    let parsed = value
        .parse::<u64>()
        .map_err(|err| format!("{name} must be a positive integer number of seconds: {err}"))?;
    if parsed == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(Some(parsed))
}

fn require_enterprise_tenant_scope(backend_env: &str, backend: &str) -> Result<(), String> {
    if tenant_scope_from_env()?.is_some() {
        return Ok(());
    }

    Err(format!(
        "{backend_env}={backend} requires {LANGGRAPH_TENANT_ENV} so LangGraph checkpoint thread_id values are tenant-scoped"
    ))
}

/// Resolve the SQLite checkpoint database path for a named decision graph.
///
/// Databases live under Sentinel's state directory and are keyed by graph
/// family; individual decisions are still isolated by LangGraph `thread_id`.
pub(crate) fn sqlite_path(graph_name: &str) -> Result<String, String> {
    let dir = crate::state_store::state_dir().join("decision-graphs");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create decision graph checkpoint dir: {e}"))?;
    Ok(dir
        .join(format!("{graph_name}.db"))
        .to_string_lossy()
        .to_string())
}

/// Build a durable decision-graph checkpointer from env-selected backend.
pub(crate) async fn checkpointer_for_graph(
    graph_name: &str,
) -> Result<DecisionGraphCheckpointer, String> {
    let config = DecisionGraphCheckpointerConfig::from_env(graph_name)?;
    checkpointer_for_config(config).await
}

/// Build a durable decision-graph checkpointer from explicit config.
///
/// If a selected backend is missing from the build features, this errors. It
/// never silently changes backend.
pub(crate) async fn checkpointer_for_config(
    config: DecisionGraphCheckpointerConfig,
) -> Result<DecisionGraphCheckpointer, String> {
    let backend = config.backend_name();
    let scope = config.scope_name();
    let saver = match config {
        DecisionGraphCheckpointerConfig::Sqlite { database_path } => {
            sqlite_checkpointer(&database_path).await
        }
        DecisionGraphCheckpointerConfig::Postgres {
            database_url,
            schema,
        } => postgres_checkpointer(&database_url, schema.as_deref()).await,
        DecisionGraphCheckpointerConfig::Redis {
            redis_url,
            ttl_seconds,
        } => redis_checkpointer(&redis_url, ttl_seconds).await,
    }?;
    Ok(DecisionGraphCheckpointer {
        saver,
        backend,
        scope,
    })
}

#[cfg(feature = "sqlite")]
async fn sqlite_checkpointer(database_path: &str) -> Result<Arc<dyn CheckpointSaver>, String> {
    let checkpointer = SqliteCheckpointer::new(database_path)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(checkpointer))
}

#[cfg(not(feature = "sqlite"))]
async fn sqlite_checkpointer(_database_path: &str) -> Result<Arc<dyn CheckpointSaver>, String> {
    Err(
        "decision graph SQLite checkpointer requested, but sentinel-infrastructure was built without the sqlite feature"
            .to_string(),
    )
}

#[cfg(feature = "postgres")]
async fn postgres_checkpointer(
    database_url: &str,
    schema: Option<&str>,
) -> Result<Arc<dyn CheckpointSaver>, String> {
    let checkpointer = match schema {
        Some(schema) => PostgresCheckpointer::with_schema(database_url, schema).await,
        None => PostgresCheckpointer::new(database_url).await,
    }
    .map_err(|e| e.to_string())?;
    Ok(Arc::new(checkpointer))
}

#[cfg(not(feature = "postgres"))]
async fn postgres_checkpointer(
    _database_url: &str,
    _schema: Option<&str>,
) -> Result<Arc<dyn CheckpointSaver>, String> {
    Err(
        "decision graph Postgres checkpointer requested, but sentinel-infrastructure was built without the postgres feature"
            .to_string(),
    )
}

#[cfg(feature = "redis")]
async fn redis_checkpointer(
    redis_url: &str,
    ttl_seconds: Option<u64>,
) -> Result<Arc<dyn CheckpointSaver>, String> {
    let checkpointer = match ttl_seconds {
        Some(ttl_seconds) => RedisCheckpointer::with_ttl(redis_url, ttl_seconds).await,
        None => RedisCheckpointer::new(redis_url).await,
    }
    .map_err(|e| e.to_string())?;
    Ok(Arc::new(checkpointer))
}

#[cfg(not(feature = "redis"))]
async fn redis_checkpointer(
    _redis_url: &str,
    _ttl_seconds: Option<u64>,
) -> Result<Arc<dyn CheckpointSaver>, String> {
    Err(
        "decision graph Redis checkpointer requested, but sentinel-infrastructure was built without the redis feature"
            .to_string(),
    )
}

/// Derive a checkpoint thread id for one immutable decision-graph run.
///
/// Durable LangGraph execution resumes by `thread_id`. A bare ticket id would
/// make a later audit of the same ticket resume an old terminal checkpoint.
/// Hashing the serialized input keeps retries for the same decision idempotent
/// while giving changed facts/verdicts a fresh run thread under the same graph.
///
/// When `SENTINEL_LANGGRAPH_TENANT` is set, the tenant is part of the thread id
/// so hosted distributed checkpointers can safely share storage without
/// cross-tenant resume collisions.
pub(crate) fn run_thread_id<T: Serialize>(
    graph_name: &str,
    identifier: &str,
    input: &T,
) -> Result<String, String> {
    let bytes = serde_json::to_vec(input)
        .map_err(|e| format!("failed to serialize {graph_name} graph input: {e}"))?;
    let digest = Sha256::digest(&bytes);
    let base = format!("{graph_name}:{identifier}:{}", encode_hex(&digest));
    tenant_scoped_thread_id(base, tenant_scope_from_env()?.as_deref())
}

fn tenant_scoped_thread_id(base: String, tenant: Option<&str>) -> Result<String, String> {
    tenant_scoped_langgraph_thread_id(base, tenant)
}

fn tenant_scope_from_env() -> Result<Option<String>, String> {
    let value = match std::env::var(LANGGRAPH_TENANT_ENV) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(err) => return Err(format!("failed to read {LANGGRAPH_TENANT_ENV}: {err}")),
    };
    let tenant = value.trim();
    if tenant.is_empty() {
        return Err(format!("{LANGGRAPH_TENANT_ENV} is set but empty"));
    }
    validate_tenant_scope(tenant)?;
    Ok(Some(tenant.to_string()))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    static DECISION_GRAPH_CHECKPOINTER_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_decision_graph_checkpointer_env<R>(
        backend: Option<&str>,
        postgres_url: Option<&str>,
        postgres_schema: Option<&str>,
        tenant_scope: Option<&str>,
        f: impl FnOnce() -> R,
    ) -> R {
        with_decision_graph_checkpointer_env_full(
            backend,
            postgres_url,
            postgres_schema,
            None,
            None,
            tenant_scope,
            f,
        )
    }

    fn with_decision_graph_checkpointer_env_full<R>(
        backend: Option<&str>,
        postgres_url: Option<&str>,
        postgres_schema: Option<&str>,
        redis_url: Option<&str>,
        redis_ttl_secs: Option<&str>,
        tenant_scope: Option<&str>,
        f: impl FnOnce() -> R,
    ) -> R {
        let _guard = DECISION_GRAPH_CHECKPOINTER_ENV_LOCK
            .lock()
            .expect("decision graph checkpointer env lock poisoned");
        let previous_backend = std::env::var_os(DecisionGraphCheckpointerConfig::BACKEND_ENV);
        let previous_url = std::env::var_os(DecisionGraphCheckpointerConfig::POSTGRES_URL_ENV);
        let previous_schema =
            std::env::var_os(DecisionGraphCheckpointerConfig::POSTGRES_SCHEMA_ENV);
        let previous_redis_url = std::env::var_os(DecisionGraphCheckpointerConfig::REDIS_URL_ENV);
        let previous_redis_ttl =
            std::env::var_os(DecisionGraphCheckpointerConfig::REDIS_TTL_SECS_ENV);
        let previous_tenant = std::env::var_os(LANGGRAPH_TENANT_ENV);

        match backend {
            Some(value) => std::env::set_var(DecisionGraphCheckpointerConfig::BACKEND_ENV, value),
            None => std::env::remove_var(DecisionGraphCheckpointerConfig::BACKEND_ENV),
        }
        match postgres_url {
            Some(value) => {
                std::env::set_var(DecisionGraphCheckpointerConfig::POSTGRES_URL_ENV, value);
            }
            None => std::env::remove_var(DecisionGraphCheckpointerConfig::POSTGRES_URL_ENV),
        }
        match postgres_schema {
            Some(value) => {
                std::env::set_var(DecisionGraphCheckpointerConfig::POSTGRES_SCHEMA_ENV, value);
            }
            None => std::env::remove_var(DecisionGraphCheckpointerConfig::POSTGRES_SCHEMA_ENV),
        }
        match redis_url {
            Some(value) => {
                std::env::set_var(DecisionGraphCheckpointerConfig::REDIS_URL_ENV, value);
            }
            None => std::env::remove_var(DecisionGraphCheckpointerConfig::REDIS_URL_ENV),
        }
        match redis_ttl_secs {
            Some(value) => {
                std::env::set_var(DecisionGraphCheckpointerConfig::REDIS_TTL_SECS_ENV, value);
            }
            None => std::env::remove_var(DecisionGraphCheckpointerConfig::REDIS_TTL_SECS_ENV),
        }
        match tenant_scope {
            Some(value) => std::env::set_var(LANGGRAPH_TENANT_ENV, value),
            None => std::env::remove_var(LANGGRAPH_TENANT_ENV),
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        match previous_backend {
            Some(value) => {
                std::env::set_var(DecisionGraphCheckpointerConfig::BACKEND_ENV, value);
            }
            None => std::env::remove_var(DecisionGraphCheckpointerConfig::BACKEND_ENV),
        }
        match previous_url {
            Some(value) => {
                std::env::set_var(DecisionGraphCheckpointerConfig::POSTGRES_URL_ENV, value);
            }
            None => std::env::remove_var(DecisionGraphCheckpointerConfig::POSTGRES_URL_ENV),
        }
        match previous_schema {
            Some(value) => {
                std::env::set_var(DecisionGraphCheckpointerConfig::POSTGRES_SCHEMA_ENV, value);
            }
            None => std::env::remove_var(DecisionGraphCheckpointerConfig::POSTGRES_SCHEMA_ENV),
        }
        match previous_redis_url {
            Some(value) => {
                std::env::set_var(DecisionGraphCheckpointerConfig::REDIS_URL_ENV, value);
            }
            None => std::env::remove_var(DecisionGraphCheckpointerConfig::REDIS_URL_ENV),
        }
        match previous_redis_ttl {
            Some(value) => {
                std::env::set_var(DecisionGraphCheckpointerConfig::REDIS_TTL_SECS_ENV, value);
            }
            None => std::env::remove_var(DecisionGraphCheckpointerConfig::REDIS_TTL_SECS_ENV),
        }
        match previous_tenant {
            Some(value) => std::env::set_var(LANGGRAPH_TENANT_ENV, value),
            None => std::env::remove_var(LANGGRAPH_TENANT_ENV),
        }

        match result {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    #[test]
    fn decision_graph_checkpointer_config_defaults_to_sqlite() {
        with_decision_graph_checkpointer_env(None, None, None, None, || {
            let config =
                DecisionGraphCheckpointerConfig::from_env("severity").expect("sqlite config");
            match config {
                DecisionGraphCheckpointerConfig::Sqlite { database_path } => {
                    assert!(database_path.ends_with("decision-graphs/severity.db"));
                    assert_eq!(
                        DecisionGraphCheckpointerConfig::Sqlite {
                            database_path: database_path.clone(),
                        }
                        .scope_name(),
                        format!("database_path:{database_path}")
                    );
                }
                DecisionGraphCheckpointerConfig::Postgres { .. } => {
                    panic!("default must be sqlite")
                }
                DecisionGraphCheckpointerConfig::Redis { .. } => panic!("default must be sqlite"),
            }
        });
    }

    #[test]
    fn decision_graph_checkpointer_config_accepts_postgres_schema() {
        with_decision_graph_checkpointer_env(
            Some("postgres"),
            Some("postgres://sentinel:sentinel@localhost/sentinel"),
            Some("sentinel_decisions"),
            Some("legatus_ai"),
            || {
                let config =
                    DecisionGraphCheckpointerConfig::from_env("ignored").expect("postgres config");
                assert_eq!(
                    config,
                    DecisionGraphCheckpointerConfig::Postgres {
                        database_url: "postgres://sentinel:sentinel@localhost/sentinel".to_string(),
                        schema: Some("sentinel_decisions".to_string()),
                    }
                );
                assert_eq!(config.backend_name(), "postgres");
                assert_eq!(config.scope_name(), "schema:sentinel_decisions");
            },
        );
    }

    #[test]
    fn decision_graph_checkpointer_config_uses_public_postgres_scope_without_schema() {
        with_decision_graph_checkpointer_env(
            Some("postgres"),
            Some("postgres://sentinel:sentinel@localhost/sentinel"),
            None,
            Some("legatus_ai"),
            || {
                let config =
                    DecisionGraphCheckpointerConfig::from_env("ignored").expect("postgres config");
                assert_eq!(config.backend_name(), "postgres");
                assert_eq!(config.scope_name(), "schema:public");
            },
        );
    }

    #[test]
    fn decision_graph_checkpointer_config_accepts_redis_ttl() {
        with_decision_graph_checkpointer_env_full(
            Some("redis"),
            None,
            None,
            Some("redis://localhost:6379/0"),
            Some("3600"),
            Some("legatus_ai"),
            || {
                let config =
                    DecisionGraphCheckpointerConfig::from_env("ignored").expect("redis config");
                assert_eq!(
                    config,
                    DecisionGraphCheckpointerConfig::Redis {
                        redis_url: "redis://localhost:6379/0".to_string(),
                        ttl_seconds: Some(3600),
                    }
                );
                assert_eq!(config.backend_name(), "redis");
                assert_eq!(config.scope_name(), "ttl_seconds:3600");
            },
        );
    }

    #[test]
    fn decision_graph_checkpointer_config_accepts_redis_without_ttl() {
        with_decision_graph_checkpointer_env_full(
            Some("redis-checkpoint"),
            None,
            None,
            Some("redis://localhost:6379/1"),
            None,
            Some("legatus_ai"),
            || {
                let config =
                    DecisionGraphCheckpointerConfig::from_env("ignored").expect("redis config");
                assert_eq!(config.backend_name(), "redis");
                assert_eq!(config.scope_name(), "ttl_seconds:none");
            },
        );
    }

    #[test]
    fn decision_graph_checkpointer_config_requires_postgres_url() {
        with_decision_graph_checkpointer_env(Some("postgres"), None, None, None, || {
            let err = DecisionGraphCheckpointerConfig::from_env("severity")
                .expect_err("postgres URL must be required");
            assert!(err.contains(DecisionGraphCheckpointerConfig::POSTGRES_URL_ENV));
            assert!(
                !err.contains("severity.db"),
                "postgres selection must not use sqlite: {err}"
            );
        });
    }

    #[test]
    fn decision_graph_checkpointer_config_requires_tenant_scope_for_postgres() {
        with_decision_graph_checkpointer_env(
            Some("postgres"),
            Some("postgres://sentinel:sentinel@localhost/sentinel"),
            None,
            None,
            || {
                let err = DecisionGraphCheckpointerConfig::from_env("severity")
                    .expect_err("postgres config must require tenant scope");
                assert!(err.contains(DecisionGraphCheckpointerConfig::BACKEND_ENV));
                assert!(err.contains(LANGGRAPH_TENANT_ENV));
                assert!(err.contains("tenant-scoped"));
                assert!(
                    !err.contains("severity.db"),
                    "postgres selection must not use sqlite: {err}"
                );
            },
        );
    }

    #[test]
    fn decision_graph_checkpointer_config_requires_redis_url() {
        with_decision_graph_checkpointer_env_full(
            Some("redis"),
            None,
            None,
            None,
            None,
            None,
            || {
                let err = DecisionGraphCheckpointerConfig::from_env("severity")
                    .expect_err("redis URL must be required");
                assert!(err.contains(DecisionGraphCheckpointerConfig::REDIS_URL_ENV));
                assert!(
                    !err.contains("severity.db"),
                    "redis selection must not use sqlite: {err}"
                );
            },
        );
    }

    #[test]
    fn decision_graph_checkpointer_config_requires_tenant_scope_for_redis() {
        with_decision_graph_checkpointer_env_full(
            Some("redis"),
            None,
            None,
            Some("redis://localhost:6379/0"),
            None,
            None,
            || {
                let err = DecisionGraphCheckpointerConfig::from_env("severity")
                    .expect_err("redis config must require tenant scope");
                assert!(err.contains(DecisionGraphCheckpointerConfig::BACKEND_ENV));
                assert!(err.contains(LANGGRAPH_TENANT_ENV));
                assert!(err.contains("tenant-scoped"));
                assert!(
                    !err.contains("severity.db"),
                    "redis selection must not use sqlite: {err}"
                );
            },
        );
    }

    #[test]
    fn decision_graph_checkpointer_config_rejects_empty_backend_env() {
        with_decision_graph_checkpointer_env(Some("   "), None, None, None, || {
            let err = DecisionGraphCheckpointerConfig::from_env("severity")
                .expect_err("empty backend must fail");
            assert!(err.contains(DecisionGraphCheckpointerConfig::BACKEND_ENV));
            assert!(err.contains("set but empty"));
            assert!(
                !err.contains("severity.db"),
                "empty backend must not fall back to sqlite: {err}"
            );
        });
    }

    #[test]
    fn decision_graph_checkpointer_config_rejects_empty_postgres_schema_env() {
        with_decision_graph_checkpointer_env(
            Some("postgres"),
            Some("postgres://sentinel:sentinel@localhost/sentinel"),
            Some("   "),
            Some("legatus_ai"),
            || {
                let err = DecisionGraphCheckpointerConfig::from_env("severity")
                    .expect_err("empty schema must fail when configured");
                assert!(err.contains(DecisionGraphCheckpointerConfig::POSTGRES_SCHEMA_ENV));
                assert!(err.contains("set but empty"));
            },
        );
    }

    #[test]
    fn decision_graph_checkpointer_config_rejects_invalid_redis_ttl_env() {
        with_decision_graph_checkpointer_env_full(
            Some("redis"),
            None,
            None,
            Some("redis://localhost:6379/0"),
            Some("0"),
            Some("legatus_ai"),
            || {
                let err = DecisionGraphCheckpointerConfig::from_env("severity")
                    .expect_err("zero Redis TTL must fail when configured");
                assert!(err.contains(DecisionGraphCheckpointerConfig::REDIS_TTL_SECS_ENV));
                assert!(err.contains("greater than zero"));
            },
        );
    }

    #[test]
    fn decision_graph_checkpointer_config_rejects_unknown_backend() {
        with_decision_graph_checkpointer_env(Some("unsupported"), None, None, None, || {
            let err = DecisionGraphCheckpointerConfig::from_env("severity")
                .expect_err("unknown backend must fail");
            assert!(err.contains("expected sqlite, postgres, or redis"));
        });
    }

    #[test]
    fn decision_graph_thread_id_is_tenant_scoped_when_configured() {
        let scoped =
            tenant_scoped_thread_id("severity:SEN-123:abc123".to_string(), Some("legatus_ai"))
                .expect("valid tenant");

        assert_eq!(scoped, "tenant:legatus_ai:severity:SEN-123:abc123");
    }

    #[test]
    fn decision_graph_thread_id_rejects_malformed_tenant_scope() {
        let err =
            tenant_scoped_thread_id("severity:SEN-123:abc123".to_string(), Some("tenant:escape"))
                .expect_err("tenant delimiter injection must fail");

        assert!(err.contains(LANGGRAPH_TENANT_ENV));
        assert!(err.contains("invalid characters"));
    }

    #[cfg(not(feature = "postgres"))]
    #[tokio::test]
    async fn postgres_decision_checkpointer_request_fails_without_postgres_feature() {
        let result = checkpointer_for_config(DecisionGraphCheckpointerConfig::Postgres {
            database_url: "postgres://sentinel:sentinel@localhost/sentinel".to_string(),
            schema: Some("sentinel_decisions".to_string()),
        })
        .await;
        let err = match result {
            Ok(_) => panic!("postgres backend must require postgres feature"),
            Err(err) => err,
        };
        assert!(err.contains("built without the postgres feature"));
    }

    #[cfg(not(feature = "redis"))]
    #[tokio::test]
    async fn redis_decision_checkpointer_request_fails_without_redis_feature() {
        let result = checkpointer_for_config(DecisionGraphCheckpointerConfig::Redis {
            redis_url: "redis://localhost:6379/0".to_string(),
            ttl_seconds: Some(60),
        })
        .await;
        let err = match result {
            Ok(_) => panic!("redis backend must require redis feature"),
            Err(err) => err,
        };
        assert!(err.contains("built without the redis feature"));
    }
}
