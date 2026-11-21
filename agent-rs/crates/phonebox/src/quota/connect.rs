//! The usage requests' network: IPv4 first, each address with an equal share of the time that is left (on networks
//! where IPv6 is advertised but not routable, trying it first failed the quota checks), and the one HTTP client the
//! quota checks share (15 s, `User-Agent: PhoneBox/<version>`).

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpSocket, TcpStream};
use tokio::time::Instant;

/// Time all connect attempts of one request may use together; the request itself times out after 15 s.
pub const CONNECT_BUDGET: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The caller gave up.
    Cancelled,
    /// No address answered in time (`HttpRequestException("Could not connect")`), with the last attempt's error.
    Failed(Option<String>),
    /// The name did not resolve.
    Resolve(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Cancelled => f.write_str("The operation was canceled."),
            ConnectError::Failed(_) => f.write_str("Could not connect"),
            ConnectError::Resolve(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ConnectError {}

/// The addresses in the order to try them: IPv4 first, each family keeping its resolver order.
pub fn ipv4_first(addrs: &[IpAddr]) -> Vec<IpAddr> {
    let mut ordered = addrs.to_vec();
    ordered.sort_by_key(|a| !a.is_ipv4()); // stable: each family keeps its order
    ordered
}

/// Tries the addresses one after another, IPv4 first. Each attempt gets an equal share of the time that is left, so
/// dead addresses of one family can't use up the budget before the other family is tried. An attempt that does not
/// hand over its stream is dropped, socket and all.
pub async fn connect_in_order<C, F>(
    addrs: &[IpAddr],
    port: u16,
    budget: Duration,
    connect: C,
    cancel: impl Future<Output = ()>,
) -> Result<TcpStream, ConnectError>
where
    C: Fn(TcpSocket, SocketAddr) -> F,
    F: Future<Output = io::Result<TcpStream>>,
{
    let ordered = ipv4_first(addrs);
    let clock = Instant::now();
    tokio::pin!(cancel);
    let mut last = None;
    for (i, ip) in ordered.iter().enumerate() {
        let Some(left) = budget.checked_sub(clock.elapsed()).filter(|l| !l.is_zero()) else {
            break;
        };
        let share = left / u32::try_from(ordered.len() - i).unwrap_or(u32::MAX);
        let socket = match if ip.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        } {
            Ok(socket) => socket,
            Err(e) => {
                last = Some(e.to_string());
                continue;
            }
        };
        tokio::select! {
            biased;
            _ = &mut cancel => return Err(ConnectError::Cancelled),
            attempt = tokio::time::timeout(share, connect(socket, SocketAddr::new(*ip, port))) => match attempt {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    return Ok(stream);
                }
                Ok(Err(e)) => last = Some(e.to_string()),
                Err(_) => last = Some("The operation was canceled.".into()),
            },
        }
    }
    if cancel.as_mut().now_or_never().is_some() {
        return Err(ConnectError::Cancelled);
    }
    Err(ConnectError::Failed(last))
}

/// Resolves `host` and connects with [`connect_in_order`] within [`CONNECT_BUDGET`].
pub async fn connect_ipv4_first(
    host: &str,
    port: u16,
    cancel: impl Future<Output = ()>,
) -> Result<TcpStream, ConnectError> {
    tokio::pin!(cancel);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let resolved = tokio::select! {
        biased;
        _ = &mut cancel => return Err(ConnectError::Cancelled),
        resolved = tokio::net::lookup_host((host, port)) => resolved,
    };
    let addrs: Vec<IpAddr> = match resolved {
        Ok(found) => found.map(|a| a.ip()).collect(),
        Err(e) => return Err(ConnectError::Resolve(e.to_string())),
    };
    connect_in_order(
        &addrs,
        port,
        CONNECT_BUDGET,
        |socket, addr| socket.connect(addr),
        cancel,
    )
    .await
}

