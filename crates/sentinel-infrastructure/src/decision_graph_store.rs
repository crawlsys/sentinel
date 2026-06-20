//! Durable storage paths for LangGraph-powered infrastructure decision graphs.

use std::sync::Arc;

use langgraph_core::application::services::CompilationResult;
use langgraph_core::ports::CheckpointSaver;
use sentinel_domain::langgraph_thread::{
    tenant_scoped_thread_id as tenant_scoped_langgraph_thread_id, validate_tenant_scope,
    validate_thread_id_component, LANGGRAPH_TENANT_ENV,
};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest as _, Sha256};

#[cfg(feature = "postgres")]
use langgraph_core::PostgresCheckpointer;
#[cfg(feature = "redis")]
use langgraph_core::RedisCheckpointer;
#[cfg(feature = "sqlite")]
use langgraph_core::SqliteCheckpointer;

const CHECKPOINTER_BACKEND_METADATA: &str = "sentinel.checkpointer_backend";
const CHECKPOINTER_TENANT_SCOPE_METADATA: &str = "sentinel.checkpointer_tenant_scope";

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
    tenant_scope: Option<String>,
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
    pub(crate) fn tenant_scope(&self) -> Option<&str> {
        self.tenant_scope.as_deref()
    }

    #[must_use]
    pub(crate) fn tenant_scope_metadata_value(&self) -> &str {
        self.tenant_scope().unwrap_or("")
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
        let backend = decision_checkpointer_backend_from_env()?;
        match backend.as_str() {
            "sqlite" => Ok(Self::Sqlite {
                database_path: sqlite_path(graph_name)?,
            }),
            "postgres" => {
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
            "redis" => {
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
            _ => unreachable!("decision_checkpointer_backend_from_env only returns known backends"),
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

fn decision_checkpointer_backend_from_env() -> Result<String, String> {
    let backend = match std::env::var(DecisionGraphCheckpointerConfig::BACKEND_ENV) {
        Ok(value) => {
            let backend = value.trim();
            if backend.is_empty() {
                return Err(format!(
                    "{} is set but empty; expected sqlite, postgres, or redis",
                    DecisionGraphCheckpointerConfig::BACKEND_ENV
                ));
            }
            backend.to_ascii_lowercase()
        }
        Err(std::env::VarError::NotPresent) => return Ok("sqlite".to_string()),
        Err(err) => {
            return Err(format!(
                "failed to read {}: {err}",
                DecisionGraphCheckpointerConfig::BACKEND_ENV
            ));
        }
    };

    match backend.as_str() {
        "sqlite" => Ok("sqlite".to_string()),
        "postgres" => Ok("postgres".to_string()),
        "redis" => Ok("redis".to_string()),
        other => Err(format!(
            "unsupported decision graph checkpointer backend '{other}' from {}; expected sqlite, postgres, or redis",
            DecisionGraphCheckpointerConfig::BACKEND_ENV
        )),
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

#[cfg(any(feature = "postgres", feature = "redis", test))]
fn tenant_scope_for_checkpointer_backend(backend: &str) -> Result<Option<String>, String> {
    match backend {
        "sqlite" => Ok(None),
        "postgres" | "redis" => tenant_scope_from_env()?.map_or_else(
            || {
                Err(format!(
                    "decision graph {backend} checkpointer requires {LANGGRAPH_TENANT_ENV} so LangGraph checkpoint thread_id values are tenant-scoped"
                ))
            },
            |tenant| Ok(Some(tenant)),
        ),
        other => Err(format!(
            "unsupported decision graph checkpointer backend '{other}'"
        )),
    }
}

fn tenant_scope_for_checkpointer_config(
    config: &DecisionGraphCheckpointerConfig,
) -> Result<Option<String>, String> {
    match config {
        DecisionGraphCheckpointerConfig::Sqlite { .. } => Ok(None),
        DecisionGraphCheckpointerConfig::Postgres { .. } => {
            tenant_scope_for_postgres_checkpointer_config()
        }
        DecisionGraphCheckpointerConfig::Redis { .. } => {
            tenant_scope_for_redis_checkpointer_config()
        }
    }
}

#[cfg(feature = "postgres")]
fn tenant_scope_for_postgres_checkpointer_config() -> Result<Option<String>, String> {
    tenant_scope_for_checkpointer_backend("postgres")
}

#[cfg(not(feature = "postgres"))]
fn tenant_scope_for_postgres_checkpointer_config() -> Result<Option<String>, String> {
    Ok(None)
}

#[cfg(feature = "redis")]
fn tenant_scope_for_redis_checkpointer_config() -> Result<Option<String>, String> {
    tenant_scope_for_checkpointer_backend("redis")
}

#[cfg(not(feature = "redis"))]
fn tenant_scope_for_redis_checkpointer_config() -> Result<Option<String>, String> {
    Ok(None)
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
    let tenant_scope = tenant_scope_for_checkpointer_config(&config)?;
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
        tenant_scope,
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
/// make a later audit of the same ticket resume an old terminal checkpoint,
/// and raw identifiers can contain separators or control characters from user
/// input. Hashing both the identifier and serialized input keeps retries for
/// the same decision idempotent while giving changed facts/verdicts a fresh run
/// thread under the same graph.
///
/// Hosted distributed checkpointers require `SENTINEL_LANGGRAPH_TENANT` and
/// include it in the thread id so shared storage cannot resume across tenant
/// boundaries. Local SQLite ids stay unscoped even if a tenant env var is set.
#[cfg(test)]
pub(crate) fn run_thread_id<T: Serialize>(
    graph_name: &str,
    identifier: &str,
    input: &T,
) -> Result<String, String> {
    let backend = decision_checkpointer_backend_from_env()?;
    run_thread_id_for_backend(&backend, graph_name, identifier, input)
}

/// Derive a checkpoint thread id from the compiled graph's backend metadata.
///
/// Production decision graph runs use this helper so thread identity follows
/// the graph that is actually executing, not a mutable process env value that
/// may differ from an explicitly supplied checkpointer.
pub(crate) fn run_thread_id_for_compiled<S, T>(
    compiled: &CompilationResult<S>,
    graph_name: &str,
    identifier: &str,
    input: &T,
) -> Result<String, String>
where
    S: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    T: Serialize,
{
    let backend = compiled_checkpointer_backend(graph_name, compiled)?;
    let tenant_scope = compiled_checkpointer_tenant_scope(graph_name, compiled, &backend)?;
    run_thread_id_for_tenant_scope(tenant_scope.as_deref(), graph_name, identifier, input)
}

#[cfg(test)]
fn run_thread_id_for_backend<T: Serialize>(
    backend: &str,
    graph_name: &str,
    identifier: &str,
    input: &T,
) -> Result<String, String> {
    let base = decision_thread_id_base(graph_name, identifier, input)?;
    let tenant_scope = tenant_scope_for_checkpointer_backend(backend)?;
    tenant_scoped_thread_id(base, tenant_scope.as_deref())
}

fn run_thread_id_for_tenant_scope<T: Serialize>(
    tenant_scope: Option<&str>,
    graph_name: &str,
    identifier: &str,
    input: &T,
) -> Result<String, String> {
    let base = decision_thread_id_base(graph_name, identifier, input)?;
    tenant_scoped_thread_id(base, tenant_scope)
}

fn decision_thread_id_base<T: Serialize>(
    graph_name: &str,
    identifier: &str,
    input: &T,
) -> Result<String, String> {
    validate_thread_id_component(graph_name, "graph_name")?;
    let bytes = serde_json::to_vec(input)
        .map_err(|e| format!("failed to serialize {graph_name} graph input: {e}"))?;
    let input_digest = Sha256::digest(&bytes);
    let identifier_digest = Sha256::digest(identifier.as_bytes());
    Ok(format!(
        "{graph_name}:id-{}:input-{}",
        encode_hex(identifier_digest.as_ref()),
        encode_hex(input_digest.as_ref())
    ))
}

fn compiled_checkpointer_backend<S>(
    graph_name: &str,
    compiled: &CompilationResult<S>,
) -> Result<String, String>
where
    S: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    if compiled.checkpointer.is_none() {
        return Err(format!(
            "decision graph {graph_name} thread_id requires a configured LangGraph checkpointer"
        ));
    }

    let mut backend: Option<String> = None;
    let mut saw_node = false;
    for node_id in compiled.graph.node_ids() {
        saw_node = true;
        let node = compiled
            .graph
            .node_introspection(node_id.as_str())
            .ok_or_else(|| {
                format!(
                    "{graph_name} decision graph node '{}' is missing compiled introspection",
                    node_id.as_str()
                )
            })?;
        let node_backend = node
            .metadata
            .get(CHECKPOINTER_BACKEND_METADATA)
            .ok_or_else(|| {
                format!(
                    "{graph_name} decision graph node '{}' is missing {CHECKPOINTER_BACKEND_METADATA} metadata",
                    node.id
                )
            })?;
        match backend.as_deref() {
            Some(existing) if existing != *node_backend => {
                return Err(format!(
                    "{graph_name} decision graph has inconsistent checkpointer backend metadata: expected {existing}, node '{}' had {node_backend}",
                    node.id
                ));
            }
            Some(_) => {}
            None => backend = Some((*node_backend).to_string()),
        }
    }

    if !saw_node {
        return Err(format!(
            "{graph_name} decision graph must contain at least one node with {CHECKPOINTER_BACKEND_METADATA} metadata"
        ));
    }

    let backend = backend.ok_or_else(|| {
        format!("{graph_name} decision graph is missing {CHECKPOINTER_BACKEND_METADATA} metadata")
    })?;
    match backend.as_str() {
        "sqlite" | "postgres" | "redis" => Ok(backend),
        other => Err(format!(
            "unsupported decision graph checkpointer backend '{other}' from compiled {graph_name} metadata"
        )),
    }
}

fn compiled_checkpointer_tenant_scope<S>(
    graph_name: &str,
    compiled: &CompilationResult<S>,
    backend: &str,
) -> Result<Option<String>, String>
where
    S: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    let mut tenant_scope: Option<String> = None;
    let mut saw_node = false;
    for node_id in compiled.graph.node_ids() {
        saw_node = true;
        let node = compiled
            .graph
            .node_introspection(node_id.as_str())
            .ok_or_else(|| {
                format!(
                    "{graph_name} decision graph node '{}' is missing compiled introspection",
                    node_id.as_str()
                )
            })?;
        let node_tenant_scope = node
            .metadata
            .get(CHECKPOINTER_TENANT_SCOPE_METADATA)
            .ok_or_else(|| {
                format!(
                    "{graph_name} decision graph node '{}' is missing {CHECKPOINTER_TENANT_SCOPE_METADATA} metadata",
                    node.id
                )
            })?;
        match tenant_scope.as_deref() {
            Some(existing) if existing != *node_tenant_scope => {
                return Err(format!(
                    "{graph_name} decision graph has inconsistent checkpointer tenant metadata: expected {existing:?}, node '{}' had {node_tenant_scope:?}",
                    node.id
                ));
            }
            Some(_) => {}
            None => tenant_scope = Some((*node_tenant_scope).to_string()),
        }
    }

    if !saw_node {
        return Err(format!(
            "{graph_name} decision graph must contain at least one node with {CHECKPOINTER_TENANT_SCOPE_METADATA} metadata"
        ));
    }

    let tenant_scope = tenant_scope.ok_or_else(|| {
        format!(
            "{graph_name} decision graph is missing {CHECKPOINTER_TENANT_SCOPE_METADATA} metadata"
        )
    })?;
    match backend {
        "sqlite" => {
            if tenant_scope.is_empty() {
                Ok(None)
            } else {
                Err(format!(
                    "{graph_name} SQLite decision graph must not carry hosted tenant metadata"
                ))
            }
        }
        "postgres" | "redis" => {
            if tenant_scope.is_empty() {
                return Err(format!(
                    "{graph_name} {backend} decision graph requires non-empty {CHECKPOINTER_TENANT_SCOPE_METADATA} metadata"
                ));
            }
            validate_tenant_scope(&tenant_scope)?;
            Ok(Some(tenant_scope))
        }
        other => Err(format!(
            "unsupported decision graph checkpointer backend '{other}' from compiled {graph_name} metadata"
        )),
    }
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
    use langgraph_core::application::services::GraphCompiler;
    use langgraph_core::domain::value_objects::{NodeConfig, NodeError, StateSchema, END, START};
    use langgraph_core::StateGraphBuilder;
    use serde::Deserialize;

    static DECISION_GRAPH_CHECKPOINTER_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct TestDecisionState {
        identifier: String,
    }

    async fn compiled_test_graph(
        backend: &str,
        tenant_scope: &str,
    ) -> CompilationResult<TestDecisionState> {
        let checkpointer = checkpointer_for_config(DecisionGraphCheckpointerConfig::Sqlite {
            database_path: ":memory:".to_string(),
        })
        .await
        .expect("sqlite checkpointer");
        let schema = StateSchema::<TestDecisionState>::new().with_serializable_validation();
        let graph = StateGraphBuilder::<TestDecisionState>::with_schema(schema)
            .add_node_with_config(
                "classify",
                |state: &TestDecisionState| Ok::<_, NodeError>(state.clone()),
                NodeConfig::new()
                    .with_metadata("sentinel.graph", "severity")
                    .with_metadata("sentinel.node", "classify")
                    .with_metadata(CHECKPOINTER_BACKEND_METADATA, backend)
                    .with_metadata("sentinel.checkpointer_scope", "database_path::memory:")
                    .with_metadata(CHECKPOINTER_TENANT_SCOPE_METADATA, tenant_scope),
            )
            .add_edge(START, "classify")
            .add_edge("classify", END)
            .build()
            .expect("test graph");
        GraphCompiler::new()
            .with_checkpointer(checkpointer.into_saver())
            .compile_with_config(graph)
            .expect("compile test graph")
    }

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
            Some("redis"),
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
    fn decision_graph_checkpointer_config_rejects_backend_aliases_without_normalization() {
        with_decision_graph_checkpointer_env_full(
            Some("redis-checkpoint"),
            None,
            None,
            Some("redis://localhost:6379/1"),
            None,
            Some("legatus_ai"),
            || {
                let err = DecisionGraphCheckpointerConfig::from_env("severity")
                    .expect_err("redis-checkpoint alias must be rejected");

                assert!(err.contains(
                    "unsupported decision graph checkpointer backend 'redis-checkpoint'"
                ));
                assert!(err.contains("expected sqlite, postgres, or redis"));
            },
        );

        with_decision_graph_checkpointer_env(
            Some("postgresql"),
            Some("postgres://sentinel:sentinel@localhost/sentinel"),
            None,
            Some("legatus_ai"),
            || {
                let err = DecisionGraphCheckpointerConfig::from_env("severity")
                    .expect_err("postgresql alias must be rejected");

                assert!(
                    err.contains("unsupported decision graph checkpointer backend 'postgresql'")
                );
                assert!(err.contains("expected sqlite, postgres, or redis"));
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

    #[cfg(all(feature = "postgres", feature = "redis"))]
    #[test]
    fn explicit_hosted_decision_checkpointer_config_requires_tenant_scope() {
        with_decision_graph_checkpointer_env(None, None, None, None, || {
            let postgres_err =
                tenant_scope_for_checkpointer_config(&DecisionGraphCheckpointerConfig::Postgres {
                    database_url: "postgres://sentinel:sentinel@localhost/sentinel".to_string(),
                    schema: Some("sentinel_decisions".to_string()),
                })
                .expect_err("explicit postgres config must require tenant scope");
            assert!(postgres_err.contains(LANGGRAPH_TENANT_ENV));
            assert!(postgres_err.contains("tenant-scoped"));

            let redis_err =
                tenant_scope_for_checkpointer_config(&DecisionGraphCheckpointerConfig::Redis {
                    redis_url: "redis://localhost:6379/0".to_string(),
                    ttl_seconds: Some(60),
                })
                .expect_err("explicit redis config must require tenant scope");
            assert!(redis_err.contains(LANGGRAPH_TENANT_ENV));
            assert!(redis_err.contains("tenant-scoped"));
        });
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
    fn decision_graph_run_thread_id_ignores_tenant_for_local_sqlite_default() {
        with_decision_graph_checkpointer_env(None, None, None, Some("legatus_ai"), || {
            let thread_id = run_thread_id(
                "severity",
                "SEN-123",
                &serde_json::json!({"ticket": "SEN-123"}),
            )
            .expect("sqlite thread id");

            assert!(thread_id.starts_with("severity:id-"));
            assert!(thread_id.contains(":input-"));
            assert!(
                !thread_id.contains("SEN-123"),
                "decision graph thread ids must hash raw identifiers: {thread_id}"
            );
            assert!(
                !thread_id.starts_with("tenant:"),
                "local SQLite thread ids must not inherit hosted tenant scope: {thread_id}"
            );
        });
    }

    #[test]
    fn decision_graph_run_thread_id_uses_tenant_for_hosted_redis_backend() {
        with_decision_graph_checkpointer_env_full(
            Some("redis"),
            None,
            None,
            Some("redis://localhost:6379/0"),
            None,
            Some("legatus_ai"),
            || {
                let thread_id = run_thread_id(
                    "severity",
                    "SEN-123",
                    &serde_json::json!({"ticket": "SEN-123"}),
                )
                .expect("redis thread id");

                assert!(thread_id.starts_with("tenant:legatus_ai:severity:id-"));
                assert!(thread_id.contains(":input-"));
                assert!(
                    !thread_id.contains("SEN-123"),
                    "hosted decision graph thread ids must hash raw identifiers: {thread_id}"
                );
            },
        );
    }

    #[test]
    fn decision_graph_run_thread_id_rejects_postgres_alias_without_normalization() {
        with_decision_graph_checkpointer_env(
            Some("postgresql"),
            Some("postgres://sentinel:sentinel@localhost/sentinel"),
            None,
            Some("legatus_ai"),
            || {
                let err = run_thread_id(
                    "severity",
                    "SEN-123",
                    &serde_json::json!({"ticket": "SEN-123"}),
                )
                .expect_err("postgresql alias must be rejected");

                assert!(
                    err.contains("unsupported decision graph checkpointer backend 'postgresql'")
                );
                assert!(err.contains("expected sqlite, postgres, or redis"));
            },
        );
    }

    #[test]
    fn decision_graph_run_thread_id_hashes_unsafe_identifier() {
        with_decision_graph_checkpointer_env(None, None, None, None, || {
            let first = run_thread_id(
                "severity",
                "SEN:123\nescape",
                &serde_json::json!({"ticket": "SEN-123"}),
            )
            .expect("thread id");
            let second = run_thread_id(
                "severity",
                "SEN-123",
                &serde_json::json!({"ticket": "SEN-123"}),
            )
            .expect("thread id");

            assert_ne!(first, second);
            assert!(first.starts_with("severity:id-"));
            assert!(first.contains(":input-"));
            assert!(!first.contains("SEN:123"));
            assert!(!first.contains('\n'));
        });
    }

    #[test]
    fn decision_graph_run_thread_id_requires_tenant_for_hosted_backend() {
        with_decision_graph_checkpointer_env(
            Some("postgres"),
            Some("postgres://sentinel:sentinel@localhost/sentinel"),
            None,
            None,
            || {
                let err = run_thread_id(
                    "severity",
                    "SEN-123",
                    &serde_json::json!({"ticket": "SEN-123"}),
                )
                .expect_err("hosted backend must require tenant scope");

                assert!(err.contains("decision graph postgres checkpointer requires"));
                assert!(err.contains(LANGGRAPH_TENANT_ENV));
                assert!(err.contains("tenant-scoped"));
            },
        );
    }

    #[test]
    fn decision_graph_run_thread_id_rejects_unknown_backend_without_fallback() {
        with_decision_graph_checkpointer_env(Some("unsupported"), None, None, None, || {
            let err = run_thread_id(
                "severity",
                "SEN-123",
                &serde_json::json!({"ticket": "SEN-123"}),
            )
            .expect_err("unsupported backend must fail");

            assert!(err.contains("unsupported decision graph checkpointer backend 'unsupported'"));
            assert!(err.contains("expected sqlite, postgres, or redis"));
        });
    }

    #[tokio::test]
    async fn compiled_decision_graph_thread_id_uses_compiled_tenant_scope_after_env_drift() {
        let compiled = compiled_test_graph("redis", "legatus_ai").await;
        with_decision_graph_checkpointer_env_full(
            Some("redis"),
            None,
            None,
            Some("redis://localhost:6379/0"),
            None,
            Some("wrong_tenant"),
            || {
                let thread_id = run_thread_id_for_compiled(
                    &compiled,
                    "severity",
                    "SEN-123",
                    &serde_json::json!({"ticket": "SEN-123"}),
                )
                .expect("compiled hosted thread id");

                assert!(thread_id.starts_with("tenant:legatus_ai:severity:id-"));
                assert!(thread_id.contains(":input-"));
                assert!(
                    !thread_id.contains("SEN-123"),
                    "compiled hosted thread ids must hash raw identifiers: {thread_id}"
                );
            },
        );
    }

    #[tokio::test]
    async fn compiled_decision_graph_thread_id_rejects_missing_hosted_tenant_metadata() {
        let compiled = compiled_test_graph("postgres", "").await;
        let err = run_thread_id_for_compiled(
            &compiled,
            "severity",
            "SEN-123",
            &serde_json::json!({"ticket": "SEN-123"}),
        )
        .expect_err("hosted compiled graph must carry tenant metadata");

        assert!(err.contains("requires non-empty"));
        assert!(err.contains(CHECKPOINTER_TENANT_SCOPE_METADATA));
    }

    #[tokio::test]
    async fn compiled_decision_graph_thread_id_rejects_sqlite_tenant_metadata() {
        let compiled = compiled_test_graph("sqlite", "legatus_ai").await;
        let err = run_thread_id_for_compiled(
            &compiled,
            "severity",
            "SEN-123",
            &serde_json::json!({"ticket": "SEN-123"}),
        )
        .expect_err("sqlite compiled graph must not carry tenant metadata");

        assert!(err.contains("SQLite decision graph"));
        assert!(err.contains("must not carry hosted tenant metadata"));
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
