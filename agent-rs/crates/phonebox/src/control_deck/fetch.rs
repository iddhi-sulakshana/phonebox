//! The deck's outbound HTTP for public internet radio (the directory, the stream check, logos): a seam the tests fake,
//! and the real client that never follows a redirect by itself, uses no proxy and connects to public addresses only.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;

/// Why an exchange failed, as far as the phone's texts tell apart.
#[derive(Debug, Clone, PartialEq)]
pub enum FetchError {
    /// Every address of the host is a local one ("local address, not checked").
    NonPublic,
    /// The name did not resolve.
    HostNotFound,
    Refused,
    Unreachable,
    Timeout,
    Other(String),
}

/// One GET; headers beyond the client's own.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub url: reqwest::Url,
    pub headers: Vec<(&'static str, String)>,
}

impl FetchRequest {
    pub fn get(url: reqwest::Url) -> FetchRequest {
        FetchRequest {
            url,
            headers: vec![],
        }
    }
}

/// The answer's head, its body still to be read.
pub struct FetchResponse {
    pub status: u16,
    pub location: Option<String>,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub body: BoxStream<'static, Result<Bytes, FetchError>>,
}

pub type Fetch = Arc<
    dyn Fn(FetchRequest) -> BoxFuture<'static, Result<FetchResponse, FetchError>> + Send + Sync,
>;

/// The resolver found no public address for the host.
#[derive(Debug)]
struct NonPublicHost;

impl std::fmt::Display for NonPublicHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("local address, not checked")
    }
}

impl std::error::Error for NonPublicHost {}

/// The resolver could not resolve the host.
#[derive(Debug)]
struct HostNotFound(String);

impl std::fmt::Display for HostNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HostNotFound {}

/// Resolves a host to its public addresses only (IP-literal hosts never reach a resolver: check them before asking).
struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| {
                    Box::new(HostNotFound(e.to_string()))
                        as Box<dyn std::error::Error + Send + Sync>
                })?;
            let public: Vec<std::net::SocketAddr> = found
                .filter(|a| super::radio_directory::is_public_ip(a.ip()))
                .collect();
            if public.is_empty() {
                return Err(Box::new(NonPublicHost) as Box<dyn std::error::Error + Send + Sync>);
            }
            let addrs: reqwest::dns::Addrs = Box::new(public.into_iter());
            Ok(addrs)
        })
    }
}

/// What a reqwest error was, from its cause chain.
fn classify(e: &reqwest::Error) -> FetchError {
    if e.is_timeout() {
        return FetchError::Timeout;
    }
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(c) = cause {
        if c.is::<NonPublicHost>() {
            return FetchError::NonPublic;
        }
        if c.is::<HostNotFound>() {
            return FetchError::HostNotFound;
        }
        if let Some(io) = c.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::ConnectionRefused => return FetchError::Refused,
                std::io::ErrorKind::NetworkUnreachable | std::io::ErrorKind::HostUnreachable => {
                    return FetchError::Unreachable;
                }
                std::io::ErrorKind::TimedOut => return FetchError::Timeout,
                _ => {}
            }
        }
        cause = c.source();
    }
    FetchError::Other(e.to_string())
}

/// The real client: `connect` to connect, `timeout` per exchange, no redirects, no proxy, public addresses only,
/// `User-Agent: PhoneBox/<version>` and the given `Accept` header.
pub fn public_fetch(connect: Duration, timeout: Duration, accept: Option<&'static str>) -> Fetch {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(accept) = accept {
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static(accept),
        );
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(connect)
        .timeout(timeout)
        .user_agent(format!("PhoneBox/{}", crate::app_paths::version()))
        .default_headers(headers)
        .dns_resolver(Arc::new(PublicOnly))
        .build()
        .expect("the radio client");
    Arc::new(move |req: FetchRequest| {
        let client = client.clone();
        Box::pin(async move {
            let mut builder = client.get(req.url);
            for (name, value) in req.headers {
                builder = builder.header(name, value);
            }
            let res = builder.send().await.map_err(|e| classify(&e))?;
            let header = |name: reqwest::header::HeaderName| {
                res.headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            let location = header(reqwest::header::LOCATION);
            let content_type = header(reqwest::header::CONTENT_TYPE);
            Ok(FetchResponse {
                status: res.status().as_u16(),
                location,
                content_type,
                content_length: res.content_length(),
                body: res
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(|e| classify(&e)))
                    .boxed(),
            })
        })
    })
}

/// The body, unless it is larger than `max` bytes (by its length header or as read).
pub async fn read_bounded(res: FetchResponse, max: usize) -> Option<Vec<u8>> {
    if res.content_length.is_some_and(|length| length > max as u64) {
        return None;
    }
    let mut out = Vec::new();
    let mut body = res.body;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.ok()?;
        if out.len() + chunk.len() > max {
            return None;
        }
        out.extend_from_slice(&chunk);
    }
    Some(out)
}
