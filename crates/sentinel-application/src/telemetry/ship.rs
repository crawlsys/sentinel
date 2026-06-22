//! `sentinel telemetry ship` — idempotent spool drain to R2 (LEG-260).
//!
//! Walks the spool manifests (written by `telemetry collect`, LEG-259) and
//! PUTs each batch to the lake via one of two transports:
//!
//! - **Bearer worker** (preferred): `PUT {TELEMETRY_INGEST_URL}/{object_key}`
//!   with `Authorization: Bearer {TELEMETRY_INGEST_TOKEN}` against the
//!   Cloudflare ingest Worker (LEG-262). The Worker enforces first-write-wins
//!   idempotency, so a plain 200 means "the object is in the bucket" — no
//!   HEAD pre-check needed.
//! - **`SigV4` direct** (fallback): signed S3-compatible PUT/HEAD straight at
//!   the bucket endpoint, used when the `TELEMETRY_INGEST_*` pair is absent.
//!
//! Reliability contract (both transports):
//!
//! - **Idempotent retries** — the object key embeds the content sha256, so
//!   a retried/duplicated PUT overwrites identical bytes; before uploading we
//!   `HEAD` the key and skip the upload when the object already exists
//!   (same hash → no dupe, no wasted bytes).
//! - **Delete only after confirmed PUT** — spool files are removed only on
//!   a 2xx PUT (or a confirmed-already-present HEAD). Data file first, then
//!   manifest; a manifest whose data file is missing is recovered via HEAD
//!   (crash window after a confirmed PUT).
//! - **Retry with backoff + jitter** — transport errors and 5xx are retried
//!   up to `attempts` per object per run; 4xx is fatal (retrying won't
//!   help). Unshipped batches stay spooled for the next run — the spool IS
//!   the durable retry queue, so an endpoint outage just means everything
//!   waits.
//!
//! Credentials come from the environment — never hardcoded:
//!
//! | var | meaning |
//! |---|---|
//! | `TELEMETRY_INGEST_URL` | bearer mode: ingest Worker base URL (with `TELEMETRY_INGEST_TOKEN`, selects bearer transport) |
//! | `TELEMETRY_INGEST_TOKEN` | bearer mode: `Authorization: Bearer` token |
//! | `R2_ACCOUNT_ID` | Cloudflare account id → `https://{id}.r2.cloudflarestorage.com` |
//! | `R2_ENDPOINT` | optional explicit endpoint override (takes precedence; used for local fakes / MinIO / AWS) |
//! | `R2_ACCESS_KEY_ID` / `R2_SECRET_ACCESS_KEY` | S3-compatible key pair |
//! | `R2_BUCKET` | target bucket |
//! | `R2_REGION` | optional, defaults to `auto` (R2's region) |

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::time::Duration;

use super::sigv4::{self, SigningContext, EMPTY_PAYLOAD_SHA256};
use super::spool::{self, BatchManifest};

/// Attempts per object per run (plan §4: "3 attempts/run").
pub const DEFAULT_ATTEMPTS: u32 = 3;

/// Base backoff between attempts (doubles per attempt, plus jitter).
pub const DEFAULT_BACKOFF_BASE_MS: u64 = 500;

/// Everything `ship` needs to reach the bucket.
#[derive(Debug, Clone)]
pub struct ShipConfig {
    /// S3-compatible endpoint, no trailing slash, e.g.
    /// `https://{account}.r2.cloudflarestorage.com`.
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
    pub attempts: u32,
    pub backoff_base_ms: u64,
}

impl ShipConfig {
    /// Build from `R2_*` environment variables (see module docs). Errors
    /// name every missing variable so a misconfigured timer unit fails
    /// loudly and diagnosably.
    pub fn from_env() -> Result<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());

        let endpoint = if let Some(e) = var("R2_ENDPOINT") {
            e.trim_end_matches('/').to_string()
        } else {
            let account = var("R2_ACCOUNT_ID").ok_or_else(|| {
                anyhow::anyhow!(
                    "missing R2_ENDPOINT or R2_ACCOUNT_ID — set R2_ACCOUNT_ID (plus \
                     R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY / R2_BUCKET) in the \
                     environment, e.g. via `doppler run --`"
                )
            })?;
            format!("https://{account}.r2.cloudflarestorage.com")
        };
        let need = |k: &str| {
            var(k).ok_or_else(|| anyhow::anyhow!("missing required env var {k} (never hardcoded)"))
        };
        Ok(Self {
            endpoint,
            bucket: need("R2_BUCKET")?,
            access_key_id: need("R2_ACCESS_KEY_ID")?,
            secret_access_key: need("R2_SECRET_ACCESS_KEY")?,
            region: var("R2_REGION").unwrap_or_else(|| "auto".to_string()),
            attempts: DEFAULT_ATTEMPTS,
            backoff_base_ms: DEFAULT_BACKOFF_BASE_MS,
        })
    }
}

