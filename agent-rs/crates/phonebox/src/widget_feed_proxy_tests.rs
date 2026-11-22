//! `WidgetFeedProxyTests` (the fetch half) and `WidgetFeedProxyTransportTests`: a fake sender and a body whose reads,
//! failures and drop are observed stand in for C#'s handler and stream.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures_util::FutureExt;

use super::*;

const URL: &str = "https://api.open-meteo.com/v1/forecast";

#[test]
fn try_validate_target_enforces_https_default_port_and_allowlist() {
    for (url, want) in [
        (
            "https://api.open-meteo.com/v1/forecast?latitude=59.4&longitude=24.7",
            true,
        ),
        (
            "https://air-quality-api.open-meteo.com/v1/air-quality?latitude=1&longitude=2",
            true,
        ),
        (
            "https://hacker-news.firebaseio.com/v0/topstories.json",
            true,
        ),
        ("https://api.frankfurter.dev/v1/latest", true),
        ("https://api.coingecko.com/api/v3/coins/markets", true),
        ("http://api.open-meteo.com/v1/forecast", false),
        ("https://api.open-meteo.com:8443/v1/forecast", false),
        ("https://user:pass@api.open-meteo.com/v1/forecast", false),
        ("https://127.0.0.1/latest", false),
        ("https://169.254.169.254/latest/meta-data/", false),
        ("https://example.com/feed.json", false),
        ("", false),
    ] {
        assert_eq!(try_validate_target(Some(url)).is_some(), want, "{url}");
    }
    assert!(try_validate_target(Some("https://API.Open-Meteo.com:443/v1")).is_some());
    assert!(try_validate_target(Some("https://api.open-meteo.com./v1")).is_none());
    assert!(try_validate_target(None).is_none());
}

/// A body of `size` bytes read 16 KiB at a time; it can fail after `fail_after` bytes or pause on its first read.
#[derive(Clone, Default)]
struct Probe {
    reads: Arc<AtomicUsize>,
    disposed: Arc<AtomicBool>,
    entered: Arc<tokio::sync::Notify>,
}

struct Guard(Arc<AtomicBool>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn body(
    size: usize,
    fail_after: Option<usize>,
    pause: bool,
    probe: &Probe,
) -> BoxStream<'static, Result<Bytes, ProxyFail>> {
    let p = probe.clone();
    let guard = Guard(probe.disposed.clone());
    futures_util::stream::unfold((0usize, guard), move |(offset, guard)| {
        let p = p.clone();
        async move {
            p.reads.fetch_add(1, Ordering::SeqCst);
            if pause {
                p.entered.notify_one();
                futures_util::future::pending::<()>().await;
            }
            if fail_after.is_some_and(|limit| offset >= limit) {
                return Some((
                    Err(ProxyFail::Io("owned peer ended mid-body".into())),
                    (offset, guard),
                ));
            }
            let mut count = (16 * 1024).min(size - offset);
            if let Some(end) = fail_after {
                count = count.min(end - offset);
            }
            if count == 0 {
                return None;
            }
            Some((Ok(Bytes::from(vec![b'x'; count])), (offset + count, guard)))
        }
    })
    .boxed()
}

fn sender(
    status: u16,
    declared: Option<u64>,
    make: impl Fn() -> BoxStream<'static, Result<Bytes, ProxyFail>> + Send + Sync + 'static,
) -> ProxySend {
    let make = Arc::new(make);
    Arc::new(move |_url| {
        let make = make.clone();
        async move {
            Ok(ProxyReply {
                status,
                content_type: Some("application/json".into()),
                content_length: declared,
                body: make(),
            })
        }
        .boxed()
    })
}

fn length(outcome: &ProxyOutcome) -> (u16, usize) {
    match outcome {
        ProxyOutcome::Disallowed => (400, 0),
        ProxyOutcome::Status(code) => (*code, 0),
        ProxyOutcome::Body { status, body, .. } => (*status, body.len()),
    }
}

