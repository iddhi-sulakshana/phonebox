//! `WidgetFeedProxy.cs`: a strict local/USB-only HTTPS proxy for the built-in public widget feeds, for a docked phone
//! without its own internet, and `IsPublicAddress`, which keeps fetches the agent makes for the phone (feeds, cover
//! art) off the LAN and this computer.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use reqwest::Url;

pub const MAX_RESPONSE_BYTES: usize = 1_048_576;
const ALLOWED_HOSTS: [&str; 5] = [
    "api.open-meteo.com",
    "air-quality-api.open-meteo.com",
    "hacker-news.firebaseio.com",
    "api.frankfurter.dev",
    "api.coingecko.com",
];
const DEFAULT_TYPE: &str = "application/json; charset=utf-8";

/// Why the upstream exchange ended early: a transport failure (502) or a cancellation the caller did not ask for (504).
#[derive(Debug, Clone, PartialEq)]
pub enum ProxyFail {
    Io(String),
    Canceled,
}

/// The upstream answer, its body still to be read.
pub struct ProxyReply {
    pub status: u16,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub body: BoxStream<'static, Result<Bytes, ProxyFail>>,
}

/// The proxy's HTTP: real, or a fake in tests.
pub type ProxySend =
    Arc<dyn Fn(Url) -> BoxFuture<'static, Result<ProxyReply, ProxyFail>> + Send + Sync>;

/// What `/api/proxy` answers.
#[derive(Debug, Clone, PartialEq)]
pub enum ProxyOutcome {
    /// 400 `{"error":"Disallowed or invalid feed URL"}`.
    Disallowed,
    /// 502 or 504, with no body and no content type.
    Status(u16),
    /// The upstream status, content type and body.
    Body {
        status: u16,
        content_type: String,
        body: Vec<u8>,
    },
}

/// The real client: public addresses only (each tried in turn, IPv4 first), no proxy, no cookies, no redirects,
/// 8 s to connect, no User-Agent.
pub fn real_send() -> ProxySend {
    let resolver = crate::net::FilteredResolver::new(
        is_public_address,
        |_| "Refused non-public address for feed host".to_string(),
        crate::net::Pick::Ipv4First,
    );
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(8))
        .tcp_nodelay(true)
        .dns_resolver(Arc::new(resolver))
        .build()
        .expect("the widget feed client");
    Arc::new(move |url: Url| {
        let client = client.clone();
        Box::pin(async move {
            let res = client.get(url).send().await.map_err(proxy_fail)?;
            let content_type = res
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            Ok(ProxyReply {
                status: res.status().as_u16(),
                content_type,
                content_length: res.content_length(),
                body: res
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(proxy_fail))
                    .boxed(),
            })
        })
    })
}

/// A connect timeout is a 504 like the exchange's own deadline; any other failure is a 502.
fn proxy_fail(e: reqwest::Error) -> ProxyFail {
    if e.is_timeout() {
        ProxyFail::Canceled
    } else {
        ProxyFail::Io(crate::net::error_text(&e))
    }
}

/// An `https` address on the default port, without user info, to one of the five widget hosts.
pub fn try_validate_target(raw: Option<&str>) -> Option<Url> {
    let raw = raw.filter(|r| !r.trim().is_empty() && r.encode_utf16().count() <= 2048)?;
    let u = Url::parse(raw).ok()?;
    let host = u.host_str()?;
    (u.scheme() == "https"
        && u.port().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && ALLOWED_HOSTS.iter().any(|h| h.eq_ignore_ascii_case(host)))
    .then_some(u)
}

/// One proxied GET: 10 s for connect, headers and body; a redirect or a body over 1 MiB is a 502.
pub async fn fetch(raw: Option<&str>, send: &ProxySend) -> ProxyOutcome {
    let Some(target) = try_validate_target(raw) else {
        return ProxyOutcome::Disallowed;
    };
    let exchange = async {
        let reply = send(target).await?;
        if (300..400).contains(&reply.status) {
            return Ok(ProxyOutcome::Status(502));
        }
        if reply
            .content_length
            .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
        {
            return Ok(ProxyOutcome::Status(502));
        }
        let mut body = reply.body;
        let mut buf = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Ok(ProxyOutcome::Status(502));
            }
            buf.extend_from_slice(&chunk);
        }
        let content_type = reply
            .content_type
            .as_deref()
            .and_then(media_type)
            .unwrap_or_else(|| DEFAULT_TYPE.into());
        Ok(ProxyOutcome::Body {
            status: reply.status,
            content_type,
            body: buf,
        })
    };
    // the caller's cancellation is this future being dropped; a timeout or an upstream cancellation is a 504
    match tokio::time::timeout(Duration::from_secs(10), exchange).await {
        Err(_) | Ok(Err(ProxyFail::Canceled)) => ProxyOutcome::Status(504),
        Ok(Err(ProxyFail::Io(_))) => ProxyOutcome::Status(502),
        Ok(Ok(outcome)) => outcome,
    }
}

