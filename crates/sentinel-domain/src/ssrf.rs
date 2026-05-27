//! SSRF protection — default-block private network ranges (M4.7,
//! task #25, `ContextForge` pattern).
//!
//! Pure-domain classification: take a hostname (or literal IP) and a
//! policy, return Ok if the target is allowed or `SsrfError` if not.
//! No I/O, no DNS resolution — callers extract the host from their
//! URL (via `reqwest::Url::host_str` etc.) and pass it in. This
//! keeps `sentinel-domain` free of HTTP/URL crate deps and lets every
//! infrastructure adapter (skills-mcp, sentinel-mcp, evidence
//! adapters from #38) gate their outbound calls through the same
//! policy without duplicating the classification table.
//!
//! # What's blocked by default
//!
//! - **IPv4**: 10/8, 172.16/12, 192.168/16 (RFC1918), 127/8
//!   (loopback), 169.254/16 (link-local — covers AWS / GCP metadata
//!   endpoints at 169.254.169.254), 0.0.0.0/8 (current network),
//!   100.64/10 (CGN / Tailscale ULA), 224.0/4 (multicast),
//!   240.0/4 (reserved/future).
//! - **IPv6**: `::1/128` (loopback), `fc00::/7` (ULA), `fe80::/10`
//!   (link-local), `::ffff:0:0/96` (IPv4-mapped, classified by the
//!   embedded v4 address), `::/128` (unspecified).
//! - **Hostname literals**: "localhost", and the cloud-metadata
//!   hostnames `metadata.google.internal`, `metadata.aws.internal`,
//!   `metadata.gke.internal` (these resolve to private IPs in their
//!   environments but a misconfigured DNS could send them anywhere
//!   — block by name as belt-and-braces).
//!
//! # What's NOT covered (yet)
//!
//! **DNS rebinding**: a hostname can resolve to a public IP at
//! lookup-time and a private IP at request-time. Closing this hole
//! requires resolving the host once, pinning the connection to that
//! IP, and validating the IP. That's an infrastructure concern
//! (custom resolver in the HTTP client) and lands as a follow-up.
//! Today's guard catches the obvious cases — literal IPs and
//! "localhost" — which is most of the real-world SSRF surface.
//!
//! # Allowlist / denylist escape hatches
//!
//! `SsrfPolicy.allow_private` flips the default-deny: when `true`,
//! private IPs are allowed (useful for legitimate local-Doppler /
//! local-Linear-cache traffic). `SsrfPolicy.allowlist` adds specific
//! hosts (literal IPs or hostnames) that bypass the deny check —
//! finer-grained than flipping the global flag. `SsrfPolicy.denylist`
//! hard-blocks specific hosts even if they'd otherwise pass (the
//! cloud-metadata hostnames sit in the default denylist via
//! `default_denylist()`).

use serde::{Deserialize, Serialize};
use std::net::IpAddr;

/// SSRF policy. Default policy denies all private network ranges
/// and the well-known cloud-metadata hostnames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SsrfPolicy {
    /// When `true`, private IP ranges are allowed. Default `false`.
    /// Set this when running against a local-network environment
    /// where private addressing is expected (e.g. dogfooding a local
    /// Doppler proxy or Linear cache server). Allowlist is finer-
    /// grained and usually preferable.
    #[serde(default)]
    pub allow_private: bool,

    /// Hosts (literal IPs or hostnames, lowercased) that bypass the
    /// private-IP check. Use this for "I have one private endpoint
    /// I trust" scenarios without flipping the global flag.
    #[serde(default)]
    pub allowlist: Vec<String>,

    /// Hosts that are hard-blocked, even if they'd otherwise pass
    /// (e.g. they resolve to a public IP). Use this to lock out
    /// known sensitive endpoints by name. Default value populated by
    /// [`default_denylist`].
    #[serde(default = "default_denylist")]
    pub denylist: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            allow_private: false,
            allowlist: Vec::new(),
            denylist: default_denylist(),
        }
    }
}

/// The canonical list of hostnames that should never be reachable
/// from a skill, even if they resolve to a public IP. Matches
/// AWS / GCP / GKE / Azure metadata-service hostnames plus the
/// "localhost" literal as a belt-and-braces against DNS misconfig.
#[must_use]
pub fn default_denylist() -> Vec<String> {
    vec![
        "localhost".to_string(),
        "metadata.google.internal".to_string(),
        "metadata.aws.internal".to_string(),
        "metadata.gke.internal".to_string(),
        "metadata.azure.com".to_string(),
        "169.254.169.254".to_string(), // AWS / GCP / Azure metadata IP literal
    ]
}

