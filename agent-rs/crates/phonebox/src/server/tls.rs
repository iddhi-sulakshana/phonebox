//! The Wi-Fi port's TLS (Kestrel's `UseHttps(lanCert)`): the pinned self-signed certificate, TLS 1.2 and 1.3, ALPN
//! `http/1.1`. Handshakes run in their own tasks with a timeout, so a client that connects and never finishes one
//! does not hold up the next phone; finished connections reach axum through [`TlsListener`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;

use crate::lan_cert::LanCert;

/// Kestrel's default TLS handshake timeout.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The server side of the Wi-Fi port's TLS: exactly `cert` (no chain, no SNI choice), its P-256 key, ring's crypto.
pub fn server_config(cert: &LanCert) -> Result<Arc<rustls::ServerConfig>, rustls::Error> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    // ring explicitly: the process-wide default provider is not something this server should depend on
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pkcs8.clone())),
        )?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Accepts TCP connections, completes each TLS handshake in its own task within `handshake_timeout`, and hands the
/// finished connections to axum. Dropping it stops accepting and releases the port.
pub struct TlsListener {
    ready: tokio::sync::mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
    accept: tokio::task::JoinHandle<()>,
}

impl TlsListener {
    pub fn new(
        tcp: TcpListener,
        config: Arc<rustls::ServerConfig>,
        handshake_timeout: Duration,
    ) -> std::io::Result<TlsListener> {
        let local = tcp.local_addr()?;
        let (done, ready) = tokio::sync::mpsc::channel(64);
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        let accept = tokio::spawn(async move {
            loop {
                let (stream, addr) = match tcp.accept().await {
                    Ok(connection) => connection,
                    Err(e) => {
                        // as axum's own listener: a failed connection is skipped, anything else (out of file handles)
                        // gets a pause before the next try
                        if !is_connection_error(&e) {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                        continue;
                    }
                };
                let (acceptor, done) = (acceptor.clone(), done.clone());
                tokio::spawn(async move {
                    if let Ok(Ok(tls)) =
                        tokio::time::timeout(handshake_timeout, acceptor.accept(stream)).await
                    {
                        let _ = done.send((tls, addr)).await;
                    }
                });
            }
        });
        Ok(TlsListener {
            ready,
            local,
            accept,
        })
    }
}

fn is_connection_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        e.kind(),
        ConnectionRefused | ConnectionAborted | ConnectionReset
    )
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(connection) => connection,
            // the accept loop ended (it only does when dropped): accept nothing more
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::server::AppState;
    use crate::server::guard::tests::test_state;

    const KEY: &str = "phone-key";

    /// The phone's side: trusts exactly the certificate with this fingerprint (`AgentLink.kt` pins it).
    #[derive(Debug)]
    struct Pinned {
        fingerprint: String,
        provider: Arc<rustls::crypto::CryptoProvider>,
    }

    impl ServerCertVerifier for Pinned {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            if crate::lan_cert::fingerprint(end_entity) == self.fingerprint {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General("not the pinned certificate".into()))
            }
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    fn client(
        fingerprint: &str,
        version: &'static rustls::SupportedProtocolVersion,
    ) -> tokio_rustls::TlsConnector {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[version])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned {
                fingerprint: fingerprint.to_string(),
                provider,
            }))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        tokio_rustls::TlsConnector::from(Arc::new(config))
    }

    struct Wifi {
        _dir: tempfile::TempDir,
        state: AppState,
        addr: SocketAddr,
        fingerprint: String,
        server: tokio::task::JoinHandle<std::io::Result<()>>,
    }

    /// The Wi-Fi server on a free port, with Wi-Fi on, the phone key `phone-key` and a trackpad page.
    async fn start(handshake_timeout: Duration) -> Wifi {
        start_with(handshake_timeout, crate::server::WIFI_HEADERS_TIMEOUT).await
    }

    async fn start_with(handshake_timeout: Duration, headers_timeout: Duration) -> Wifi {
        let (dir, state) = test_state();
        state
            .cfg
            .save(
                r#"{"lan":{"enabled":true,"port":8766,"token":"phone-key","webhookToken":"hook-key"},
                    "pages":[{"enabled":true,"widgets":[{"type":"trackpad","options":{"control":true}}]}]}"#,
                None,
            )
            .unwrap();
        let cert = LanCert::from(crate::lan_cert::create("tls-test", chrono::Utc::now()));
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let listener =
            TlsListener::new(tcp, server_config(&cert).unwrap(), handshake_timeout).unwrap();
        let server = tokio::spawn(crate::server::serve_wifi_with(
            listener,
            state.clone(),
            Duration::from_secs(30),
            headers_timeout,
        ));
        Wifi {
            _dir: dir,
            state,
            addr,
            fingerprint: cert.fingerprint,
            server,
        }
    }

    async fn connect(
        wifi: &Wifi,
        version: &'static rustls::SupportedProtocolVersion,
    ) -> tokio_rustls::client::TlsStream<TcpStream> {
        let tcp = TcpStream::connect(wifi.addr).await.unwrap();
        let name = ServerName::try_from("phonebox.invalid").unwrap();
        client(&wifi.fingerprint, version)
            .connect(name, tcp)
            .await
            .unwrap()
    }

    /// Writes `request` and reads up to the end of the response head.
    async fn head<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        stream: &mut S,
        request: &str,
    ) -> String {
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while !got.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                .await
                .expect("an answer")
                .unwrap();
            assert!(
                n > 0,
                "closed before the response head: {}",
                String::from_utf8_lossy(&got)
            );
            got.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&got).into_owned()
    }

    fn get(path: &str, key: Option<&str>, close: bool) -> String {
        let auth = key
            .map(|k| format!("Authorization: Bearer {k}\r\n"))
            .unwrap_or_default();
        let connection = if close { "Connection: close\r\n" } else { "" };
        format!("GET {path} HTTP/1.1\r\nHost: 192.168.1.10:8766\r\n{auth}{connection}\r\n")
    }

    /// Reads until the server closes the connection; false when it is still open after `within`.
    async fn closed_within<S: tokio::io::AsyncRead + Unpin>(
        stream: &mut S,
        within: Duration,
    ) -> bool {
        let mut buf = [0u8; 4096];
        tokio::time::timeout(within, async {
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        })
        .await
        .is_ok()
    }

    #[tokio::test]
    async fn a_client_that_never_finishes_its_headers_is_disconnected() {
        let wifi = start_with(HANDSHAKE_TIMEOUT, Duration::from_millis(500)).await;
        let mut tls = connect(&wifi, &rustls::version::TLS13).await;
        tls.write_all(b"GET /api/state HT").await.unwrap();
        assert!(closed_within(&mut tls, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn an_idle_kept_alive_connection_is_closed() {
        let wifi = start_with(HANDSHAKE_TIMEOUT, Duration::from_millis(500)).await;
        let mut tls = connect(&wifi, &rustls::version::TLS13).await;
        let answer = head(&mut tls, &get("/api/state", Some(KEY), false)).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(closed_within(&mut tls, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn the_phone_key_opens_the_api_over_tls_1_2_and_1_3() {
        let wifi = start(HANDSHAKE_TIMEOUT).await;
        for (version, expected) in [
            (&rustls::version::TLS12, rustls::ProtocolVersion::TLSv1_2),
            (&rustls::version::TLS13, rustls::ProtocolVersion::TLSv1_3),
        ] {
            let mut tls = connect(&wifi, version).await;
            assert_eq!(tls.get_ref().1.protocol_version(), Some(expected));
            assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
            let answer = head(&mut tls, &get("/api/state", Some(KEY), true)).await;
            assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
            assert!(
                answer
                    .to_ascii_lowercase()
                    .contains("cache-control: no-store"),
                "{answer}"
            );

            let mut tls = connect(&wifi, version).await;
            let answer = head(&mut tls, &get("/api/state", None, true)).await;
            assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
        }
        // the certificate the phone pins is the one served, alone
        let tls = connect(&wifi, &rustls::version::TLS13).await;
        let chain = tls.get_ref().1.peer_certificates().unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(crate::lan_cert::fingerprint(&chain[0]), wifi.fingerprint);
    }

    #[tokio::test]
    async fn the_trackpad_upgrades_over_tls() {
        let wifi = start(HANDSHAKE_TIMEOUT).await;
        let mut tls = connect(&wifi, &rustls::version::TLS13).await;
        let request = format!(
            "GET /api/trackpad HTTP/1.1\r\nHost: 192.168.1.10:8766\r\nAuthorization: Bearer {KEY}\r\n\
             Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        let answer = head(&mut tls, &request).await;
        assert!(answer.starts_with("HTTP/1.1 101"), "{answer}");
        assert!(answer.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "{answer}");
    }

    #[tokio::test]
    async fn a_client_that_never_finishes_its_handshake_blocks_no_one() {
        let wifi = start(Duration::from_millis(400)).await;
        let mut idle = TcpStream::connect(wifi.addr).await.unwrap();
        let mut half = TcpStream::connect(wifi.addr).await.unwrap();
        half.write_all(&[0x16, 0x03, 0x01, 0x00, 0x50])
            .await
            .unwrap(); // a ClientHello header, then nothing

        let started = Instant::now();
        let mut tls = connect(&wifi, &rustls::version::TLS13).await;
        let answer = head(&mut tls, &get("/api/state", Some(KEY), true)).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "{:?}",
            started.elapsed()
        );

        // both are dropped once the handshake time is up
        for stream in [&mut idle, &mut half] {
            let mut byte = [0u8; 1];
            let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
                .await
                .expect("dropped after the handshake timeout");
            assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
        }
    }

    #[tokio::test]
    async fn stop_ends_both_servers_and_frees_both_ports() {
        let wifi = start(HANDSHAKE_TIMEOUT).await;
        let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = local.local_addr().unwrap();
        let local_server = tokio::spawn(crate::server::serve(
            local,
            wifi.state.clone(),
            Duration::from_secs(30),
        ));

        // a phone keeps its Wi-Fi connection open (keep-alive), and one sits in the middle of a handshake
        let mut tls = connect(&wifi, &rustls::version::TLS13).await;
        let answer = head(&mut tls, &get("/api/state", Some(KEY), false)).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        let _idle = TcpStream::connect(wifi.addr).await.unwrap();

        wifi.state.stop.stop();
        for (name, server) in [("Wi-Fi", wifi.server), ("local", local_server)] {
            let result = tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .unwrap_or_else(|_| panic!("the {name} server did not stop"));
            assert!(result.unwrap().is_ok(), "{name}");
        }
        TcpListener::bind(wifi.addr)
            .await
            .expect("the Wi-Fi port is free");
        TcpListener::bind(local_addr)
            .await
            .expect("the local port is free");
    }
}