/// Everything `ship` needs to reach the bearer ingest Worker (LEG-262).
#[derive(Debug, Clone)]
pub struct BearerConfig {
    /// Worker base URL, no trailing slash, e.g.
    /// `https://legatus-telemetry-ingest.legatus-ai.workers.dev`.
    pub ingest_url: String,
    pub token: String,
    pub attempts: u32,
    pub backoff_base_ms: u64,
}

/// How `ship` reaches the lake: the bearer ingest Worker when the
/// `TELEMETRY_INGEST_*` pair is configured, direct `SigV4` otherwise.
#[derive(Debug, Clone)]
pub enum ShipTransport {
    Bearer(BearerConfig),
    SigV4(ShipConfig),
}

impl ShipTransport {
    /// Select the transport from the environment: `TELEMETRY_INGEST_URL` +
    /// `TELEMETRY_INGEST_TOKEN` → bearer worker; neither → `SigV4` via the
    /// `R2_*` vars; exactly one of the pair → loud config error.
    pub fn from_env() -> Result<Self> {
        Self::from_env_with(
            |k| std::env::var(k).ok().filter(|v| !v.is_empty()),
            ShipConfig::from_env,
        )
    }

    fn from_env_with(
        var: impl Fn(&str) -> Option<String>,
        sigv4_fallback: impl FnOnce() -> Result<ShipConfig>,
    ) -> Result<Self> {
        match (var("TELEMETRY_INGEST_URL"), var("TELEMETRY_INGEST_TOKEN")) {
            (Some(url), Some(token)) => Ok(Self::Bearer(BearerConfig {
                ingest_url: url.trim_end_matches('/').to_string(),
                token,
                attempts: DEFAULT_ATTEMPTS,
                backoff_base_ms: DEFAULT_BACKOFF_BASE_MS,
            })),
            (Some(_), None) | (None, Some(_)) => Err(anyhow::anyhow!(
                "TELEMETRY_INGEST_URL and TELEMETRY_INGEST_TOKEN must be set together \
                 (bearer transport) — set both, or neither to fall back to SigV4 R2_* creds"
            )),
            (None, None) => Ok(Self::SigV4(sigv4_fallback()?)),
        }
    }

    fn attempts(&self) -> u32 {
        match self {
            Self::Bearer(c) => c.attempts,
            Self::SigV4(c) => c.attempts,
        }
    }

    fn backoff_base_ms(&self) -> u64 {
        match self {
            Self::Bearer(c) => c.backoff_base_ms,
            Self::SigV4(c) => c.backoff_base_ms,
        }
    }
}

/// Outcome of one ship run.
#[derive(Debug, Default, Clone)]
pub struct ShipReport {
    /// Batches PUT this run.
    pub shipped: u64,
    /// Batches whose object already existed (idempotent re-ship).
    pub skipped_existing: u64,
    /// Batches left spooled after exhausting attempts.
    pub failed: u64,
    pub bytes_shipped: u64,
    /// One entry per failed batch.
    pub errors: Vec<String>,
}

/// One entry in the dry-run listing.
#[derive(Debug, Clone)]
pub struct DryRunEntry {
    pub object_key: String,
    pub compressed_bytes: u64,
    pub rows: u64,
}

/// List what a ship run would upload — no network, no deletions, no
/// credentials required.
pub fn dry_run_report(spool_dir: &Path) -> Result<Vec<DryRunEntry>> {
    Ok(spool::list_manifests(spool_dir)?
        .into_iter()
        .map(|(_, m)| DryRunEntry {
            object_key: m.object_key,
            compressed_bytes: m.compressed_bytes,
            rows: m.rows,
        })
        .collect())
}