/// A GET with these headers, or a POST of `body`.
#[derive(Debug, Clone)]
pub struct UsageRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UsageReply {
    pub status: u16,
    /// The `Retry-After` header as sent.
    pub retry_after: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpFail {
    /// The request never reached the server (`HttpRequestError.ConnectionError` / `NameResolutionError`).
    Connect(String),
    Other(String),
}

/// The quota checks' HTTP: real, or a fake in tests.
pub type UsageHttp =
    Arc<dyn Fn(UsageRequest) -> BoxFuture<'static, Result<UsageReply, HttpFail>> + Send + Sync>;

/// The real client: HTTP/1.1 over [`connect_ipv4_first`], TLS (rustls, the webpki roots) when the build has it.
pub fn usage_http() -> UsageHttp {
    #[cfg(feature = "tls")]
    {
        let config = rustls::ClientConfig::builder_with_provider(tls_provider())
            .with_safe_default_protocol_versions()
            .expect("the ring provider supports the default TLS versions")
            .with_root_certificates(webpki_roots())
            .with_no_client_auth();
        http_with_tls(config)
    }
    #[cfg(not(feature = "tls"))]
    {
        wrap(Client::builder(TokioExecutor::new()).build(Ipv4FirstConnector))
    }
}

#[cfg(feature = "tls")]
pub(crate) fn tls_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[cfg(feature = "tls")]
pub(crate) fn webpki_roots() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

/// The quota client with this TLS configuration (HTTP/1.1 over [`connect_ipv4_first`], no proxy, no redirects).
#[cfg(feature = "tls")]
pub(crate) fn http_with_tls(config: rustls::ClientConfig) -> UsageHttp {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http()
        .enable_http1()
        .wrap_connector(Ipv4FirstConnector);
    wrap(Client::builder(TokioExecutor::new()).build(connector))
}

fn wrap(client: UsageClient) -> UsageHttp {
    Arc::new(move |req| {
        let client = client.clone();
        Box::pin(async move { send(&client, req).await })
    })
}

#[cfg(feature = "tls")]
type UsageClient = Client<hyper_rustls::HttpsConnector<Ipv4FirstConnector>, Full<Bytes>>;
#[cfg(not(feature = "tls"))]
type UsageClient = Client<Ipv4FirstConnector, Full<Bytes>>;

/// Why the connector gave up, with the host and port as .NET adds them (`Could not connect (host:443)`).
#[derive(Debug)]
struct ConnectFailure(String);

impl std::fmt::Display for ConnectFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConnectFailure {}

/// hyper's connector: [`connect_ipv4_first`] to the URL's host.
#[derive(Clone)]
struct Ipv4FirstConnector;

impl tower::Service<http::Uri> for Ipv4FirstConnector {
    type Response = TokioIo<TcpStream>;
    type Error = ConnectFailure;
    type Future = BoxFuture<'static, Result<Self::Response, ConnectFailure>>;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), ConnectFailure>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: http::Uri) -> Self::Future {
        Box::pin(async move {
            let host = uri.host().unwrap_or("").to_string();
            let port = uri
                .port_u16()
                .unwrap_or(if uri.scheme_str() == Some("https") {
                    443
                } else {
                    80
                });
            connect_ipv4_first(&host, port, std::future::pending())
                .await
                .map(TokioIo::new)
                .map_err(|e| ConnectFailure(format!("{e} ({host}:{port})")))
        })
    }
}

