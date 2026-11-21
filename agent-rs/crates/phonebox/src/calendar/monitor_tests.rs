//! `MonitorAuditTests.CalendarsWithTheSameNameDoNotMultiplyEventsWhileAFeedFails` and the monitor's own behaviour.

use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::FutureExt;

use super::*;
use crate::testutil::{FakeHttp, Reply};

fn store(json: &str) -> (tempfile::TempDir, Arc<ConfigStore>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, json).unwrap();
    (dir, Arc::new(ConfigStore::new(path)))
}

fn tomorrow_ics(title: &str) -> String {
    let day = chrono::Utc::now().date_naive() + chrono::Days::new(1);
    let d = day.format("%Y%m%d");
    format!(
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{title}\r\nDTSTART:{d}T090000Z\r\nDTEND:{d}T093000Z\r\nSUMMARY:{title}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn titles(m: &CalendarMonitor) -> Vec<String> {
    let mut t: Vec<String> = m
        .snapshot()
        .events
        .iter()
        .map(|e| e.title.clone())
        .collect();
    t.sort();
    t
}

#[tokio::test]
async fn calendars_with_the_same_name_do_not_multiply_events_while_a_feed_fails() {
    let healthy = Arc::new(AtomicBool::new(true));
    let h = healthy.clone();
    let server = FakeHttp::start(move |req| {
        let ok = h.load(Ordering::SeqCst);
        async move {
            if ok {
                Reply {
                    status: 200,
                    content_type: "text/calendar",
                    body: tomorrow_ics(req.path.trim_matches('/')),
                }
            } else {
                Reply {
                    status: 503,
                    content_type: "text/plain",
                    body: String::new(),
                }
            }
        }
    })
    .await;
    // "Add calendar" in the settings creates entries with an empty name, so two unnamed feeds are the normal case
    let (_d, cfg) = store(&format!(
        r#"{{"calendars":[{{"name":"","url":"{0}/work"}},{{"name":"","url":"{0}/home"}}]}}"#,
        server.url
    ));
    let monitor = CalendarMonitor::new(cfg.clone()).with_zone(TimeZone::UTC);
    let calendars = cfg.get().calendars.clone();
    monitor.refresh(&calendars).await;
    assert_eq!(titles(&monitor), ["home", "work"]);

    healthy.store(false, Ordering::SeqCst); // the network drops (laptop asleep, Wi-Fi down) for half an hour
    for _ in 0..3 {
        monitor.refresh(&calendars).await;
    }
    let snap = monitor.snapshot();
    assert_eq!(
        snap.errors,
        ["Calendar: Response status code does not indicate success: 503 (Service Unavailable)."; 2]
    );
    assert_eq!(snap.events.len(), 2);
    assert_eq!(titles(&monitor), ["home", "work"]);
}

fn fake(
    answer: impl Fn(&str) -> Result<String, String> + Send + Sync + 'static,
) -> (IcsFetch, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let fetch: IcsFetch = Arc::new(move |url: String| {
        let result = answer(&url);
        log.lock().unwrap().push(url);
        async move { result }.boxed()
    });
    (fetch, asked)
}

#[tokio::test]
async fn feeds_are_named_coloured_sorted_and_webcal_is_read_over_https() {
    let (_d, cfg) = store(
        r##"{"calendars":[{"name":"  Work ","url":" webcal://cal.example/a.ics ","color":"#ff0000"},{"name":" ","url":"https://cal.example/b.ics"},{"name":"Skip","url":"  "}]}"##,
    );
    let (fetch, asked) = fake(|url| {
        let day = chrono::Utc::now().date_naive() + chrono::Days::new(1);
        let d = day.format("%Y%m%d");
        Ok(if url.ends_with("a.ics") {
            format!(
                "BEGIN:VEVENT\nDTSTART:{d}T100000Z\nSUMMARY:Zeta\nEND:VEVENT\nBEGIN:VEVENT\nDTSTART:{d}T100000Z\nSUMMARY:Alpha\nEND:VEVENT"
            )
        } else {
            format!("BEGIN:VEVENT\nDTSTART:{d}T080000Z\nSUMMARY:Early\nEND:VEVENT")
        })
    });
    let monitor = CalendarMonitor::new(cfg.clone())
        .with_fetch(fetch)
        .with_zone(TimeZone::UTC);
    monitor.refresh(&sources(&cfg.get())).await;
    assert_eq!(
        *asked.lock().unwrap(),
        ["https://cal.example/a.ics", "https://cal.example/b.ics"]
    );
    let snap = monitor.snapshot();
    let got: Vec<(&str, &str, &str)> = snap
        .events
        .iter()
        .map(|e| (e.title.as_str(), e.calendar.as_str(), e.color.as_str()))
        .collect();
    // the settings trim names (Config.cs:656); a blank one is "Calendar"
    assert_eq!(
        got,
        [
            ("Early", "Calendar", "#60a5fa"),
            ("Alpha", "Work", "#ff0000"),
            ("Zeta", "Work", "#ff0000")
        ]
    );
    assert!(snap.errors.is_empty());
    assert!(snap.updated > 0);
}

#[tokio::test]
async fn a_failing_feed_reports_why_without_its_address() {
    let server = FakeHttp::start(|_| async {
        Reply {
            status: 404,
            content_type: "text/plain",
            body: String::new(),
        }
    })
    .await;
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = closed.local_addr().unwrap().port();
    drop(closed);
    let (_d, cfg) = store(&format!(
        r#"{{"calendars":[{{"name":"Gone","url":"{}/private/s3cret.ics"}},{{"name":"Dead","url":"http://127.0.0.1:{dead}/s3cret.ics"}},{{"name":"Odd","url":"not a url"}},{{"name":"Ftp","url":"ftp://cal.example/s3cret.ics"}}]}}"#,
        server.url
    ));
    let monitor = CalendarMonitor::new(cfg.clone()).with_zone(TimeZone::UTC);
    monitor.refresh(&sources(&cfg.get())).await;
    let errors = monitor.snapshot().errors.clone();
    assert_eq!(
        errors[0],
        "Gone: Response status code does not indicate success: 404 (Not Found)."
    );
    assert!(
        errors[1].starts_with("Dead: ") && errors[1].ends_with(&format!("(127.0.0.1:{dead})")),
        "{errors:?}"
    );
    assert_eq!(
        errors[2],
        "Odd: An invalid request URI was provided. Either the request URI must be an absolute URI or BaseAddress must be set."
    );
    assert_eq!(errors[3], "Ftp: The 'ftp' scheme is not supported.");
    assert!(errors.iter().all(|e| !e.contains("s3cret")), "{errors:?}");
}

#[test]
fn feed_text_is_decoded_as_dotnet_reads_it() {
    assert_eq!(decode_body("é".as_bytes(), None).unwrap(), "é");
    assert_eq!(decode_body(&[0xEF, 0xBB, 0xBF, b'a'], None).unwrap(), "a");
    assert_eq!(decode_body(&[0xFF, 0xFE, b'a', 0], None).unwrap(), "a");
    assert_eq!(decode_body(&[0xFE, 0xFF, 0, b'a'], None).unwrap(), "a");
    assert_eq!(
        decode_body(&[0xE9], Some("text/calendar; charset=ISO-8859-1")).unwrap(),
        "é"
    );
    assert_eq!(
        decode_body(&[0xE9], Some("text/calendar; charset=\"us-ascii\"")).unwrap(),
        "?"
    );
    assert_eq!(decode_body(&[0xE9], None).unwrap(), "\u{FFFD}");
    assert_eq!(
        decode_body(
            &[0xEF, 0xBB, 0xBF, b'a'],
            Some("text/calendar;charset=utf-8")
        )
        .unwrap(),
        "a"
    );
    assert_eq!(
        decode_body(b"x", Some("text/calendar; charset=windows-1252")).unwrap_err(),
        "The character set provided in ContentType is invalid. Cannot read content as string using an invalid character set."
    );
}

#[tokio::test]
async fn the_current_event_is_a_timed_one_happening_now_matching_a_name() {
    let (_d, cfg) = store("{}");
    let monitor = CalendarMonitor::new(cfg);
    let now = crate::server::now_ms();
    let event = |title: &str, start: i64, end: i64, all_day: bool| CalendarEvent {
        title: title.into(),
        start,
        end,
        all_day,
        location: None,
        calendar: String::new(),
        color: String::new(),
    };
    *lock(&monitor.snap) = Arc::new(CalendarSnapshot {
        events: vec![
            event("Holiday", now - 1000, now + 1000, true),
            event("Lunch", now - 1000, now + 1000, false),
            event("Team Standup", now - 1000, now + 1000, false),
            event("Ended", now - 2000, now, false),
        ],
        updated: now,
        errors: vec![],
    });
    assert_eq!(monitor.current(&[]).map(|e| e.title), Some("Lunch".into()));
    assert_eq!(
        monitor.current(&["standup".into()]).map(|e| e.title),
        Some("Team Standup".into())
    );
    assert_eq!(monitor.current(&["ended".into()]), None);
}

#[tokio::test(start_paused = true)]
async fn the_loop_refreshes_at_once_and_when_the_calendars_change() {
    let (_d, cfg) = store("{}");
    let (fetch, asked) = fake(|_| Ok(String::new()));
    let monitor = Arc::new(
        CalendarMonitor::new(cfg.clone())
            .with_fetch(fetch)
            .with_zone(TimeZone::UTC),
    );
    let stop = Stop::default();
    let task = monitor.start(stop.clone());
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(monitor.snapshot().updated > 0); // the first pass runs even with no calendars
    cfg.save(
        r#"{"calendars":[{"name":"A","url":"https://cal.example/a.ics"}]}"#,
        None,
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(3100)).await;
    assert_eq!(*asked.lock().unwrap(), ["https://cal.example/a.ics"]);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(asked.lock().unwrap().len(), 1); // nothing changed: the next read waits for 10 minutes
    stop.stop();
    task.await.unwrap();
}