/// Drain the spool to the bucket. Failures never abort the run (other
/// batches still get their chance) and never delete anything — the batch
/// just stays spooled for the next run.
pub async fn ship_spool(transport: &ShipTransport, spool_dir: &Path) -> Result<ShipReport> {
    let manifests = spool::list_manifests(spool_dir)?;
    let mut report = ShipReport::default();
    if manifests.is_empty() {
        return Ok(report);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("build http client")?;

    for (manifest_path, manifest) in manifests {
        match ship_one(transport, &client, spool_dir, &manifest_path, &manifest).await {
            Ok(ShipOutcome::Shipped) => {
                report.shipped += 1;
                report.bytes_shipped += manifest.compressed_bytes;
            }
            Ok(ShipOutcome::AlreadyPresent) => report.skipped_existing += 1,
            Err(err) => {
                report.failed += 1;
                report
                    .errors
                    .push(format!("{}: {err:#}", manifest.object_key));
            }
        }
    }
    Ok(report)
}

enum ShipOutcome {
    Shipped,
    AlreadyPresent,
}

async fn ship_one(
    transport: &ShipTransport,
    client: &reqwest::Client,
    spool_dir: &Path,
    manifest_path: &Path,
    manifest: &BatchManifest,
) -> Result<ShipOutcome> {
    let data_path = spool_dir.join(&manifest.data_file);

    // Crash-window recovery: data file already deleted means the PUT was
    // confirmed last run but the manifest delete didn't land (the data file
    // is only ever removed after a confirmed 2xx, data-first then manifest).
    if !data_path.exists() {
        match transport {
            // SigV4 can double-check via HEAD; if the object is NOT there,
            // that's real data loss and we say so loudly.
            ShipTransport::SigV4(cfg) => {
                if head_exists(cfg, client, &manifest.object_key).await? {
                    remove_spool_pair(&data_path, manifest_path)?;
                    return Ok(ShipOutcome::AlreadyPresent);
                }
                anyhow::bail!(
                    "manifest {} has no data file {} and the object is not in the bucket — \
                     refusing to delete the manifest (data would be lost)",
                    manifest_path.display(),
                    manifest.data_file,
                );
            }
            // The Worker has no HEAD; trust the delete-order invariant
            // (data file gone ⇒ a 200 was confirmed last run) and finish
            // the manifest cleanup.
            ShipTransport::Bearer(_) => {
                remove_spool_pair(&data_path, manifest_path)?;
                return Ok(ShipOutcome::AlreadyPresent);
            }
        }
    }

    // SigV4 idempotent re-ship: the key embeds the content hash, so
    // existence == identical bytes already in the lake. Bearer mode skips
    // this — the Worker is first-write-wins, so a re-PUT 200 is the same
    // guarantee with one round-trip.
    if let ShipTransport::SigV4(cfg) = transport {
        match head_exists(cfg, client, &manifest.object_key).await {
            Ok(true) => {
                remove_spool_pair(&data_path, manifest_path)?;
                return Ok(ShipOutcome::AlreadyPresent);
            }
            Ok(false) => {}
            // HEAD trouble is non-fatal: fall through and let PUT (with its
            // retries) be the arbiter.
            Err(_) => {}
        }
    }

    let body = fs::read(&data_path).with_context(|| format!("read {}", data_path.display()))?;

    let mut last_err: Option<anyhow::Error> = None;
    let attempts = transport.attempts().max(1);
    for attempt in 1..=attempts {
        if attempt > 1 {
            let backoff = transport
                .backoff_base_ms()
                .saturating_mul(1 << (attempt - 2));
            tokio::time::sleep(Duration::from_millis(backoff + jitter_ms(backoff / 2 + 1))).await;
        }
        let put_result = match transport {
            ShipTransport::SigV4(cfg) => {
                put_object(cfg, client, &manifest.object_key, body.clone()).await
            }
            ShipTransport::Bearer(cfg) => {
                put_bearer(cfg, client, &manifest.object_key, body.clone()).await
            }
        };
        match put_result {
            Ok(()) => {
                // Confirmed PUT — only now may the spool forget the batch.
                remove_spool_pair(&data_path, manifest_path)?;
                return Ok(ShipOutcome::Shipped);
            }
            Err(err) if err.retryable && attempt < attempts => {
                last_err = Some(err.into_anyhow());
            }
            Err(err) => {
                last_err = Some(err.into_anyhow());
                break;
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("upload failed with no recorded error"))
        .context(format!("PUT {} (left spooled)", manifest.object_key)))
}

/// PUT error carrying retry classification: transport + 5xx are retryable,
/// 4xx is not (auth/key problems don't heal by retrying).
struct PutError {
    retryable: bool,
    message: String,
}

impl PutError {
    fn into_anyhow(self) -> anyhow::Error {
        anyhow::anyhow!("{}", self.message)
    }
}

async fn put_object(
    cfg: &ShipConfig,
    client: &reqwest::Client,
    key: &str,
    body: Vec<u8>,
) -> std::result::Result<(), PutError> {
    let payload_hash = spool::sha256_hex(&body);
    let (url, request_headers, authorization) =
        signed_request_parts(cfg, "PUT", key, &payload_hash).map_err(|e| PutError {
            retryable: false,
            message: format!("{e:#}"),
        })?;

    let mut req = client.put(&url).body(body);
    for (k, v) in &request_headers {
        req = req.header(k, v);
    }
    let resp = req
        .header("authorization", &authorization)
        .send()
        .await
        .map_err(|e| PutError {
            retryable: true,
            message: format!("transport error: {e}"),
        })?;

    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body_snippet: String = resp
        .text()
        .await
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect();
    Err(PutError {
        retryable: status.is_server_error(),
        message: format!("HTTP {status}: {body_snippet}"),
    })
}

/// Bearer-worker PUT (LEG-262 contract): `PUT {ingest_url}/{key}` with
/// `Authorization: Bearer`, explicit `Content-Length`, body = the zstd
/// batch bytes. The Worker is idempotent first-write-wins, so a 200 on a
/// re-ship of the same key is success — no overwrite, no dupe.
async fn put_bearer(
    cfg: &BearerConfig,
    client: &reqwest::Client,
    key: &str,
    body: Vec<u8>,
) -> std::result::Result<(), PutError> {
    let url = format!("{}/{key}", cfg.ingest_url);
    let resp = client
        .put(&url)
        .header("authorization", format!("Bearer {}", cfg.token))
        .header("content-type", "application/zstd")
        .header("content-length", body.len())
        .body(body)
        .send()
        .await
        .map_err(|e| PutError {
            retryable: true,
            message: format!("transport error: {e}"),
        })?;

    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body_snippet: String = resp
        .text()
        .await
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect();
    Err(PutError {
        retryable: status.is_server_error(),
        message: format!("HTTP {status}: {body_snippet}"),
    })
}

async fn head_exists(cfg: &ShipConfig, client: &reqwest::Client, key: &str) -> Result<bool> {
    let (url, request_headers, authorization) =
        signed_request_parts(cfg, "HEAD", key, EMPTY_PAYLOAD_SHA256)?;
    let mut req = client.head(&url);
    for (k, v) in &request_headers {
        req = req.header(k, v);
    }
    let resp = req
        .header("authorization", &authorization)
        .send()
        .await
        .context("HEAD request")?;
    Ok(resp.status().is_success())
}

/// Build the signed pieces for a request to `key`: full URL, the headers
/// that participate in the signature, and the `Authorization` value.
fn signed_request_parts(
    cfg: &ShipConfig,
    method: &str,
    key: &str,
    payload_sha256_hex: &str,
) -> Result<(String, Vec<(String, String)>, String)> {
    let encoded_key = sigv4::uri_encode_key(key);
    let canonical_uri = format!("/{}/{encoded_key}", cfg.bucket);
    let url = format!("{}{canonical_uri}", cfg.endpoint);

    let parsed = reqwest::Url::parse(&url).with_context(|| format!("parse url {url}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("endpoint {} has no host", cfg.endpoint))?;
    // reqwest sends `Host: host:port` for non-default ports — the signed
    // value must match exactly.
    let host_header = parsed
        .port()
        .map_or_else(|| host.to_string(), |p| format!("{host}:{p}"));

    let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let headers = vec![
        ("host".to_string(), host_header),
        (
            "x-amz-content-sha256".to_string(),
            payload_sha256_hex.to_string(),
        ),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];

    let ctx = SigningContext {
        access_key_id: &cfg.access_key_id,
        secret_access_key: &cfg.secret_access_key,
        region: &cfg.region,
        service: "s3",
        amz_date: &amz_date,
    };
    let authorization = sigv4::authorization_header(
        &ctx,
        method,
        &canonical_uri,
        "",
        &headers,
        payload_sha256_hex,
    );

    // `host` is set by reqwest itself; we only send the x-amz-* pair.
    let request_headers = headers
        .into_iter()
        .filter(|(k, _)| k != "host")
        .collect::<Vec<_>>();
    Ok((url, request_headers, authorization))
}

/// Delete the spool pair, data file first (the manifest is the marker that
/// recovery code keys off, so it goes last).
fn remove_spool_pair(data_path: &Path, manifest_path: &Path) -> Result<()> {
    if data_path.exists() {
        fs::remove_file(data_path).with_context(|| format!("remove {}", data_path.display()))?;
    }
    fs::remove_file(manifest_path)
        .with_context(|| format!("remove {}", manifest_path.display()))?;
    Ok(())
}

/// Cheap jitter without a rand dependency — sub-second wall-clock nanos.
fn jitter_ms(max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    nanos % max
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::spool::{BatchSpec, SpoolConfig};
    use axum::body::Bytes;
    use axum::extract::{Path as AxPath, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::put;
    use axum::Router;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    /// In-process S3 stand-in: stores PUT bodies, answers HEAD, can inject
    /// failures. Proves wire-level behavior without a real bucket.
    #[derive(Default)]
    struct FakeS3 {
        objects: Mutex<HashMap<String, Vec<u8>>>,
        put_count: AtomicU32,
        head_count: AtomicU32,
        /// Next N PUTs answer 500.
        fail_puts_remaining: AtomicU32,
        /// All PUTs answer this status when set (e.g. 403).
        put_status_override: Mutex<Option<StatusCode>>,
    }

    impl FakeS3 {
        fn object_keys(&self) -> Vec<String> {
            let mut keys: Vec<String> = self.objects.lock().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        }
    }

    async fn put_handler(
        State(s): State<Arc<FakeS3>>,
        AxPath((bucket, key)): AxPath<(String, String)>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        assert_eq!(bucket, "telemetry-test");
        // Every request must be SigV4-signed with the content hash header.
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential="),
            "unsigned PUT: {auth}"
        );
        let declared_sha = headers
            .get("x-amz-content-sha256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(
            declared_sha,
            spool::sha256_hex(&body),
            "payload hash mismatch"
        );

        s.put_count.fetch_add(1, Ordering::SeqCst);
        let status_override = *s.put_status_override.lock().unwrap();
        if let Some(code) = status_override {
            return code;
        }
        if s.fail_puts_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
        s.objects.lock().unwrap().insert(key, body.to_vec());
        StatusCode::OK
    }

    async fn head_handler(
        State(s): State<Arc<FakeS3>>,
        AxPath((_bucket, key)): AxPath<(String, String)>,
    ) -> StatusCode {
        s.head_count.fetch_add(1, Ordering::SeqCst);
        if s.objects.lock().unwrap().contains_key(&key) {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        }
    }

    async fn spawn_fake_s3(state: Arc<FakeS3>) -> SocketAddr {
        let app = Router::new()
            .route("/{bucket}/{*key}", put(put_handler).head(head_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    fn test_config(addr: SocketAddr) -> ShipConfig {
        ShipConfig {
            endpoint: format!("http://{addr}"),
            bucket: "telemetry-test".to_string(),
            access_key_id: "test-access-key".to_string(),
            secret_access_key: "test-secret".to_string(),
            region: "auto".to_string(),
            attempts: 3,
            backoff_base_ms: 1,
        }
    }

    fn sigv4(cfg: ShipConfig) -> ShipTransport {
        ShipTransport::SigV4(cfg)
    }

    /// Spool one real batch (through the LEG-259 writer) and return its
    /// manifest.
    fn spool_batch(dir: &Path, n: u32) -> BatchManifest {
        let cfg = SpoolConfig::new(dir.to_path_buf());
        let ndjson = format!("{{\"hook\":\"h{n}\",\"schema\":\"ledger.v1\"}}\n");
        let spec = BatchSpec {
            source: "claude".to_string(),
            kind: "ledger".to_string(),
            schema: "ledger.v1".to_string(),
            key_prefix: "v1/ledger/claude/dt=2026-06-12".to_string(),
            stamp: format!("20260612T0102{n:02}Z_20260612T0102{n:02}Z"),
            rows: 1,
            first_ts: None,
            last_ts: None,
        };
        spool::write_batch(&cfg, &spec, ndjson.as_bytes()).unwrap()
    }

    fn spool_files(dir: &Path) -> usize {
        fs::read_dir(dir).map_or(0, Iterator::count)
    }

    #[tokio::test]
    async fn ship_puts_signed_objects_and_deletes_only_after_confirm() {
        let tmp = tempfile::tempdir().unwrap();
        let m1 = spool_batch(tmp.path(), 1);
        let m2 = spool_batch(tmp.path(), 2);

        let fake = Arc::new(FakeS3::default());
        let addr = spawn_fake_s3(fake.clone()).await;
        let report = ship_spool(&sigv4(test_config(addr)), tmp.path())
            .await
            .unwrap();

        assert_eq!(report.shipped, 2);
        assert_eq!(report.failed, 0);
        assert_eq!(fake.object_keys(), {
            let mut k = vec![m1.object_key.clone(), m2.object_key.clone()];
            k.sort();
            k
        });
        // Uploaded bytes are the spooled zstd bytes, verbatim.
        let stored = fake
            .objects
            .lock()
            .unwrap()
            .get(&m1.object_key)
            .cloned()
            .unwrap();
        let decompressed = zstd::decode_all(stored.as_slice()).unwrap();
        assert!(decompressed.starts_with(b"{\"hook\":\"h1\""));
        // Spool fully drained — delete happened after the confirmed PUT.
        assert_eq!(spool_files(tmp.path()), 0);
    }

    #[tokio::test]
    async fn idempotent_reship_same_hash_skips_put_and_drains_spool() {
        let tmp = tempfile::tempdir().unwrap();
        spool_batch(tmp.path(), 1);

        let fake = Arc::new(FakeS3::default());
        let addr = spawn_fake_s3(fake.clone()).await;
        let cfg = sigv4(test_config(addr));

        ship_spool(&cfg, tmp.path()).await.unwrap();
        assert_eq!(fake.put_count.load(Ordering::SeqCst), 1);

        // Same content collected again (e.g. checkpoint crash window) →
        // same hash → same key. Re-ship must not duplicate the upload.
        spool_batch(tmp.path(), 1);
        let report = ship_spool(&cfg, tmp.path()).await.unwrap();
        assert_eq!(report.skipped_existing, 1);
        assert_eq!(report.shipped, 0);
        assert_eq!(
            fake.put_count.load(Ordering::SeqCst),
            1,
            "no second PUT for an identical batch"
        );
        assert_eq!(fake.object_keys().len(), 1);
        assert_eq!(spool_files(tmp.path()), 0, "spool still drains");
    }

    #[tokio::test]
    async fn retries_on_5xx_with_backoff_then_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        spool_batch(tmp.path(), 1);

        let fake = Arc::new(FakeS3::default());
        fake.fail_puts_remaining.store(2, Ordering::SeqCst);
        let addr = spawn_fake_s3(fake.clone()).await;

        let report = ship_spool(&sigv4(test_config(addr)), tmp.path())
            .await
            .unwrap();
        assert_eq!(report.shipped, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(
            fake.put_count.load(Ordering::SeqCst),
            3,
            "two 500s then the successful third attempt"
        );
        assert_eq!(spool_files(tmp.path()), 0);
    }

    #[tokio::test]
    async fn exhausted_5xx_retries_leave_batch_spooled() {
        let tmp = tempfile::tempdir().unwrap();
        spool_batch(tmp.path(), 1);

        let fake = Arc::new(FakeS3::default());
        fake.fail_puts_remaining.store(99, Ordering::SeqCst);
        let addr = spawn_fake_s3(fake.clone()).await;

        let report = ship_spool(&sigv4(test_config(addr)), tmp.path())
            .await
            .unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(report.shipped, 0);
        assert_eq!(
            fake.put_count.load(Ordering::SeqCst),
            3,
            "exactly 3 attempts/run"
        );
        assert_eq!(
            spool_files(tmp.path()),
            2,
            "batch + manifest stay for next run"
        );
        assert!(report.errors[0].contains("HTTP 500"), "{:?}", report.errors);
    }

    #[tokio::test]
    async fn fatal_4xx_does_not_retry() {
        let tmp = tempfile::tempdir().unwrap();
        spool_batch(tmp.path(), 1);

        let fake = Arc::new(FakeS3::default());
        *fake.put_status_override.lock().unwrap() = Some(StatusCode::FORBIDDEN);
        let addr = spawn_fake_s3(fake.clone()).await;

        let report = ship_spool(&sigv4(test_config(addr)), tmp.path())
            .await
            .unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(
            fake.put_count.load(Ordering::SeqCst),
            1,
            "4xx is fatal — retrying an auth/key error is useless"
        );
        assert_eq!(spool_files(tmp.path()), 2, "nothing deleted on failure");
    }

    #[tokio::test]
    async fn endpoint_down_spool_survives_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        spool_batch(tmp.path(), 1);
        spool_batch(tmp.path(), 2);
        let before: Vec<_> = {
            let mut v: Vec<String> = fs::read_dir(tmp.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };

        // Bind-then-drop a listener to get a port with nothing behind it.
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let mut cfg = test_config(addr);
        cfg.attempts = 2;

        let report = ship_spool(&sigv4(cfg), tmp.path()).await.unwrap();
        assert_eq!(report.failed, 2);
        assert_eq!(report.shipped, 0);
        let after: Vec<_> = {
            let mut v: Vec<String> = fs::read_dir(tmp.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        assert_eq!(before, after, "spool intact through the outage");
        assert!(
            report.errors[0].contains("transport error"),
            "{:?}",
            report.errors
        );
    }

    #[tokio::test]
    async fn dry_run_lists_keys_without_network_or_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        let m = spool_batch(tmp.path(), 1);

        let entries = dry_run_report(tmp.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].object_key, m.object_key);
        assert_eq!(entries[0].rows, 1);
        assert_eq!(spool_files(tmp.path()), 2, "dry run deletes nothing");
    }

    #[tokio::test]
    async fn manifest_with_missing_data_recovers_via_head() {
        let tmp = tempfile::tempdir().unwrap();
        let m = spool_batch(tmp.path(), 1);

        let fake = Arc::new(FakeS3::default());
        let addr = spawn_fake_s3(fake.clone()).await;
        let cfg = sigv4(test_config(addr));

        // Crash window: PUT confirmed + data file removed, manifest left.
        fake.objects
            .lock()
            .unwrap()
            .insert(m.object_key.clone(), b"already-there".to_vec());
        fs::remove_file(tmp.path().join(&m.data_file)).unwrap();

        let report = ship_spool(&cfg, tmp.path()).await.unwrap();
        assert_eq!(report.skipped_existing, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(spool_files(tmp.path()), 0, "manifest cleanup completed");

        // But when the object is NOT in the bucket, the manifest is kept
        // and the loss is reported.
        let m2 = spool_batch(tmp.path(), 2);
        fs::remove_file(tmp.path().join(&m2.data_file)).unwrap();
        let report = ship_spool(&cfg, tmp.path()).await.unwrap();
        assert_eq!(report.failed, 1);
        assert!(
            report.errors[0].contains("data would be lost"),
            "{:?}",
            report.errors
        );
        assert_eq!(spool_files(tmp.path()), 1, "manifest preserved as evidence");
    }

    #[test]
    fn from_env_requires_creds_and_never_hardcodes() {
        // Run in a subprocess-free way: just verify the error names the
        // missing vars when the env is empty-ish. (Env mutation is avoided;
        // these vars are absent in the test environment.)
        if std::env::var("R2_ENDPOINT").is_ok() || std::env::var("R2_ACCOUNT_ID").is_ok() {
            return; // operator env has real creds — skip
        }
        let err = ShipConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("R2_ACCOUNT_ID"), "{err}");
    }

    // ---- bearer-worker transport (LEG-262 ingest Worker) ----

    /// In-process ingest-Worker stand-in: bearer auth, first-write-wins
    /// idempotency (re-PUT of an existing key → 200, no overwrite), PUT-only.
    #[derive(Default)]
    struct FakeWorker {
        token: String,
        objects: Mutex<HashMap<String, Vec<u8>>>,
        put_count: AtomicU32,
        /// Next N PUTs answer 500.
        fail_puts_remaining: AtomicU32,
        /// All PUTs answer this status when set (e.g. 401).
        put_status_override: Mutex<Option<StatusCode>>,
    }

    impl FakeWorker {
        fn object_keys(&self) -> Vec<String> {
            let mut keys: Vec<String> = self.objects.lock().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        }
    }

    async fn worker_put_handler(
        State(w): State<Arc<FakeWorker>>,
        AxPath(key): AxPath<String>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        w.put_count.fetch_add(1, Ordering::SeqCst);

        // The contract: bearer auth + explicit Content-Length on every PUT.
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(auth, format!("Bearer {}", w.token), "bad bearer: {auth}");
        let declared_len: usize = headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .expect("Content-Length required");
        assert_eq!(declared_len, body.len(), "Content-Length mismatch");

        let status_override = *w.put_status_override.lock().unwrap();
        if let Some(code) = status_override {
            return code;
        }
        if w.fail_puts_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
        // First write wins: an existing key is NOT overwritten, still 200.
        w.objects
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| body.to_vec());
        StatusCode::OK
    }

    async fn spawn_fake_worker(state: Arc<FakeWorker>) -> SocketAddr {
        let app = Router::new()
            .route("/{*key}", put(worker_put_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    fn bearer_transport(addr: SocketAddr) -> ShipTransport {
        ShipTransport::Bearer(BearerConfig {
            ingest_url: format!("http://{addr}"),
            token: "test-ingest-token".to_string(),
            attempts: 3,
            backoff_base_ms: 1,
        })
    }

    fn fake_worker() -> Arc<FakeWorker> {
        Arc::new(FakeWorker {
            token: "test-ingest-token".to_string(),
            ..FakeWorker::default()
        })
    }

    #[tokio::test]
    async fn bearer_puts_with_token_at_key_url_and_drains_spool() {
        let tmp = tempfile::tempdir().unwrap();
        let m1 = spool_batch(tmp.path(), 1);
        let m2 = spool_batch(tmp.path(), 2);

        let fake = fake_worker();
        let addr = spawn_fake_worker(fake.clone()).await;
        let report = ship_spool(&bearer_transport(addr), tmp.path())
            .await
            .unwrap();

        assert_eq!(report.shipped, 2);
        assert_eq!(report.failed, 0);
        // PUT URL is exactly {base}/{object_key} — axum's wildcard capture
        // hands us the object key verbatim.
        assert_eq!(fake.object_keys(), {
            let mut k = vec![m1.object_key.clone(), m2.object_key.clone()];
            k.sort();
            k
        });
        // Body is the spooled zstd bytes, verbatim.
        let stored = fake
            .objects
            .lock()
            .unwrap()
            .get(&m1.object_key)
            .cloned()
            .unwrap();
        let decompressed = zstd::decode_all(stored.as_slice()).unwrap();
        assert!(decompressed.starts_with(b"{\"hook\":\"h1\""));
        assert_eq!(spool_files(tmp.path()), 0, "spool drained after 200s");
    }

    #[tokio::test]
    async fn bearer_reship_is_idempotent_via_first_write_wins_200() {
        let tmp = tempfile::tempdir().unwrap();
        spool_batch(tmp.path(), 1);

        let fake = fake_worker();
        let addr = spawn_fake_worker(fake.clone()).await;
        let transport = bearer_transport(addr);

        ship_spool(&transport, tmp.path()).await.unwrap();
        assert_eq!(fake.put_count.load(Ordering::SeqCst), 1);

        // Same content collected again → same key. Bearer mode just PUTs
        // again; the Worker answers 200 without overwriting → shipped,
        // spool drained, still exactly one object.
        spool_batch(tmp.path(), 1);
        let report = ship_spool(&transport, tmp.path()).await.unwrap();
        assert_eq!(report.shipped, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(fake.put_count.load(Ordering::SeqCst), 2, "re-PUT, no HEAD");
        assert_eq!(fake.object_keys().len(), 1, "no dupe object");
        assert_eq!(spool_files(tmp.path()), 0);
    }

    #[tokio::test]
    async fn bearer_retries_5xx_then_leaves_batch_spooled() {
        let tmp = tempfile::tempdir().unwrap();
        spool_batch(tmp.path(), 1);

        let fake = fake_worker();
        fake.fail_puts_remaining.store(99, Ordering::SeqCst);
        let addr = spawn_fake_worker(fake.clone()).await;

        let report = ship_spool(&bearer_transport(addr), tmp.path())
            .await
            .unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(report.shipped, 0);
        assert_eq!(
            fake.put_count.load(Ordering::SeqCst),
            3,
            "exactly 3 attempts/run"
        );
        assert_eq!(
            spool_files(tmp.path()),
            2,
            "batch + manifest stay for next run"
        );
        assert!(report.errors[0].contains("HTTP 500"), "{:?}", report.errors);
    }

    #[tokio::test]
    async fn bearer_4xx_is_fatal_and_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        spool_batch(tmp.path(), 1);

        let fake = fake_worker();
        *fake.put_status_override.lock().unwrap() = Some(StatusCode::UNAUTHORIZED);
        let addr = spawn_fake_worker(fake.clone()).await;

        let report = ship_spool(&bearer_transport(addr), tmp.path())
            .await
            .unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(
            fake.put_count.load(Ordering::SeqCst),
            1,
            "4xx is fatal — a bad token does not heal by retrying"
        );
        assert_eq!(spool_files(tmp.path()), 2, "nothing deleted on failure");
        assert!(report.errors[0].contains("HTTP 401"), "{:?}", report.errors);
    }

    #[tokio::test]
    async fn bearer_crash_window_manifest_without_data_is_cleaned_up() {
        let tmp = tempfile::tempdir().unwrap();
        let m = spool_batch(tmp.path(), 1);

        let fake = fake_worker();
        let addr = spawn_fake_worker(fake.clone()).await;

        // Crash window: PUT confirmed + data file removed, manifest left.
        // (The data file is only ever deleted after a confirmed 200.)
        fs::remove_file(tmp.path().join(&m.data_file)).unwrap();
        let report = ship_spool(&bearer_transport(addr), tmp.path())
            .await
            .unwrap();
        assert_eq!(report.skipped_existing, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(fake.put_count.load(Ordering::SeqCst), 0, "no network");
        assert_eq!(spool_files(tmp.path()), 0, "manifest cleanup completed");
    }

    #[test]
    fn transport_selection_prefers_bearer_pair_and_rejects_half_config() {
        let env = |url: Option<&str>, token: Option<&str>| {
            let url = url.map(str::to_string);
            let token = token.map(str::to_string);
            move |k: &str| match k {
                "TELEMETRY_INGEST_URL" => url.clone(),
                "TELEMETRY_INGEST_TOKEN" => token.clone(),
                _ => None,
            }
        };
        let no_sigv4 = || -> Result<ShipConfig> { anyhow::bail!("sigv4 fallback consulted") };

        // Both set → bearer (trailing slash trimmed), SigV4 env never read.
        let t = ShipTransport::from_env_with(
            env(Some("https://ingest.example/"), Some("tok")),
            no_sigv4,
        )
        .unwrap();
        match t {
            ShipTransport::Bearer(c) => {
                assert_eq!(c.ingest_url, "https://ingest.example");
                assert_eq!(c.token, "tok");
                assert_eq!(c.attempts, DEFAULT_ATTEMPTS);
            }
            ShipTransport::SigV4(_) => panic!("expected bearer"),
        }

        // Half a pair → loud config error, not a silent SigV4 fallback.
        for half in [
            env(Some("https://ingest.example"), None),
            env(None, Some("tok")),
        ] {
            let err = ShipTransport::from_env_with(half, no_sigv4).unwrap_err();
            assert!(err.to_string().contains("must be set together"), "{err}");
        }

        // Neither → SigV4 fallback is consulted.
        let err = ShipTransport::from_env_with(env(None, None), no_sigv4).unwrap_err();
        assert!(err.to_string().contains("sigv4 fallback consulted"));
    }
}
