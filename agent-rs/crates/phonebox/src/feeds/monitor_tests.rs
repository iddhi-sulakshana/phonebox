//! `FeedTests.cs` class `FeedMonitorTests`.

use std::sync::atomic::{AtomicI64, Ordering};

use futures_util::FutureExt;

use super::http::tests::{fake, ok, reply};
use super::http::{FeedReply, FeedRequest};
use super::*;

const RSS: &str = r#"<rss><channel><item><title>Hello</title><link>https://feeds.example/hello</link><pubDate>Mon, 16 Nov 2026 09:30:00 GMT</pubDate></item><item><title>Nolink</title></item></channel></rss>"#;

struct Fixture {
    _dir: tempfile::TempDir,
    cfg: Arc<ConfigStore>,
    now: Arc<AtomicI64>,
}

fn config_json(widget: &str, page_enabled: bool, feeds: &str, open: bool) -> String {
    format!(
        r#"{{"feeds":{feeds},"pages":[{{"id":"p","title":"P","enabled":{page_enabled},"widgets":[{{"id":"w","type":"{widget}","x":0,"y":0,"w":6,"h":4,"options":{{"open":{open}}}}}]}},{{"id":"q","title":"Q","widgets":[{{"id":"c","type":"clock","x":0,"y":0,"w":6,"h":4}}]}}]}}"#
    )
}

const BLOG: &str = r#"[{"name":"Blog","url":"https://feeds.example/rss"}]"#;

impl Fixture {
    fn new(widget: &str, page_enabled: bool, feeds: &str, open: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, config_json(widget, page_enabled, feeds, open)).unwrap();
        Fixture {
            cfg: Arc::new(ConfigStore::new(path)),
            _dir: dir,
            now: Arc::new(AtomicI64::new(1_800_000_000_000)),
        }
    }

    fn blog() -> Fixture {
        Fixture::new("feeds", true, BLOG, true)
    }

    fn monitor(&self, send: FeedSend) -> (FeedMonitor, Arc<Mutex<Vec<String>>>) {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let log = opened.clone();
        let now = self.now.clone();
        let m = FeedMonitor::new(self.cfg.clone())
            .with_send(send)
            .with_clock(move || now.load(Ordering::SeqCst))
            .with_opener(move |u| {
                log.lock().unwrap().push(u.to_string());
                Ok(())
            });
        (m, opened)
    }

    fn advance(&self, ms: i64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    fn save(&self, json: &str) {
        self.cfg.save(json, None).unwrap();
    }
}

