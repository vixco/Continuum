//! # Web fetch (`mcp__kairo__web_fetch`)
//!
//! HTTP GET only, aggressively constrained:
//!
//! - Scheme whitelist: `http`, `https`.
//! - Host must resolve to a public IP. All private (RFC 1918), loopback,
//!   link-local, multicast, and unspecified addresses are rejected before
//!   the request is issued.
//! - Redirects are **disabled** to prevent redirect-SSRF. If a redirect is
//!   required, the orchestrator should call the target URL directly.
//! - Response body is read up to [`WEB_FETCH_MAX_BYTES`] using streaming chunks
//!   so malicious servers can't force a large download.
//! - 5 second total timeout (set on the reqwest client).
//!
//! Not intended for API interaction — this is for fetching references, docs,
//! and quick lookups. No POST/PUT/DELETE is offered by design.

use std::net::IpAddr;
use std::time::Duration;

use reqwest::{Client, StatusCode};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use url::Url;

/// Maximum bytes of response body included in the returned content.
pub const WEB_FETCH_MAX_BYTES: usize = 50 * 1024;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct WebFetchRequest {
    /// Absolute URL with `http://` or `https://` scheme.
    pub url: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WebFetchResponse {
    pub url: String,
    pub status: u16,
    pub content_type: Option<String>,
    /// Body, prefixed with a truncation marker if [`truncated`] is true.
    pub content: String,
    pub truncated: bool,
    pub bytes_read: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum WebFetchError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("scheme '{0}' not allowed — only http(s) is supported")]
    SchemeNotAllowed(String),
    #[error("URL has no host")]
    NoHost,
    #[error("DNS resolution failed: {0}")]
    DnsFailed(String),
    #[error("address {0} is private or non-routable — blocked for safety")]
    PrivateAddress(String),
    #[error("HTTP request failed: {0}")]
    HttpFailed(String),
    #[error(
        "server returned a redirect ({0}); redirects are disabled — call the new URL directly"
    )]
    Redirected(u16),
}

/// Performs a guarded HTTP GET against the given URL.
pub async fn fetch(client: &Client, url_str: &str) -> Result<WebFetchResponse, WebFetchError> {
    let url = Url::parse(url_str).map_err(|e| WebFetchError::InvalidUrl(e.to_string()))?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(WebFetchError::SchemeNotAllowed(url.scheme().to_string()));
    }

    let host = url.host_str().ok_or(WebFetchError::NoHost)?.to_string();
    let port = url
        .port_or_known_default()
        .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });

    // Resolve host (whether literal IP or DNS name) and verify every address
    // is publicly routable BEFORE dialing.
    let addrs = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::lookup_host((host.as_str(), port)),
    )
    .await
    .map_err(|_| WebFetchError::DnsFailed("resolution timed out".into()))?
    .map_err(|e| WebFetchError::DnsFailed(e.to_string()))?;

    let mut had_any = false;
    for a in addrs {
        had_any = true;
        if !is_public_ip(&a.ip()) {
            return Err(WebFetchError::PrivateAddress(a.ip().to_string()));
        }
    }
    if !had_any {
        return Err(WebFetchError::DnsFailed("no addresses resolved".into()));
    }

    let resp = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| WebFetchError::HttpFailed(e.to_string()))?;

    // We asked the client to never redirect; reqwest returns the redirect
    // response itself as a normal response. Surface 3xx explicitly to the
    // caller so it's clear they need a different URL.
    if resp.status().is_redirection() {
        return Err(WebFetchError::Redirected(resp.status().as_u16()));
    }

    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    // Stream the body, capping at WEB_FETCH_MAX_BYTES to prevent runaway downloads.
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::with_capacity(WEB_FETCH_MAX_BYTES.min(64 * 1024));
    let mut truncated = false;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| WebFetchError::HttpFailed(e.to_string()))?
    {
        if buf.len() + chunk.len() > WEB_FETCH_MAX_BYTES {
            let remaining = WEB_FETCH_MAX_BYTES - buf.len();
            buf.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk);
    }

    let bytes_read = buf.len();
    let text = String::from_utf8_lossy(&buf).into_owned();

    let content = if truncated {
        format!(
            "[truncated, showing first {shown_kb}KB (cap)]\n\n{text}",
            shown_kb = WEB_FETCH_MAX_BYTES / 1024,
        )
    } else {
        text
    };

    // Non-2xx statuses are returned as successful tool results (so the
    // orchestrator can see the error body) but with the numeric status exposed.
    let _ = StatusCode::from_u16(status); // sanity

    Ok(WebFetchResponse {
        url: url.to_string(),
        status,
        content_type,
        content,
        truncated,
        bytes_read,
    })
}

/// Returns true if the IP address is on the public Internet: not private,
/// not loopback, not link-local, not multicast, not unspecified.
pub fn is_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_private()
                && !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_multicast()
                && !v4.is_unspecified()
                && !v4.is_broadcast()
                && !is_carrier_grade_nat(v4)
                && !is_v4_benchmark(v4)
        }
        IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_multicast()
                && !v6.is_unspecified()
                && !is_v6_unique_local(v6)
                && !is_v6_link_local(v6)
        }
    }
}

fn is_carrier_grade_nat(v4: &std::net::Ipv4Addr) -> bool {
    // 100.64.0.0/10 — RFC 6598 CGN space
    let o = v4.octets();
    o[0] == 100 && (o[1] & 0b1100_0000) == 0b0100_0000
}

fn is_v4_benchmark(v4: &std::net::Ipv4Addr) -> bool {
    // 198.18.0.0/15
    let o = v4.octets();
    o[0] == 198 && (o[1] == 18 || o[1] == 19)
}

fn is_v6_unique_local(v6: &std::net::Ipv6Addr) -> bool {
    // fc00::/7
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

fn is_v6_link_local(v6: &std::net::Ipv6Addr) -> bool {
    // fe80::/10
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn rejects_private_ipv4() {
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    }

    #[test]
    fn rejects_loopback() {
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(!is_public_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn rejects_link_local() {
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));
    }

    #[test]
    fn rejects_cgnat() {
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(
            100, 127, 255, 255
        ))));
        // Just outside CGN range — publicly routable
        assert!(is_public_ip(&IpAddr::V4(Ipv4Addr::new(100, 63, 0, 1))));
        assert!(is_public_ip(&IpAddr::V4(Ipv4Addr::new(100, 128, 0, 1))));
    }

    #[test]
    fn rejects_v6_unique_local() {
        let ip = "fd12:3456:789a::1".parse::<IpAddr>().unwrap();
        assert!(!is_public_ip(&ip));
    }

    #[test]
    fn accepts_public_ipv4() {
        assert!(is_public_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(is_public_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn rejects_unspecified() {
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(!is_public_ip(&IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
    }

    #[tokio::test]
    async fn rejects_localhost_url() {
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let r = fetch(&client, "http://127.0.0.1:8080/admin").await;
        assert!(matches!(r, Err(WebFetchError::PrivateAddress(_))));
    }

    #[tokio::test]
    async fn rejects_private_ip_url() {
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let r = fetch(&client, "http://10.0.0.5/").await;
        assert!(matches!(r, Err(WebFetchError::PrivateAddress(_))));
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let r = fetch(&client, "file:///etc/passwd").await;
        assert!(matches!(r, Err(WebFetchError::SchemeNotAllowed(_))));
    }

    #[tokio::test]
    async fn rejects_invalid_url() {
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let r = fetch(&client, "not a url").await;
        assert!(matches!(r, Err(WebFetchError::InvalidUrl(_))));
    }
}