async fn send(client: &UsageClient, req: UsageRequest) -> Result<UsageReply, HttpFail> {
    if !cfg!(feature = "tls") && req.url.starts_with("https:") {
        return Err(HttpFail::Other("this build has no TLS".into()));
    }
    let builder = if req.body.is_some() {
        http::Request::post(&req.url)
    } else {
        http::Request::get(&req.url)
    };
    let mut builder = builder.header(
        http::header::USER_AGENT,
        format!("PhoneBox/{}", crate::app_paths::version()),
    );
    for (name, value) in &req.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let request = builder
        .body(Full::new(Bytes::from(req.body.unwrap_or_default())))
        .map_err(|e| HttpFail::Other(e.to_string()))?;
    let exchange = async {
        let res = client.request(request).await.map_err(client_error)?;
        let status = res.status().as_u16();
        let retry_after = res
            .headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = res
            .into_body()
            .collect()
            .await
            .map_err(|e| HttpFail::Other(e.to_string()))?
            .to_bytes();
        Ok(UsageReply {
            status,
            retry_after,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    };
    tokio::time::timeout(REQUEST_TIMEOUT, exchange).await.unwrap_or_else(|_| {
        Err(HttpFail::Other(
            "The request was canceled due to the configured HttpClient.Timeout of 15 seconds elapsing.".into(),
        ))
    })
}

/// A connect failure of ours is "offline"; anything else (TLS, the server hanging up) is the innermost error's text.
fn client_error(e: hyper_util::client::legacy::Error) -> HttpFail {
    let mut message = e.to_string();
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    while let Some(err) = source {
        if let Some(failure) = err.downcast_ref::<ConnectFailure>() {
            return HttpFail::Connect(failure.0.clone());
        }
        message = err.to_string();
        source = err.source();
    }
    HttpFail::Other(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn ipv4_addresses_are_tried_before_ipv6_but_keep_their_order() {
        // api.anthropic.com resolves to an IPv6 address first; on networks where IPv6 is advertised but not routable,
        // .NET failed with "Socket is not connected" instead of using the IPv4 address after it.
        let addrs = [
            ip("2607:6bc0::10"),
            ip("160.79.104.10"),
            ip("fe80::1"),
            ip("10.0.0.2"),
        ];
        let ordered: Vec<String> = ipv4_first(&addrs).iter().map(ToString::to_string).collect();
        assert_eq!(
            ordered,
            ["160.79.104.10", "10.0.0.2", "2607:6bc0::10", "fe80::1"]
        );
    }

    #[tokio::test]
    async fn connects_to_an_ipv4_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let mut stream = tokio::time::timeout(
            Duration::from_secs(10),
            connect_ipv4_first("127.0.0.1", port, std::future::pending()),
        )
        .await
        .unwrap()
        .unwrap();
        let mut server = accept.await.unwrap();
        stream.write_all(&[42]).await.unwrap();
        let mut buffer = [0u8; 1];
        assert_eq!(server.read(&mut buffer).await.unwrap(), 1);
        assert_eq!(buffer[0], 42);
    }

    #[tokio::test]
    async fn fails_with_an_http_error_when_nothing_listens() {
        let port = {
            let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            free.local_addr().unwrap().port()
        };
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            connect_ipv4_first("127.0.0.1", port, std::future::pending()),
        )
        .await
        .unwrap();
        assert!(
            matches!(result, Err(ConnectError::Failed(Some(_)))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn dead_addresses_leave_time_for_the_last_one() {
        // three addresses that never answer, then one that does: before, each took a fixed share of the request
        // timeout, so the working one was never reached. Every address is IPv4 so the order stays as given.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let addrs = [
            ip("10.255.255.1"),
            ip("10.255.255.2"),
            ip("10.255.255.3"),
            ip("127.0.0.1"),
        ];
        let tried = Arc::new(Mutex::new(Vec::new()));
        let budget = Duration::from_secs(4);
        let clock = Instant::now();
        let stream = connect_in_order(
            &addrs,
            port,
            budget,
            |socket, endpoint| {
                tried.lock().unwrap().push(endpoint.ip());
                async move {
                    if endpoint.ip().is_loopback() {
                        socket.connect(endpoint).await
                    } else {
                        std::future::pending().await // pending until this attempt's share of the budget ends
                    }
                }
            },
            std::future::pending(),
        )
        .await;
        assert!(stream.is_ok(), "{stream:?}");
        assert_eq!(*tried.lock().unwrap(), addrs);
        // the loop stops starting attempts once the budget is spent; the slack covers timer resolution and a busy runner
        assert!(
            clock.elapsed() <= budget + Duration::from_secs(1),
            "{:?}",
            clock.elapsed()
        );
    }

    #[tokio::test]
    async fn the_socket_is_disposed_when_the_caller_cancels() {
        let held = Arc::new(());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let started_tx = Mutex::new(Some(started_tx));
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let loopback = [IpAddr::from([127, 0, 0, 1])];
        let connecting = connect_in_order(
            &loopback,
            9,
            Duration::from_secs(30),
            |socket, _| {
                let held = held.clone();
                if let Some(tx) = started_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                async move {
                    let _keep = (socket, held);
                    std::future::pending::<io::Result<TcpStream>>().await
                }
            },
            async move {
                let _ = cancel_rx.await;
            },
        );
        let canceller = async {
            started_rx.await.unwrap();
            cancel_tx.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(connecting, canceller);
        assert_eq!(result.map(|_| ()), Err(ConnectError::Cancelled));
        assert_eq!(
            Arc::strong_count(&held),
            1,
            "the attempt, its socket with it, was dropped"
        );
    }

    /// One HTTP/1.1 exchange on a local port: the request head, then `reply`.
    async fn one_reply(reply: &'static str) -> (u16, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let head = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = s.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                buf.extend_from_slice(&chunk[..n]);
            }
            s.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8(buf).unwrap()
        });
        (port, head)
    }

    #[tokio::test]
    async fn the_usage_client_sends_its_headers_and_reads_the_reply() {
        let (port, head) =
            one_reply("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 230\r\nContent-Length: 7\r\nConnection: close\r\n\r\n{\"a\":1}")
                .await;
        let http = usage_http();
        let reply = http(UsageRequest {
            url: format!("http://127.0.0.1:{port}/api/oauth/usage"),
            headers: vec![
                ("Authorization".into(), "Bearer fake-token".into()),
                ("anthropic-beta".into(), "x".into()),
            ],
            body: None,
        })
        .await
        .unwrap();
        assert_eq!(
            (
                reply.status,
                reply.retry_after.as_deref(),
                reply.body.as_str()
            ),
            (429, Some("230"), "{\"a\":1}")
        );
        let head = head.await.unwrap().to_ascii_lowercase();
        assert!(
            head.starts_with("get /api/oauth/usage http/1.1\r\n"),
            "{head}"
        );
        for line in [
            "authorization: bearer fake-token",
            "anthropic-beta: x",
            "user-agent: phonebox/",
        ] {
            assert!(head.contains(line), "{line} in {head}");
        }
    }

    #[tokio::test]
    async fn a_closed_port_is_a_connect_failure_and_https_needs_tls() {
        let port = {
            let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            free.local_addr().unwrap().port()
        };
        let http = usage_http();
        let result = http(UsageRequest {
            url: format!("http://127.0.0.1:{port}/"),
            headers: vec![],
            body: None,
        })
        .await;
        assert_eq!(
            result.unwrap_err(),
            HttpFail::Connect(format!("Could not connect (127.0.0.1:{port})"))
        );
        if !cfg!(feature = "tls") {
            let result = http(UsageRequest {
                url: "https://127.0.0.1:1/".into(),
                headers: vec![],
                body: None,
            })
            .await;
            assert_eq!(
                result.unwrap_err(),
                HttpFail::Other("this build has no TLS".into())
            );
        }
    }
}
