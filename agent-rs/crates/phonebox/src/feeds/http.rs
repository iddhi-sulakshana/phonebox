//! `FeedHttp`: outbound requests for feeds. Never to this computer (loopback), link-local, multicast or unspecified
//! addresses. Private and Tailscale addresses are allowed only for the host the user typed in Settings, and only when
//! that host is itself local: an IP address, a name without dots, or a .local / .lan / .home / .internal /
//! .home.arpa / .ts.net name. A public name whose DNS points into the local network is refused, and so is a
//! redirect from the typed host to any other host that is not public. Redirects are followed by hand, at most 5.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use reqwest::Url;

pub const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const ACCEPT: &str =
    "application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.8, */*;q=0.1";
const TOO_LARGE: &str = "the feed is larger than 4 MB";

/// One GET with these headers (sent as given, without validation).
pub struct FeedRequest {
    pub url: Url,
    pub headers: Vec<(&'static str, String)>,
}

/// An answer, its body still to be read.
pub struct FeedReply {
    pub status: u16,
    pub location: Option<String>,
    /// The raw `ETag` and `Last-Modified` headers.
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_length: Option<u64>,
    pub body: BoxStream<'static, Result<Bytes, String>>,
}

/// The two pools behind the fetches: `send(true, …)` may reach private addresses, `send(false, …)` only public ones.
pub type FeedSend =
    Arc<dyn Fn(bool, FeedRequest) -> BoxFuture<'static, Result<FeedReply, String>> + Send + Sync>;

/// What a fetch got: the body and the address it came from, or "not modified" with the validators passed through.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub body: Option<Vec<u8>>,
    pub final_uri: Url,
    pub not_modified: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// The real pools: each resolves through a filter fixed for it, so a pooled connection opened for a local host can
/// never serve a request that must stay public. No proxy, no cookies, no automatic redirects, 10 s to connect.
pub fn real_send() -> FeedSend {
    let pool = |allow_private: bool| {
        let resolver = crate::net::FilteredResolver::new(
            move |ip| allowed(ip, allow_private),
            move |host| {
                if allow_private {
                    "refused: the address is this computer or not routable".to_string()
                } else {
                    format!(
                        "refused: {host} is not a public address (for a feed on your network, use its IP address or local name)"
                    )
                }
            },
            crate::net::Pick::First,
        );
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .tcp_nodelay(true)
            .dns_resolver(Arc::new(resolver))
            .build()
            .expect("the feed client")
    };
    let (public, local) = (pool(false), pool(true));
    Arc::new(move |allow_private, req: FeedRequest| {
        let client = if allow_private {
            local.clone()
        } else {
            public.clone()
        };
        Box::pin(async move {
            let host = req.url.host_str().unwrap_or("").to_string();
            let port = req.url.port_or_known_default().unwrap_or(0);
            let mut builder = client.get(req.url);
            for (name, value) in req.headers {
                builder = builder.header(name, value);
            }
            let res = builder.send().await.map_err(|e| {
                if e.is_timeout() {
                    return "no answer in 20 s".to_string(); // the 10 s connect limit, as the fetch's own deadline reads
                }
                let text = crate::net::error_text(&e);
                if e.is_connect() {
                    // SocketsHttpHandler names the endpoint of a failed connect
                    let text = text
                        .strip_prefix("failed to lookup address information: ")
                        .unwrap_or(&text);
                    format!("{text} ({host}:{port})")
                } else {
                    text
                }
            })?;
            let header = |name: reqwest::header::HeaderName| {
                res.headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from)
            };
            let status = res.status().as_u16();
            let location = header(reqwest::header::LOCATION);
            let etag = header(reqwest::header::ETAG);
            let last_modified = header(reqwest::header::LAST_MODIFIED);
            let content_length = res.content_length();
            let body = res
                .bytes_stream()
                .map(|chunk| chunk.map_err(|e| crate::net::error_text(&e)))
                .boxed();
            Ok(FeedReply {
                status,
                location,
                etag,
                last_modified,
                content_length,
                body,
            })
        })
    })
}

/// Public addresses always; private, CGNAT/Tailscale and unique-local ones only when `allow_private`.
pub fn allowed(ip: IpAddr, allow_private: bool) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    crate::widget_feed_proxy::is_public_address(ip) || (allow_private && is_private(ip))
}

/// 10/8, 172.16/12, 192.168/16, 100.64/10 (CGNAT/Tailscale) and fc00::/7.
fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let b = v4.octets();
            b[0] == 10
                || (b[0] == 172 && (16..=31).contains(&b[1]))
                || (b[0] == 192 && b[1] == 168)
                || (b[0] == 100 && (b[1] & 0xC0) == 64)
        }
        IpAddr::V6(v6) => (v6.octets()[0] & 0xFE) == 0xFC,
    }
}

/// An IP-literal host (.NET's `IPAddress.TryParse` takes `[v6]` too).
fn literal(host: &str) -> Option<IpAddr> {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .parse()
        .ok()
}

