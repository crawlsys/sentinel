//! Read side of the telemetry lake (LEG-258): list + fetch the shipped NDJSON
//! ledger objects from R2 so a report can aggregate fleet-wide activity.
//!
//! This module is the **only IO** in the report path — the aggregation and
//! rendering ([`super::report`]) are pure functions, so a future Cloudflare
//! read-side service can reuse them over an R2 binding without this S3 client.
//!
//! Reuses the ship side's `R2_*` config ([`ShipConfig::from_env`]) and the
//! minimal `SigV4` signer ([`super::sigv4`]) — `authorization_header` already
//! takes a canonical query string, so the same signer covers `ListObjectsV2`
//! and object `GET` with no crypto changes. Reads are best-effort and lenient:
//! an undecodable object is skipped, never fatal.

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use std::fmt::Write as _;
use std::sync::OnceLock;

use super::ship::ShipConfig;
use super::sigv4::{self, SigningContext, EMPTY_PAYLOAD_SHA256};
use crate::hook_metrics::HookInvocation;

/// Lake layout root — every shipped ledger object lives under here as
/// `v1/ledger/<harness>/dt=YYYY-MM-DD/<name>.ndjson.zst`.
const LEDGER_PREFIX: &str = "v1/ledger/";

/// Fetch every ledger row shipped within the last `window_days` (UTC `dt=`
/// partitions), optionally narrowed to a single `harness`. `now` is injected
/// so the window is deterministic/testable.
///
/// Best-effort: a failed object download is logged to stderr and skipped so a
/// single corrupt object can't sink the whole report.
pub async fn fetch_rows(
    cfg: &ShipConfig,
    window_days: i64,
    harness: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Vec<HookInvocation>> {
    // Bound every request so a stalled connection can't hang the report
    // indefinitely (mirrors the 60s budget the ship side uses).
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build http client")?;
    let prefix = match harness {
        Some(h) => format!("{LEDGER_PREFIX}{h}/"),
        None => LEDGER_PREFIX.to_string(),
    };

    let cutoff = (now - chrono::Duration::days(window_days)).date_naive();
    let keys = list_objects(cfg, &client, &prefix).await?;
    let in_window: Vec<String> = keys
        .into_iter()
        .filter(|k| key_in_window(k, cutoff))
        .collect();

    let mut rows = Vec::new();
    for key in in_window {
        match get_object(cfg, &client, &key).await {
            Ok(bytes) => rows.extend(decode_ndjson_zst(&bytes)),
            Err(e) => eprintln!("[lake] skipping {key}: {e:#}"),
        }
    }
    Ok(rows)
}

/// Paginated `ListObjectsV2` under `prefix`, returning every object key.
async fn list_objects(
    cfg: &ShipConfig,
    client: &reqwest::Client,
    prefix: &str,
) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut params: Vec<(&str, &str)> = vec![("list-type", "2"), ("prefix", prefix)];
        if let Some(t) = &token {
            params.push(("continuation-token", t));
        }
        let canonical_query = canonical_query(&params);
        let canonical_uri = format!("/{}", cfg.bucket);
        let url = format!("{}{canonical_uri}?{canonical_query}", cfg.endpoint);
        let (request_headers, authorization) = signed_headers(
            cfg,
            "GET",
            &canonical_uri,
            &canonical_query,
            EMPTY_PAYLOAD_SHA256,
        )?;

        let mut req = client.get(&url);
        for (k, v) in &request_headers {
            req = req.header(k, v);
        }
        let resp = req
            .header("authorization", &authorization)
            .send()
            .await
            .context("ListObjectsV2 request")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let snippet: String = body.chars().take(300).collect();
            anyhow::bail!("ListObjectsV2 HTTP {status}: {snippet}");
        }
        let (page_keys, next) = parse_list_xml(&body);
        keys.extend(page_keys);
        match next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    Ok(keys)
}

