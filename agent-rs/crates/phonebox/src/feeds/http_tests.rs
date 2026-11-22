//! `FeedTests.cs` class `FeedHttpTests`: a fake sender stands in for C#'s `HttpMessageHandler`.

use std::sync::Mutex;

use futures_util::FutureExt;

use super::*;

type Seen = Arc<Mutex<Vec<(bool, Url, Vec<(&'static str, String)>)>>>;

pub(crate) fn reply(status: u16) -> FeedReply {
    FeedReply {
        status,
        location: None,
        etag: None,
        last_modified: None,
        content_length: None,
        body: futures_util::stream::empty().boxed(),
    }
}

pub(crate) fn ok(body: &str) -> FeedReply {
    let bytes = Bytes::from(body.to_string());
    FeedReply {
        content_length: Some(bytes.len() as u64),
        body: futures_util::stream::iter([Ok(bytes)]).boxed(),
        ..reply(200)
    }
}

pub(crate) fn redirect(to: &str) -> FeedReply {
    FeedReply {
        location: Some(to.into()),
        ..reply(302)
    }
}

/// A sender that records every request and answers with `respond`.
pub(crate) fn fake(
    respond: impl Fn(&FeedRequest) -> FeedReply + Send + Sync + 'static,
) -> (FeedSend, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let send: FeedSend = Arc::new(move |private, req: FeedRequest| {
        let answer = respond(&req);
        log.lock().unwrap().push((private, req.url, req.headers));
        async move { Ok(answer) }.boxed()
    });
    (send, seen)
}

fn hosts(seen: &Seen) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|(_, u, _)| u.host_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn address_rules() {
    for (ip, allow_private, want) in [
        ("127.0.0.1", false, false),
        ("127.0.0.1", true, false),
        ("::1", true, false),
        ("169.254.169.254", true, false),
        ("0.0.0.0", true, false),
        ("192.168.1.10", false, false),
        ("192.168.1.10", true, true),
        ("10.0.0.5", true, true),
        ("100.101.102.103", true, true),
        ("fd00::1", true, true),
        ("93.184.216.34", false, true),
        ("::ffff:127.0.0.1", true, false),
    ] {
        assert_eq!(
            allowed(ip.parse().unwrap(), allow_private),
            want,
            "{ip} {allow_private}"
        );
    }
}

#[test]
fn only_http_addresses_without_user_info() {
    for (raw, want) in [
        ("https://example.com/feed", true),
        ("feed://example.com/feed", true),
        ("http://example.com/feed", true),
        ("ftp://example.com/feed", false),
        ("file:///etc/passwd", false),
        ("https://user:pass@example.com/feed", false),
        ("example.com/feed", false),
    ] {
        assert_eq!(parse_target(Some(raw)).is_some(), want, "{raw}");
    }
    assert_eq!(
        parse_target(Some("FEED://example.com/x")).unwrap().as_str(),
        "https://example.com/x"
    );
    assert!(parse_target(Some(&format!("https://example.com/{}", "a".repeat(2048)))).is_none());
    assert!(parse_target(Some("   ")).is_none());
    assert!(parse_target(None).is_none());
}

#[tokio::test]
async fn the_real_handler_never_connects_to_this_computer() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let send = real_send();
    let err = get(&send, &format!("http://127.0.0.1:{port}/feed"), None, None)
        .await
        .unwrap_err();
    assert!(err.contains("refused"), "{err}");
    // and the pool itself, for a name that resolves to this computer (the resolver, not the literal check)
    let url = Url::parse(&format!("http://localhost:{port}/feed")).unwrap();
    let err = match send(
        true,
        FeedRequest {
            url,
            headers: Vec::new(),
        },
    )
    .await
    {
        Err(e) => e,
        Ok(r) => panic!("connected: HTTP {}", r.status),
    };
    assert!(err.contains("refused"), "{err}");
    assert!(err.ends_with(&format!("(localhost:{port})")), "{err}");
    // nothing reached the socket
    assert_eq!(
        listener.accept().map(|_| ()).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn a_redirect_to_the_agent_is_refused_before_any_request() {
    let (send, seen) = fake(|r| {
        if r.url.host_str() == Some("feeds.example") {
            redirect("http://127.0.0.1:8765/api/settings")
        } else {
            reply(200)
        }
    });
    let err = get(&send, "https://feeds.example/rss", None, None)
        .await
        .unwrap_err();
    assert!(err.contains("refused"), "{err}");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn redirects_are_followed_and_capped() {
    let hops = Arc::new(Mutex::new(0));
    let counter = hops.clone();
    let (send, _) = fake(move |_| {
        let mut n = counter.lock().unwrap();
        *n += 1;
        if *n <= 3 {
            redirect(&format!("https://feeds.example/{n}"))
        } else {
            ok("<rss/>")
        }
    });
    let r = get(&send, "https://feeds.example/rss", None, None)
        .await
        .unwrap();
    assert_eq!(r.final_uri.as_str(), "https://feeds.example/3");

    let (send, seen) = fake(|_| redirect("https://feeds.example/again"));
    assert_eq!(
        get(&send, "https://feeds.example/rss", None, None)
            .await
            .unwrap_err(),
        "too many redirects"
    );
    assert_eq!(seen.lock().unwrap().len(), 6);
}

#[tokio::test]
async fn a_body_over_the_cap_is_refused() {
    // no Content-Length: the cap must hold while reading
    let (send, _) = fake(|_| FeedReply {
        body: futures_util::stream::iter(
            [
                vec![0u8; MAX_BYTES / 2],
                vec![0u8; MAX_BYTES / 2],
                vec![0u8; 1],
            ]
            .map(|b| Ok(Bytes::from(b))),
        )
        .boxed(),
        ..reply(200)
    });
    assert_eq!(
        get(&send, "https://feeds.example/rss", None, None)
            .await
            .unwrap_err(),
        TOO_LARGE
    );
    // exactly 4 MiB is allowed; a Content-Length over it is refused before reading
    let (send, _) = fake(|_| FeedReply {
        body: futures_util::stream::iter([Ok(Bytes::from(vec![0u8; MAX_BYTES]))]).boxed(),
        ..reply(200)
    });
    assert_eq!(
        get(&send, "https://feeds.example/rss", None, None)
            .await
            .unwrap()
            .body
            .unwrap()
            .len(),
        MAX_BYTES
    );
    let (send, _) = fake(|_| FeedReply {
        content_length: Some(MAX_BYTES as u64 + 1),
        ..ok("<rss/>")
    });
    assert_eq!(
        get(&send, "https://feeds.example/rss", None, None)
            .await
            .unwrap_err(),
        TOO_LARGE
    );
}

#[tokio::test]
async fn validators_are_sent_and_not_modified_is_reported() {
    let (send, seen) = fake(|_| reply(304));
    let r = get(
        &send,
        "https://feeds.example/rss",
        Some("\"v1\""),
        Some("Mon, 16 Nov 2026 09:30:00 GMT"),
    )
    .await
    .unwrap();
    assert!(r.not_modified);
    assert_eq!(r.etag.as_deref(), Some("\"v1\""));
    let seen = seen.lock().unwrap();
    let header = |name: &str| {
        seen[0]
            .2
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(header("If-None-Match").as_deref(), Some("\"v1\""));
    assert_eq!(
        header("If-Modified-Since").as_deref(),
        Some("Mon, 16 Nov 2026 09:30:00 GMT")
    );
    assert_eq!(header("Accept").as_deref(), Some(ACCEPT));
    assert_eq!(
        header("User-Agent"),
        Some(format!("PhoneBox/{}", crate::app_paths::version()))
    );
}

#[test]
fn local_names() {
    for (host, want) in [
        ("192.168.1.5", true),
        ("nas", true),
        ("nas.local", true),
        ("reader.lan", true),
        ("box.home.arpa", true),
        ("pc.tail1234.ts.net", true),
        ("rss.example.com", false),
        ("local.example.com", false),
        ("evil.com", false),
    ] {
        assert_eq!(is_local_name(host), want, "{host}");
    }
    assert!(is_local_name("[fd00::1]"));
    assert!(is_local_name("NAS.LOCAL."));
}

#[tokio::test]
async fn private_addresses_only_for_local_hosts() {
    for (url, private) in [
        ("https://rss.example.com/feed", false), // a public name: its DNS must not point into the network
        ("http://192.168.1.5/feed", true),
        ("http://nas.local/feed", true),
    ] {
        let (send, seen) = fake(|_| ok("<rss/>"));
        get(&send, url, None, None).await.unwrap();
        let pools: Vec<bool> = seen.lock().unwrap().iter().map(|(p, _, _)| *p).collect();
        assert_eq!(pools, [private], "{url}");
    }
}

#[tokio::test]
async fn a_redirect_from_a_local_host_to_another_host_is_public_only() {
    let (send, seen) = fake(|r| {
        if r.url.host_str() == Some("nas.local") {
            redirect("http://other.local/feed")
        } else {
            ok("<rss/>")
        }
    });
    get(&send, "http://nas.local/feed", None, None)
        .await
        .unwrap();
    assert_eq!(hosts(&seen), ["nas.local", "other.local"]);
    let pools: Vec<bool> = seen.lock().unwrap().iter().map(|(p, _, _)| *p).collect();
    assert_eq!(pools, [true, false]);
}

#[test]
fn hostile_titles_are_cut_before_cleaning() {
    // many '<' without '>' used to make the tag pattern rescan the rest of the text for each one
    let title = crate::feeds::parser::clean(&format!("{}tail", "<".repeat(2_000_000)), true);
    assert!(title.encode_utf16().count() <= 301);
    assert_eq!(crate::feeds::parser::clean("a<x>b</x>c", true), "a b c");
    assert_eq!(crate::feeds::parser::clean("1 &lt; 2", true), "1 < 2");
}

#[tokio::test]
async fn the_answer_keeps_dotnets_validators_and_the_fetch_errors_name_no_address() {
    let (send, _) = fake(|_| FeedReply {
        etag: Some(" W/\"v2\" ".into()),
        last_modified: Some("Tue, 17 Nov 2026 10:00:00 +0200".into()),
        ..ok("<rss/>")
    });
    let r = get(&send, "https://feeds.example/rss?key=s3cret", None, None)
        .await
        .unwrap();
    assert_eq!(r.etag.as_deref(), Some("W/\"v2\""));
    assert_eq!(
        r.last_modified.as_deref(),
        Some("Tue, 17 Nov 2026 08:00:00 GMT")
    );
    assert_eq!(entity_tag(Some("v2")), None);
    assert_eq!(entity_tag(Some("\"a\"b\"")), None);
    assert_eq!(http_date(Some("yesterday")), None);
    // a redirect without a Location, other statuses, and an address that is not http(s)
    let (send, _) = fake(|_| reply(301));
    assert_eq!(
        get(&send, "https://feeds.example/rss", None, None)
            .await
            .unwrap_err(),
        "HTTP 301 without a Location"
    );
    let (send, _) = fake(|_| reply(500));
    assert_eq!(
        get(&send, "https://feeds.example/rss", None, None)
            .await
            .unwrap_err(),
        "HTTP 500"
    );
    let (send, _) = fake(|_| redirect("ftp://feeds.example/x"));
    assert_eq!(
        get(&send, "https://feeds.example/rss", None, None)
            .await
            .unwrap_err(),
        "redirected to an address that is not http(s)"
    );
    let (send, _) = fake(|_| ok(""));
    assert_eq!(
        get(&send, "ftp://feeds.example/rss", None, None)
            .await
            .unwrap_err(),
        "the address must start with http:// or https://"
    );
    assert_eq!(
        get(&send, "http://10.0.0.5/feed?k=1", None, None)
            .await
            .map(|_| ()),
        Ok(()) // a typed private literal is local
    );
    let (send, _) = fake(|_| redirect("http://10.0.0.6/feed"));
    assert_eq!(
        get(&send, "http://10.0.0.5/feed", None, None)
            .await
            .unwrap_err(),
        "refused: 10.0.0.6 is not a public address"
    );
}