/// Reasons the guard rejected a target. The error variants carry
/// the offending host + a stable `&'static str` reason so callers
/// can produce useful deny messages without parsing the enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SsrfError {
    EmptyHost,
    DeniedHost { host: String },
    PrivateIp { host: String, reason: &'static str },
    Reserved { host: String, reason: &'static str },
}

impl std::fmt::Display for SsrfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyHost => f.write_str("SSRF guard: empty host"),
            Self::DeniedHost { host } => {
                write!(f, "SSRF guard: host '{host}' is denylisted")
            }
            Self::PrivateIp { host, reason } => {
                write!(f, "SSRF guard: host '{host}' resolves to {reason}")
            }
            Self::Reserved { host, reason } => {
                write!(
                    f,
                    "SSRF guard: host '{host}' is in reserved range — {reason}"
                )
            }
        }
    }
}

impl std::error::Error for SsrfError {}

/// Check a host (literal IP or hostname) against the policy.
/// Caller extracts the host from its URL (e.g. via
/// `reqwest::Url::host_str`) and passes it in.
///
/// # Errors
///
/// - `SsrfError::EmptyHost` when `host` is empty.
/// - `SsrfError::DeniedHost` when `host` (lowercased) is in
///   `policy.denylist`.
/// - `SsrfError::PrivateIp` / `SsrfError::Reserved` when the host
///   parses as an IP that falls in a blocked range and is neither in
///   `policy.allowlist` nor cleared by `policy.allow_private`.
pub fn check_host(host: &str, policy: &SsrfPolicy) -> Result<(), SsrfError> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return Err(SsrfError::EmptyHost);
    }
    let lowered = trimmed.to_ascii_lowercase();

    // Denylist runs FIRST and unconditionally — even an allowlisted
    // host can't bypass the denylist. Cloud-metadata hostnames stay
    // blocked regardless.
    if policy
        .denylist
        .iter()
        .any(|h| h.to_ascii_lowercase() == lowered)
    {
        return Err(SsrfError::DeniedHost {
            host: trimmed.to_string(),
        });
    }

    // Allowlist: bypass the private-IP check.
    if policy
        .allowlist
        .iter()
        .any(|h| h.to_ascii_lowercase() == lowered)
    {
        return Ok(());
    }

    // If the global escape hatch is set, skip the IP classification.
    if policy.allow_private {
        return Ok(());
    }

    // Try to parse as an IP. Hostnames pass through to the implicit
    // "looks like a public DNS name" check — we don't resolve them.
    // (DNS rebinding mitigation is documented as a follow-up.)
    let stripped = strip_ipv6_brackets(trimmed);
    if let Ok(ip) = stripped.parse::<IpAddr>() {
        if let Some(reason) = blocked_ip_reason(ip) {
            // Use PrivateIp for loopback/RFC1918 (the user's mental
            // model: "this is *my* network"), Reserved for everything
            // else IANA-reserved (link-local, multicast, ULA, CGN…).
            // The distinction is purely cosmetic — both deny the
            // request, just with a different message tag.
            // For IPv4-mapped IPv6 (::ffff:V.W.X.Y), classify the
            // wrapper by its embedded v4 — the user thinks of these
            // as private addresses, not "obscure IPv6 thing".
            let effective = match ip {
                IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
                IpAddr::V4(_) => ip,
            };
            let is_private_ip = match effective {
                IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
                IpAddr::V6(v6) => v6.is_loopback(),
            };
            if is_private_ip {
                return Err(SsrfError::PrivateIp {
                    host: trimmed.to_string(),
                    reason,
                });
            }
            return Err(SsrfError::Reserved {
                host: trimmed.to_string(),
                reason,
            });
        }
    }

    Ok(())
}

/// Classify an IP. Returns `Some(reason)` if blocked.
fn blocked_ip_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                Some("loopback (127.0.0.0/8)")
            } else if v4.is_private() {
                // RFC1918 — covers 10/8, 172.16/12, 192.168/16.
                Some("private (RFC1918)")
            } else if v4.is_link_local() {
                // 169.254/16 — covers AWS/GCP/Azure metadata IP.
                Some("link-local (169.254/16, includes cloud metadata)")
            } else if v4.octets()[0] == 0 {
                // 0.0.0.0/8 — current network / DNS-rebinding pivot.
                Some("current-network (0.0.0.0/8)")
            } else if v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 64 {
                // 100.64/10 — CGN / Tailscale ULA range.
                Some("CGN / shared address space (100.64/10)")
            } else if v4.is_multicast() {
                Some("multicast (224.0/4)")
            } else if v4.octets()[0] >= 240 {
                Some("reserved/future (240.0/4)")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                Some("loopback (::1)")
            } else if v6.is_unspecified() {
                Some("unspecified (::)")
            } else if v6.is_unicast_link_local() {
                Some("link-local (fe80::/10)")
            } else if is_unique_local_v6(v6) {
                Some("unique-local (fc00::/7)")
            } else if v6.is_multicast() {
                Some("multicast (ff00::/8)")
            } else if let Some(v4) = v6.to_ipv4_mapped() {
                // ::ffff:V.W.X.Y — re-classify via the embedded IPv4.
                blocked_ip_reason(IpAddr::V4(v4))
            } else {
                None
            }
        }
    }
}