/// Signed `GET` of a single object, returning its raw bytes.
async fn get_object(cfg: &ShipConfig, client: &reqwest::Client, key: &str) -> Result<Vec<u8>> {
    let encoded_key = sigv4::uri_encode_key(key);
    let canonical_uri = format!("/{}/{encoded_key}", cfg.bucket);
    let url = format!("{}{canonical_uri}", cfg.endpoint);
    let (request_headers, authorization) =
        signed_headers(cfg, "GET", &canonical_uri, "", EMPTY_PAYLOAD_SHA256)?;

    let mut req = client.get(&url);
    for (k, v) in &request_headers {
        req = req.header(k, v);
    }
    let resp = req
        .header("authorization", &authorization)
        .send()
        .await
        .context("GET object")?;
    let status = resp.status();
    if !status.is_success() {
        let snippet: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        anyhow::bail!("GET HTTP {status}: {snippet}");
    }
    Ok(resp.bytes().await.context("read object body")?.to_vec())
}

/// Build the signed request pieces (the `x-amz-*` headers reqwest must send,
/// plus the `Authorization` value). `host` is signed but reqwest sets it
/// itself, so it is dropped from the returned header list — mirrors
/// `ship::signed_request_parts`.
fn signed_headers(
    cfg: &ShipConfig,
    method: &str,
    canonical_uri: &str,
    canonical_query: &str,
    payload_sha256_hex: &str,
) -> Result<(Vec<(String, String)>, String)> {
    let url = format!("{}{canonical_uri}", cfg.endpoint);
    let parsed = reqwest::Url::parse(&url).with_context(|| format!("parse url {url}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("endpoint {} has no host", cfg.endpoint))?;
    let host_header = parsed
        .port()
        .map_or_else(|| host.to_string(), |p| format!("{host}:{p}"));

    let amz_date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
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
        canonical_uri,
        canonical_query,
        &headers,
        payload_sha256_hex,
    );
    let request_headers = headers.into_iter().filter(|(k, _)| k != "host").collect();
    Ok((request_headers, authorization))
}

/// AWS `SigV4` canonical query string: percent-encode each name and value
/// (everything outside the RFC3986 unreserved set, including `/`), sort by
/// encoded name, join `k=v` with `&`. Built once and used verbatim for both
/// the signature and the wire URL, so the two can never disagree.
fn canonical_query(params: &[(&str, &str)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (aws_uri_encode(k), aws_uri_encode(v)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode for an `SigV4` query component: keep only the unreserved set
/// `A-Z a-z 0-9 - . _ ~`; every other byte becomes `%XX` (uppercase hex).
fn aws_uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Extract object keys and the next continuation token from a `ListObjectsV2`
/// XML response. Light regex parse — the keys are a constrained charset and
/// the repo carries no XML crate; entities are minimally unescaped.
fn parse_list_xml(xml: &str) -> (Vec<String>, Option<String>) {
    static KEY_RE: OnceLock<regex::Regex> = OnceLock::new();
    static TOKEN_RE: OnceLock<regex::Regex> = OnceLock::new();
    let key_re = KEY_RE.get_or_init(|| regex::Regex::new(r"<Key>([^<]*)</Key>").unwrap());
    let token_re = TOKEN_RE.get_or_init(|| {
        regex::Regex::new(r"<NextContinuationToken>([^<]*)</NextContinuationToken>").unwrap()
    });

    let keys = key_re
        .captures_iter(xml)
        .map(|c| xml_unescape(&c[1]))
        .collect();
    let next = token_re
        .captures(xml)
        .map(|c| xml_unescape(&c[1]))
        .filter(|t| !t.is_empty());
    (keys, next)
}

fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Keep an object if its `dt=YYYY-MM-DD` partition is on/after `cutoff`. Keys
/// with no parseable `dt=` are kept (fail-open — better to over-include in a
/// report than silently drop activity).
fn key_in_window(key: &str, cutoff: NaiveDate) -> bool {
    extract_dt(key).is_none_or(|d| d >= cutoff)
}

fn extract_dt(key: &str) -> Option<NaiveDate> {
    static DT_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = DT_RE.get_or_init(|| regex::Regex::new(r"dt=(\d{4}-\d{2}-\d{2})").unwrap());
    let cap = re.captures(key)?;
    NaiveDate::parse_from_str(&cap[1], "%Y-%m-%d").ok()
}

/// Decode a `zstd`-compressed NDJSON batch into rows. Lenient: a row that
/// fails to parse (or a non-decodable blob) is skipped, never panics. The
/// `schema` field collect stamps on each line is ignored by `HookInvocation`'s
/// deserializer.
fn decode_ndjson_zst(bytes: &[u8]) -> Vec<HookInvocation> {
    let Ok(plain) = zstd::decode_all(bytes) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&plain)
        .lines()
        .filter_map(|line| serde_json::from_str::<HookInvocation>(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_query_sorts_and_encodes_slashes() {
        // prefix slashes must be %2F; params sorted by encoded name.
        let q = canonical_query(&[("prefix", "v1/ledger/claude/"), ("list-type", "2")]);
        assert_eq!(q, "list-type=2&prefix=v1%2Fledger%2Fclaude%2F");
    }

    #[test]
    fn aws_uri_encode_matches_sigv4_rules() {
        assert_eq!(aws_uri_encode("a/b c=d"), "a%2Fb%20c%3Dd");
        assert_eq!(aws_uri_encode("keep-._~AZ09"), "keep-._~AZ09");
    }

    #[test]
    fn parse_list_xml_extracts_keys_and_token() {
        let xml = r"<ListBucketResult>
            <Contents><Key>v1/ledger/claude/dt=2026-06-22/a.ndjson.zst</Key></Contents>
            <Contents><Key>v1/ledger/codex/dt=2026-06-21/b.ndjson.zst</Key></Contents>
            <IsTruncated>true</IsTruncated>
            <NextContinuationToken>tok123==</NextContinuationToken>
        </ListBucketResult>";
        let (keys, next) = parse_list_xml(xml);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "v1/ledger/claude/dt=2026-06-22/a.ndjson.zst");
        assert_eq!(next.as_deref(), Some("tok123=="));
    }

    #[test]
    fn parse_list_xml_no_token_on_last_page() {
        let xml = r"<ListBucketResult><Contents><Key>k</Key></Contents><IsTruncated>false</IsTruncated></ListBucketResult>";
        let (keys, next) = parse_list_xml(xml);
        assert_eq!(keys, vec!["k".to_string()]);
        assert_eq!(next, None);
    }

    #[test]
    fn key_window_filter_by_dt_partition() {
        let cutoff = NaiveDate::from_ymd_opt(2026, 6, 20).unwrap();
        assert!(key_in_window(
            "v1/ledger/claude/dt=2026-06-22/x.ndjson.zst",
            cutoff
        ));
        assert!(key_in_window(
            "v1/ledger/claude/dt=2026-06-20/x.ndjson.zst",
            cutoff
        ));
        assert!(!key_in_window(
            "v1/ledger/claude/dt=2026-06-19/x.ndjson.zst",
            cutoff
        ));
        // no dt -> kept (fail-open)
        assert!(key_in_window("v1/ledger/claude/weird.ndjson.zst", cutoff));
    }

    #[test]
    fn decode_ndjson_zst_round_trips_and_skips_garbage() {
        let line = r#"{"ts":"2026-06-22T00:00:00Z","event":"PreToolUse","hook":"phase_gate","duration_us":3,"outcome":"allow","client_id":"m-abc","schema":"ledger.v1"}"#;
        let ndjson = format!("{line}\nnot json\n");
        let z = zstd::encode_all(ndjson.as_bytes(), 3).unwrap();
        let rows = decode_ndjson_zst(&z);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].client_id, "m-abc");
        assert_eq!(rows[0].hook, "phase_gate");
    }

    #[test]
    fn decode_ndjson_zst_tolerates_non_zstd() {
        assert!(decode_ndjson_zst(b"plain bytes, not zstd").is_empty());
    }
}