/// A host that names the local network itself: an IP address, a single label, or a local-only suffix.
pub fn is_local_name(host: &str) -> bool {
    const SUFFIXES: [&str; 6] = [
        ".local",
        ".lan",
        ".home",
        ".internal",
        ".home.arpa",
        ".ts.net",
    ];
    let host = host.trim_end_matches('.');
    literal(host).is_some()
        || !host.contains('.')
        || SUFFIXES.iter().any(|s| {
            host.len() >= s.len()
                && host.is_char_boundary(host.len() - s.len())
                && host[host.len() - s.len()..].eq_ignore_ascii_case(s)
        })
}

/// An http(s) address without user info (`feed://` read as `https://`), or None.
pub fn parse_target(raw: Option<&str>) -> Option<Url> {
    let raw = raw
        .filter(|r| !r.trim().is_empty() && r.encode_utf16().count() <= 2048)?
        .trim();
    let raw = match raw.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("feed://") => format!("https://{}", &raw[7..]),
        _ => raw.to_string(),
    };
    let u = Url::parse(&raw).ok()?;
    (matches!(u.scheme(), "http" | "https") && u.username().is_empty() && u.password().is_none())
        .then_some(u)
}

/// `GetAsync`: the feed at `configured`, sending the validators on the first request.
pub async fn get(
    send: &FeedSend,
    configured: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<Response, String> {
    let first =
        parse_target(Some(configured)).ok_or("the address must start with http:// or https://")?;
    let first_host = first.host_str().unwrap_or("").to_string();
    let mut uri = first;
    for hop in 0.. {
        let host = uri.host_str().unwrap_or("").to_string();
        let allow_private = host.eq_ignore_ascii_case(&first_host) && is_local_name(&first_host);
        // a literal address that is not allowed is refused before any connection
        if let Some(ip) = literal(&host)
            && !allowed(ip, allow_private)
        {
            return Err(format!("refused: {host} is not a public address"));
        }
        let mut headers = vec![
            (
                "User-Agent",
                format!("PhoneBox/{}", crate::app_paths::version()),
            ),
            ("Accept", ACCEPT.to_string()),
        ];
        if hop == 0 {
            if let Some(etag) = etag {
                headers.push(("If-None-Match", etag.to_string()));
            }
            if let Some(since) = last_modified {
                headers.push(("If-Modified-Since", since.to_string()));
            }
        }
        let reply = send(
            allow_private,
            FeedRequest {
                url: uri.clone(),
                headers,
            },
        )
        .await?;
        let code = reply.status;
        if matches!(code, 301 | 302 | 303 | 307 | 308) {
            if hop >= MAX_REDIRECTS {
                return Err("too many redirects".into());
            }
            let location = reply
                .location
                .ok_or_else(|| format!("HTTP {code} without a Location"))?;
            uri = uri
                .join(location.trim())
                .ok()
                .and_then(|next| parse_target(Some(next.as_str())))
                .ok_or("redirected to an address that is not http(s)")?;
            continue;
        }
        if code == 304 {
            return Ok(Response {
                body: None,
                final_uri: uri,
                not_modified: true,
                etag: etag.map(String::from),
                last_modified: last_modified.map(String::from),
            });
        }
        if !(200..300).contains(&code) {
            return Err(format!("HTTP {code}"));
        }
        if reply.content_length.is_some_and(|n| n > MAX_BYTES as u64) {
            return Err(TOO_LARGE.into());
        }
        let mut buf = Vec::new();
        let mut body = reply.body;
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            if buf.len() + chunk.len() > MAX_BYTES {
                return Err(TOO_LARGE.into());
            }
            buf.extend_from_slice(&chunk);
        }
        return Ok(Response {
            body: Some(buf),
            final_uri: uri,
            not_modified: false,
            etag: entity_tag(reply.etag.as_deref()),
            last_modified: http_date(reply.last_modified.as_deref()),
        });
    }
    unreachable!("the redirect loop returns")
}

/// .NET's parsed `ETag` (`"v1"` or `W/"v1"`); anything else is None.
fn entity_tag(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    let (weak, tag) = match raw.strip_prefix("W/") {
        Some(tag) => (true, tag),
        None => (false, raw),
    };
    let inner = tag.strip_prefix('"')?.strip_suffix('"')?;
    if inner.contains('"') {
        return None;
    }
    Some(if weak {
        format!("W/\"{inner}\"")
    } else {
        format!("\"{inner}\"")
    })
}

/// `Last-Modified` re-written as RFC 1123 GMT; a date that does not parse is None.
fn http_date(raw: Option<&str>) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc2822(raw?.trim()).ok()?;
    Some(
        parsed
            .with_timezone(&chrono::Utc)
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string(),
    )
}

#[cfg(test)]
#[path = "http_tests.rs"]
pub(crate) mod tests;