/// `Ipv6Addr::is_unique_local` is unstable as of Rust 1.83 — check
/// the high 7 bits manually. `fc00::/7` covers `fc00::` through `fdff::`.
const fn is_unique_local_v6(v6: std::net::Ipv6Addr) -> bool {
    let segments = v6.segments();
    (segments[0] & 0xfe00) == 0xfc00
}

/// Strip `[...]` brackets from an IPv6 literal string. URL hosts
/// arrive bracketed (e.g. `[::1]`), but `IpAddr::from_str` rejects
/// the brackets.
fn strip_ipv6_brackets(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'[' && bytes[bytes.len() - 1] == b']' {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol() -> SsrfPolicy {
        SsrfPolicy::default()
    }

    #[test]
    fn empty_host_rejected() {
        assert!(matches!(check_host("", &pol()), Err(SsrfError::EmptyHost)));
        assert!(matches!(
            check_host("   ", &pol()),
            Err(SsrfError::EmptyHost)
        ));
    }

    #[test]
    fn localhost_literal_blocked_via_denylist() {
        match check_host("localhost", &pol()) {
            Err(SsrfError::DeniedHost { host }) => assert_eq!(host, "localhost"),
            other => panic!("expected DeniedHost, got {other:?}"),
        }
    }

    #[test]
    fn cloud_metadata_hosts_blocked_by_default() {
        for h in [
            "metadata.google.internal",
            "metadata.aws.internal",
            "metadata.gke.internal",
            "metadata.azure.com",
            "169.254.169.254",
        ] {
            assert!(
                matches!(check_host(h, &pol()), Err(SsrfError::DeniedHost { .. })),
                "{h} should be denylisted by default",
            );
        }
    }

    #[test]
    fn ipv4_loopback_blocked() {
        for h in ["127.0.0.1", "127.255.255.255"] {
            assert!(
                matches!(check_host(h, &pol()), Err(SsrfError::PrivateIp { .. })),
                "{h} should be loopback-blocked",
            );
        }
    }

    #[test]
    fn ipv4_rfc1918_blocked() {
        for h in ["10.0.0.1", "172.16.5.5", "172.31.255.1", "192.168.1.1"] {
            assert!(
                matches!(check_host(h, &pol()), Err(SsrfError::PrivateIp { .. })),
                "{h} should be RFC1918-blocked",
            );
        }
    }

    #[test]
    fn ipv4_link_local_blocked() {
        // 169.254/16 — covers AWS/GCP/Azure metadata IP. The literal
        // 169.254.169.254 lives in the default denylist; other
        // link-local IPs should still hit the IP classifier.
        match check_host("169.254.10.10", &pol()) {
            Err(SsrfError::Reserved { reason, .. }) => {
                assert!(reason.contains("link-local"), "got reason: {reason}");
            }
            other => panic!("expected Reserved link-local, got {other:?}"),
        }
    }

    #[test]
    fn ipv4_current_network_blocked() {
        match check_host("0.0.0.0", &pol()) {
            Err(SsrfError::Reserved { reason, .. }) => {
                assert!(reason.contains("0.0.0.0"));
            }
            other => panic!("expected Reserved current-network, got {other:?}"),
        }
    }

    #[test]
    fn ipv4_cgn_blocked() {
        match check_host("100.64.1.1", &pol()) {
            Err(SsrfError::Reserved { reason, .. }) => {
                assert!(reason.contains("CGN"));
            }
            other => panic!("expected Reserved CGN, got {other:?}"),
        }
    }

    #[test]
    fn ipv4_multicast_and_reserved_blocked() {
        assert!(matches!(
            check_host("224.0.0.1", &pol()),
            Err(SsrfError::Reserved { .. })
        ));
        assert!(matches!(
            check_host("240.0.0.1", &pol()),
            Err(SsrfError::Reserved { .. })
        ));
    }

    #[test]
    fn ipv6_loopback_blocked() {
        match check_host("::1", &pol()) {
            Err(SsrfError::PrivateIp { reason, .. }) => assert!(reason.contains("loopback")),
            other => panic!("expected PrivateIp loopback, got {other:?}"),
        }
    }

    #[test]
    fn ipv6_brackets_stripped() {
        assert!(matches!(
            check_host("[::1]", &pol()),
            Err(SsrfError::PrivateIp { .. })
        ));
    }

    #[test]
    fn ipv6_link_local_blocked() {
        match check_host("fe80::1", &pol()) {
            Err(SsrfError::Reserved { reason, .. }) => assert!(reason.contains("fe80::")),
            other => panic!("expected Reserved fe80::, got {other:?}"),
        }
    }

    #[test]
    fn ipv6_unique_local_blocked() {
        for h in ["fc00::1", "fd00::1", "fdff::1"] {
            match check_host(h, &pol()) {
                Err(SsrfError::Reserved { reason, .. }) => {
                    assert!(reason.contains("fc00::"), "got reason: {reason}");
                }
                other => panic!("{h}: expected Reserved fc00::, got {other:?}"),
            }
        }
        // 2000:: is global, must NOT be blocked.
        assert!(check_host("2001:db8::1", &pol()).is_ok());
    }

    #[test]
    fn ipv6_mapped_ipv4_loopback_blocked() {
        // ::ffff:127.0.0.1 — the IPv4-mapped form must classify by
        // the embedded v4 address.
        assert!(matches!(
            check_host("::ffff:127.0.0.1", &pol()),
            Err(SsrfError::PrivateIp { .. })
        ));
    }

    #[test]
    fn public_ipv4_allowed_by_default() {
        assert!(check_host("8.8.8.8", &pol()).is_ok());
        assert!(check_host("1.1.1.1", &pol()).is_ok());
    }

    #[test]
    fn public_ipv6_allowed_by_default() {
        assert!(check_host("2001:4860:4860::8888", &pol()).is_ok());
    }

    #[test]
    fn arbitrary_hostname_allowed_by_default_no_resolution() {
        // We don't resolve — a public-looking name passes. DNS
        // rebinding is documented as out of scope for this layer.
        assert!(check_host("api.linear.app", &pol()).is_ok());
        assert!(check_host("github.com", &pol()).is_ok());
    }

    #[test]
    fn allowlist_bypasses_private_ip_check() {
        let mut p = pol();
        p.allowlist.push("10.0.0.5".to_string());
        assert!(check_host("10.0.0.5", &p).is_ok());
        // Other RFC1918 addresses still blocked.
        assert!(matches!(
            check_host("10.0.0.6", &p),
            Err(SsrfError::PrivateIp { .. })
        ));
    }

    #[test]
    fn allowlist_does_not_bypass_denylist() {
        // localhost is in the default denylist — putting it in
        // allowlist must NOT override.
        let mut p = pol();
        p.allowlist.push("localhost".to_string());
        assert!(matches!(
            check_host("localhost", &p),
            Err(SsrfError::DeniedHost { .. })
        ));
    }

    #[test]
    fn allow_private_flag_relaxes_global_check() {
        let mut p = pol();
        p.allow_private = true;
        assert!(check_host("10.0.0.5", &p).is_ok());
        assert!(check_host("127.0.0.1", &p).is_ok());
        assert!(check_host("::1", &p).is_ok());
        // Denylist still wins.
        assert!(matches!(
            check_host("169.254.169.254", &p),
            Err(SsrfError::DeniedHost { .. })
        ));
    }

    #[test]
    fn denylist_case_insensitive() {
        // Denylist comparison must be case-insensitive — DNS is.
        assert!(matches!(
            check_host("Localhost", &pol()),
            Err(SsrfError::DeniedHost { .. })
        ));
        assert!(matches!(
            check_host("METADATA.GOOGLE.INTERNAL", &pol()),
            Err(SsrfError::DeniedHost { .. })
        ));
    }

    #[test]
    fn policy_default_includes_known_metadata_hosts() {
        let p = SsrfPolicy::default();
        assert!(p.denylist.iter().any(|h| h == "169.254.169.254"));
        assert!(p.denylist.iter().any(|h| h == "metadata.google.internal"));
    }

    #[test]
    fn policy_serde_roundtrip() {
        let p = SsrfPolicy {
            allow_private: true,
            allowlist: vec!["10.0.0.5".to_string()],
            denylist: vec!["evil.example".to_string()],
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: SsrfPolicy = serde_json::from_str(&json).unwrap();
        assert!(back.allow_private);
        assert_eq!(back.allowlist, vec!["10.0.0.5".to_string()]);
        assert_eq!(back.denylist, vec!["evil.example".to_string()]);
    }
}