/// .NET's `MediaTypeHeaderValue.ToString()`: parameters separated by "; "; None when it is not a media type.
fn media_type(raw: &str) -> Option<String> {
    let mut parts = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, c) in raw.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                parts.push(raw[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(raw[start..].trim());
    let token = |t: &str| {
        !t.is_empty()
            && t.bytes()
                .all(|b| b.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?={}".contains(&b))
    };
    let (kind, sub) = parts[0].split_once('/')?;
    if !token(kind) || !token(sub) {
        return None;
    }
    let params: Vec<&str> = parts[1..]
        .iter()
        .copied()
        .filter(|p| !p.is_empty())
        .collect();
    if params.iter().any(|p| {
        !p.split_once('=')
            .is_some_and(|(name, _)| token(name.trim()))
    }) {
        return None;
    }
    Some(
        std::iter::once(parts[0])
            .chain(params)
            .collect::<Vec<_>>()
            .join("; "),
    )
}

pub fn is_public_address(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            let b = v4.octets();
            !(matches!(b[0], 0 | 10 | 127)
                || (b[0] == 100 && (b[1] & 0xC0) == 64) // 100.64.0.0/10 CGNAT
                || (b[0] == 169 && b[1] == 254) // link-local
                || (b[0] == 172 && (16..=31).contains(&b[1]))
                || (b[0] == 192 && b[1] == 168)
                || (b[0] == 192 && b[1] == 0 && matches!(b[2], 0 | 2)) // IETF / TEST-NET-1
                || (b[0] == 198 && (matches!(b[1], 18 | 19) || (b[1] == 51 && b[2] == 100))) // benchmark / TEST-NET-2
                || (b[0] == 203 && b[1] == 0 && b[2] == 113) // TEST-NET-3
                || b[0] >= 224) // multicast and reserved
        }
        IpAddr::V6(v6) => {
            let b = v6.octets();
            // global unicast 2000::/3 only: excludes ::, ::1, link- and site-local, multicast, NAT64, fc00::/7
            if (b[0] & 0xE0) != 0x20 {
                return false;
            }
            if b[0] == 0x20 && b[1] == 0x01 {
                if b[2] == 0 && b[3] == 0 {
                    return false; // 2001::/32 Teredo
                }
                if b[2] == 0 && b[3] == 0x02 && b[4] == 0 && b[5] == 0 {
                    return false; // 2001:2::/48 benchmarking
                }
                if b[2] == 0x0D && b[3] == 0xB8 {
                    return false; // 2001:db8::/32 documentation
                }
            }
            if b[0] == 0x3F && b[1] == 0xFF && (b[2] & 0xF0) == 0 {
                return false; // 3fff::/20 documentation
            }
            if b[0] == 0x20 && b[1] == 0x02 {
                // 2002::/16 6to4 must wrap a public IPv4
                return is_public_address(IpAddr::V4(std::net::Ipv4Addr::new(
                    b[2], b[3], b[4], b[5],
                )));
            }
            true
        }
    }
}

#[cfg(test)]
#[path = "widget_feed_proxy_tests.rs"]
mod fetch_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses_exclude_loopback_private_link_local_and_mapped() {
        let public = [
            "1.1.1.1",
            "8.8.8.8",
            "100.128.0.1",
            "172.32.0.1",
            "198.51.99.1",
            "2606:4700:4700::1111",
            "2002:0808:0808::1",
        ];
        let not_public = [
            "0.1.2.3",
            "10.0.0.1",
            "127.0.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "169.254.1.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.5",
            "192.0.0.8",
            "192.0.2.1",
            "198.18.0.1",
            "198.19.0.1",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:192.168.1.5",
            "::ffff:127.0.0.1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "fc00::1",
            "fd12::1",
            "64:ff9b::1.1.1.1",
            "2001:0:1::1",
            "2001:2:0::1",
            "2001:db8::1",
            "3fff:0fff::1",
            "2002:c0a8:0105::1",
        ];
        for a in public {
            assert!(is_public_address(a.parse().unwrap()), "{a} is public");
        }
        for a in not_public {
            assert!(!is_public_address(a.parse().unwrap()), "{a} is not public");
        }
        assert!(
            is_public_address("::ffff:1.1.1.1".parse().unwrap()),
            "a mapped public IPv4"
        );
    }

    #[test]
    fn is_public_address_rejects_loopback_private_and_link_local() {
        for (address, expected) in [
            ("127.0.0.1", false),
            ("10.0.0.1", false),
            ("172.16.0.5", false),
            ("192.168.1.10", false),
            ("169.254.169.254", false),
            ("100.64.0.1", false),
            ("::1", false),
            ("fe80::1", false),
            ("fd00::1", false),
            ("::ffff:127.0.0.1", false),
            ("::127.0.0.1", false),
            ("::ffff:0:127.0.0.1", false),
            ("64:ff9b::127.0.0.1", false),
            ("64:ff9b:1::192.168.1.1", false),
            ("2002:7f00:1::1", false),
            ("2002:0101:0101::1", true),
            ("2001::1", false),
            ("2001:db8::1", false),
            ("2001:2::1", false),
            ("100::1", false),
            ("1.1.1.1", true),
            ("2606:4700:4700::1111", true),
        ] {
            assert_eq!(
                is_public_address(address.parse().unwrap()),
                expected,
                "{address}"
            );
        }
    }

    // StoreFollowUpTests (feed proxy addresses)
    #[test]
    fn documentation_addresses_are_not_public() {
        for (address, expected) in [
            ("3fff::1", false), // 3fff::/20 documentation
            ("3fff:0fff:ffff::1", false),
            ("3fff:1000::1", true), // just past the documentation block
            ("2001:db8::1", false),
            ("2606:4700::1111", true),
        ] {
            assert_eq!(
                is_public_address(address.parse().unwrap()),
                expected,
                "{address}"
            );
        }
    }
}