#[tokio::test]
async fn fetch_async_blocks_redirects_and_oversized_responses() {
    let probe = Probe::default();
    let redirect = sender(302, None, || futures_util::stream::empty().boxed());
    assert_eq!(fetch(Some(URL), &redirect).await, ProxyOutcome::Status(502));
    let p = probe.clone();
    let big = sender(200, None, move || {
        body(MAX_RESPONSE_BYTES + 1, None, false, &p)
    });
    assert_eq!(fetch(Some(URL), &big).await, ProxyOutcome::Status(502));
    assert_eq!(
        fetch(Some("https://example.com/secret"), &big).await,
        ProxyOutcome::Disallowed
    );
    let ok = sender(200, None, || {
        futures_util::stream::iter([Ok(Bytes::from_static(br#"{"ok":true}"#))]).boxed()
    });
    assert_eq!(
        fetch(Some("https://api.frankfurter.dev/v1/latest"), &ok).await,
        ProxyOutcome::Body {
            status: 200,
            content_type: "application/json".into(),
            body: br#"{"ok":true}"#.to_vec()
        }
    );
}

#[tokio::test]
async fn body_connection_failures_become_bad_gateway() {
    for fail_after in [0, 4] {
        let probe = Probe::default();
        let p = probe.clone();
        let send = sender(200, None, move || body(32, Some(fail_after), false, &p));
        assert_eq!(
            fetch(Some(URL), &send).await,
            ProxyOutcome::Status(502),
            "{fail_after}"
        );
        assert!(probe.disposed.load(Ordering::SeqCst));
    }
}

/// C# had two rows (how the stream reports the cancellation); in Rust the caller's cancellation is the request future
/// being dropped, which both rows become.
#[tokio::test]
async fn caller_cancellation_remains_cancellation() {
    let probe = Probe::default();
    let p = probe.clone();
    let send = sender(200, None, move || body(16, None, true, &p));
    let pending = tokio::spawn(async move { fetch(Some(URL), &send).await });
    tokio::time::timeout(Duration::from_secs(5), probe.entered.notified())
        .await
        .unwrap();
    pending.abort();
    let ended = pending.await;
    assert!(ended.unwrap_err().is_cancelled()); // no status at all
    assert!(probe.disposed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn unknown_length_bodies_respect_the_decoded_byte_limit() {
    for (size, code, len) in [
        (0, 200, 0),
        (MAX_RESPONSE_BYTES - 1, 200, MAX_RESPONSE_BYTES - 1),
        (MAX_RESPONSE_BYTES, 200, MAX_RESPONSE_BYTES),
        (MAX_RESPONSE_BYTES + 1, 502, 0),
        (MAX_RESPONSE_BYTES * 2, 502, 0),
    ] {
        let probe = Probe::default();
        let p = probe.clone();
        let send = sender(200, None, move || body(size, None, false, &p));
        assert_eq!(
            length(&fetch(Some(URL), &send).await),
            (code, len),
            "{size}"
        );
        assert!(probe.disposed.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn a_declared_oversize_body_is_rejected_before_reading() {
    let probe = Probe::default();
    let p = probe.clone();
    let send = sender(200, Some(MAX_RESPONSE_BYTES as u64 + 1), move || {
        body(16, None, false, &p)
    });
    assert_eq!(fetch(Some(URL), &send).await, ProxyOutcome::Status(502));
    assert_eq!(probe.reads.load(Ordering::SeqCst), 0);
    assert!(probe.disposed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn upstream_cancellation_without_caller_cancellation_stays_gateway_timeout() {
    let failing = |fail: ProxyFail| -> ProxySend {
        Arc::new(move |_| {
            let fail = fail.clone();
            async move { Err(fail) }.boxed()
        })
    };
    assert_eq!(
        fetch(Some(URL), &failing(ProxyFail::Canceled)).await,
        ProxyOutcome::Status(504)
    );
    assert_eq!(
        fetch(
            Some(URL),
            &failing(ProxyFail::Io("owned connection failed".into()))
        )
        .await,
        ProxyOutcome::Status(502)
    );
}

#[tokio::test]
async fn an_already_canceled_caller_does_not_read_the_body() {
    let probe = Probe::default();
    let p = probe.clone();
    let send = sender(200, None, move || body(16, None, false, &p));
    let pending = tokio::spawn(async move { fetch(Some(URL), &send).await });
    pending.abort(); // before it ever ran
    assert!(pending.await.unwrap_err().is_cancelled());
    assert_eq!(probe.reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_complete_upstream_error_body_retains_its_status() {
    let probe = Probe::default();
    let p = probe.clone();
    let send = sender(503, None, move || body(9, None, false, &p));
    assert_eq!(length(&fetch(Some(URL), &send).await), (503, 9));
    assert!(probe.disposed.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn the_whole_exchange_has_ten_seconds() {
    let send: ProxySend = Arc::new(|_| futures_util::future::pending().boxed());
    assert_eq!(fetch(Some(URL), &send).await, ProxyOutcome::Status(504));
}

#[test]
fn the_upstream_content_type_is_rewritten_as_dotnet_does() {
    assert_eq!(
        media_type("application/json;charset=utf-8").as_deref(),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(
        media_type(" text/plain ;  a=1;b=\"x;y\" ").as_deref(),
        Some("text/plain; a=1; b=\"x;y\"")
    );
    assert_eq!(media_type("nonsense"), None);
    assert_eq!(media_type(""), None);
}