#[tokio::test]
async fn in_flight_fetch_publishes_only_current_sources() {
    for name in [Some("Blog"), Some("Renamed"), None] {
        let fixture = Fixture::blog();
        let entered = Arc::new(tokio::sync::Notify::new());
        let (release, released) = tokio::sync::watch::channel(false);
        let e = entered.clone();
        let send: FeedSend = Arc::new(move |_, _req: FeedRequest| {
            let (e, mut released) = (e.clone(), released.clone());
            async move {
                e.notify_one();
                let _ = released.wait_for(|r| *r).await;
                Ok(ok(RSS))
            }
            .boxed()
        });
        let (m, _) = fixture.monitor(send);
        let m = Arc::new(m);
        let pending = tokio::spawn({
            let m = m.clone();
            async move { m.tick().await }
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        match name {
            Some("Blog") => {}
            Some(renamed) => fixture.save(&config_json(
                "feeds",
                true,
                &format!(r#"[{{"name":"{renamed}","url":"https://feeds.example/rss"}}]"#),
                true,
            )),
            None => fixture.save(&config_json("feeds", true, "[]", true)),
        }
        release.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap();
        let actual: Vec<String> = m
            .snapshot()
            .items
            .iter()
            .map(|i| i.source.clone())
            .collect();
        let expected: Vec<String> = name.map(|n| vec![n.to_string(); 2]).unwrap_or_default();
        assert_eq!(actual, expected, "{name:?}");
    }
}

#[tokio::test]
async fn each_feed_is_fetched_at_most_every_fifteen_minutes() {
    let fixture = Fixture::blog();
    let (send, seen) = fake(|_| ok(RSS));
    let (m, _) = fixture.monitor(send);
    m.tick().await;
    assert_eq!(seen.lock().unwrap().len(), 1);
    let snap = m.snapshot();
    let titles: Vec<&str> = snap.items.iter().map(|i| i.title.as_str()).collect();
    assert_eq!(titles, ["Hello", "Nolink"]);
    assert!(m.snapshot().items.iter().all(|i| i.source == "Blog"));

    fixture.advance(INTERVAL_MS - 1000);
    m.tick().await;
    assert_eq!(seen.lock().unwrap().len(), 1);

    fixture.advance(1000);
    m.tick().await;
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn nothing_is_fetched_without_a_feeds_widget_on_an_enabled_page() {
    let (send, seen) = fake(|_| ok(RSS));
    let hidden = Fixture::new("feeds", false, BLOG, true);
    hidden.monitor(send.clone()).0.tick().await;
    let other = Fixture::new("clock", true, BLOG, true);
    other.monitor(send).0.tick().await;
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_fetch_keeps_the_last_headlines_and_reports_the_error() {
    let fixture = Fixture::blog();
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let f = fail.clone();
    let (send, _) = fake(move |_| {
        if f.load(Ordering::SeqCst) {
            reply(500)
        } else {
            ok(RSS)
        }
    });
    let (m, _) = fixture.monitor(send);
    m.tick().await;
    fail.store(true, Ordering::SeqCst);
    fixture.advance(INTERVAL_MS);
    m.tick().await;
    assert_eq!(m.snapshot().items.len(), 2);
    assert_eq!(m.snapshot().errors, ["Blog: HTTP 500"]);
}

#[tokio::test]
async fn articles_open_by_id_only_when_allowed() {
    let fixture = Fixture::blog();
    let (send, _) = fake(|_| ok(RSS));
    let (m, opened) = fixture.monitor(send);
    m.tick().await;
    let items = m.snapshot().items.clone();
    assert_eq!(m.open(Some(&items[0].id)), None);
    assert_eq!(*opened.lock().unwrap(), ["https://feeds.example/hello"]);
    assert_eq!(
        m.open(Some(&items[1].id)).as_deref(),
        Some("This headline has no link")
    );
    assert_eq!(
        m.open(Some("0123456789abcdef")).as_deref(),
        Some("That headline is no longer in the list")
    );
    assert_eq!(
        m.open(None).as_deref(),
        Some("That headline is no longer in the list")
    );
    assert_eq!(opened.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn opening_needs_the_widget_on_an_enabled_page() {
    let fixture = Fixture::blog();
    let (send, _) = fake(|_| ok(RSS));
    let (m, opened) = fixture.monitor(send);
    m.tick().await;
    let id = m.snapshot().items[0].id.clone();
    fixture.save(&config_json("feeds", false, BLOG, true));
    assert!(!m.open_allowed());
    assert_eq!(m.open(Some(&id)).as_deref(), Some(OPEN_OFF));
    assert!(opened.lock().unwrap().is_empty());
}

#[tokio::test]
async fn opening_is_off_until_the_widget_allows_it() {
    let fixture = Fixture::new("feeds", true, BLOG, false);
    let (send, _) = fake(|_| ok(RSS));
    let (m, opened) = fixture.monitor(send);
    m.tick().await;
    assert!(m.shown()); // the headlines are still fetched and shown
    assert!(!m.open_allowed());
    assert!(m.open(Some(&m.snapshot().items[0].id)).is_some());
    assert!(opened.lock().unwrap().is_empty());
}

#[tokio::test]
async fn no_fetch_starts_after_the_widget_is_removed() {
    let fixture = Fixture::new(
        "feeds",
        true,
        r#"[{"name":"A","url":"https://a.example/rss"},{"name":"B","url":"https://b.example/rss"}]"#,
        true,
    );
    let cfg = fixture.cfg.clone();
    let hidden = config_json(
        "feeds",
        false,
        r#"[{"name":"A","url":"https://a.example/rss"},{"name":"B","url":"https://b.example/rss"}]"#,
        true,
    );
    let (send, seen) = fake(move |_| {
        // while the first feed is being fetched, the page with the widget is hidden
        cfg.save(&hidden, None).unwrap();
        ok(RSS)
    });
    let (m, _) = fixture.monitor(send);
    m.tick().await;
    let hosts: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(_, u, _)| u.host_str().unwrap().to_string())
        .collect();
    assert_eq!(hosts, ["a.example"]);
}

#[test]
fn the_link_never_reaches_the_phone() {
    let item = FeedItem {
        id: "id".into(),
        title: "t".into(),
        source: "s".into(),
        published: 1,
        link: Some("https://secret.example/?token=abc".into()),
    };
    let json = serde_json::to_string(&item).unwrap();
    assert!(!json.contains("secret"));
    assert_eq!(
        json,
        r#"{"id":"id","title":"t","source":"s","published":1,"openable":true}"#
    );
}

#[test]
fn feed_addresses_stay_off_the_phone() {
    let fixture = Fixture::new(
        "feeds",
        true,
        r#"[{"name":"Private","url":"https://feeds.example/rss?key=s3cret"}]"#,
        true,
    );
    let public = crate::config::public::to_public(&fixture.cfg.get(), None, None);
    assert!(!public.to_string().contains("s3cret"));
}

#[tokio::test]
async fn headlines_are_newest_first_across_feeds_and_name_their_feed() {
    let fixture = Fixture::new(
        "feeds",
        true,
        r#"[{"name":"  ","url":"https://a.example/rss"},{"name":"B","url":"https://b.example/rss"},{"name":"Dup","url":"https://a.example/rss"},{"name":"","url":"  "}]"#,
        true,
    );
    let (send, seen) = fake(|r: &FeedRequest| {
        if r.url.host_str() == Some("a.example") {
            ok(
                r#"<rss><channel><item><title>A old</title><pubDate>Mon, 16 Nov 2026 08:00:00 GMT</pubDate></item><item><title>A undated</title></item></channel></rss>"#,
            )
        } else {
            ok(
                r#"<rss><channel><item><title>B new</title><pubDate>Mon, 16 Nov 2026 10:00:00 GMT</pubDate></item><item><title>B undated</title></item></channel></rss>"#,
            )
        }
    });
    let (m, _) = fixture.monitor(send);
    m.tick().await;
    assert_eq!(seen.lock().unwrap().len(), 2); // the duplicate address and the blank one are skipped
    let snap = m.snapshot();
    let got: Vec<(&str, &str)> = snap
        .items
        .iter()
        .map(|i| (i.title.as_str(), i.source.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("B new", "B"),
            ("A old", "a.example"),
            ("A undated", "a.example"),
            ("B undated", "B")
        ]
    );
    assert_eq!(snap.updated, 1_800_000_000_000);
}

#[tokio::test(start_paused = true)]
async fn a_feed_that_is_not_xml_or_does_not_answer_reports_why() {
    let fixture = Fixture::new(
        "feeds",
        true,
        r#"[{"name":"Html","url":"https://html.example/"},{"name":"Slow","url":"https://slow.example/"}]"#,
        true,
    );
    let send: FeedSend = Arc::new(|_, req: FeedRequest| {
        let slow = req.url.host_str() == Some("slow.example");
        async move {
            if slow {
                futures_util::future::pending::<()>().await;
            }
            Ok::<FeedReply, String>(ok("<html><body>hi</body></html>"))
        }
        .boxed()
    });
    let (m, _) = fixture.monitor(send);
    m.tick().await;
    assert_eq!(
        m.snapshot().errors,
        ["Html: not an RSS or Atom feed", "Slow: no answer in 20 s"]
    );
}
