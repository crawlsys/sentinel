//! Minimal AWS Signature Version 4 signer — just enough for S3-compatible
//! `PUT`/`HEAD` against Cloudflare R2 (LEG-260).
//!
//! Deliberately not the `aws-sdk`: the uploader needs exactly one signed
//! header set on two verbs, and the repo already carries `hmac` + `sha2` +
//! `hex` + `reqwest`. Verified against the official AWS `SigV4` test vector
//! (see tests).

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

type HmacSha256 = Hmac<Sha256>;

/// Hex sha256 of an empty payload (used by `HEAD`/`GET`).
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Inputs that identify the signing context.
#[derive(Debug, Clone)]
pub struct SigningContext<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    /// e.g. `auto` for R2, `us-east-1` for AWS.
    pub region: &'a str,
    /// e.g. `s3`.
    pub service: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
}

/// Compute the `Authorization` header value for a request.
///
/// - `canonical_uri` must already be URI-encoded the way it is sent on the
///   wire (each path segment percent-encoded, `/` separators preserved).
/// - `headers` are `(lowercase-name, trimmed-value)` pairs; this function
///   sorts them. They must exactly match the headers sent.
/// - `payload_sha256_hex` is the hex sha256 of the request body.
#[must_use]
pub fn authorization_header(
    ctx: &SigningContext<'_>,
    method: &str,
    canonical_uri: &str,
    canonical_query: &str,
    headers: &[(String, String)],
    payload_sha256_hex: &str,
) -> String {
    let mut sorted: Vec<&(String, String)> = headers.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let mut canonical_headers = String::new();
    for (k, v) in &sorted {
        canonical_headers.push_str(k);
        canonical_headers.push(':');
        canonical_headers.push_str(v);
        canonical_headers.push('\n');
    }
    let signed_headers: String = sorted
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_sha256_hex}"
    );

    let date = &ctx.amz_date[..8];
    let scope = format!("{date}/{}/{}/aws4_request", ctx.region, ctx.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        ctx.amz_date,
        hex::encode(Sha256::digest(canonical_request.as_bytes())),
    );

    let k_date = hmac(format!("AWS4{}", ctx.secret_access_key).as_bytes(), date);
    let k_region = hmac(&k_date, ctx.region);
    let k_service = hmac(&k_region, ctx.service);
    let k_signing = hmac(&k_service, "aws4_request");
    let signature = hex::encode(hmac(&k_signing, &string_to_sign));

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        ctx.access_key_id,
    )
}

/// Percent-encode a key for use as an S3 object path: every byte outside
/// the unreserved set (`A–Z a–z 0–9 - . _ ~`) is `%XX`-encoded, with `/`
/// kept as the segment separator (S3 single-encode style).
#[must_use]
pub fn uri_encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for &b in key.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The official AWS `SigV4` reference vector (`GET` to IAM, from the AWS
    /// General Reference "Signature Version 4 signing process" examples).
    #[test]
    fn matches_official_aws_sigv4_test_vector() {
        let ctx = SigningContext {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "iam",
            amz_date: "20150830T123600Z",
        };
        let headers = vec![
            (
                "content-type".to_string(),
                "application/x-www-form-urlencoded; charset=utf-8".to_string(),
            ),
            ("host".to_string(), "iam.amazonaws.com".to_string()),
            ("x-amz-date".to_string(), "20150830T123600Z".to_string()),
        ];
        let auth = authorization_header(
            &ctx,
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            &headers,
            EMPTY_PAYLOAD_SHA256,
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn key_encoding_preserves_slashes_and_encodes_equals() {
        assert_eq!(
            uri_encode_key("v1/ledger/claude/dt=2026-06-12/a_b.ndjson.zst"),
            "v1/ledger/claude/dt%3D2026-06-12/a_b.ndjson.zst"
        );
    }
}
