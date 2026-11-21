//! `RadioDirectoryTests.cs` and `RadioDirectoryBodyTests.cs`.

use std::net::IpAddr;
use std::sync::Arc;

use axum::body::Bytes;
use futures_util::StreamExt;

use super::*;
use crate::control_deck::fetch::{Fetch, FetchError, FetchRequest, FetchResponse};

fn url(s: &str) -> reqwest::Url {
    reqwest::Url::parse(s).unwrap()
}

/// A fake transport answering by request: (status, content type, location, body), or a transport error.
fn fake(
    respond: impl Fn(
        &reqwest::Url,
    ) -> Result<(u16, &'static str, Option<&'static str>, Vec<u8>), FetchError>
    + Send
    + Sync
    + 'static,
) -> Fetch {
    Arc::new(move |req: FetchRequest| {
        let answer = respond(&req.url);
        Box::pin(async move {
            let (status, content_type, location, body) = answer?;
            Ok(FetchResponse {
                status,
                location: location.map(str::to_string),
                content_type: Some(content_type.to_string()).filter(|t| !t.is_empty()),
                content_length: Some(body.len() as u64),
                body: futures_util::stream::once(async move { Ok(Bytes::from(body)) }).boxed(),
            })
        })
    })
}

fn item(id: &str, stream: &str) -> Option<CheckItem> {
    Some(CheckItem {
        id: Some(id.into()),
        stream_url: Some(stream.into()),
    })
}

#[test]
fn parse_countries_serializes_station_count_and_deduplicates_by_iso_code() {
    let countries = parse_countries(
        r#"[
          { "iso_3166_1": "TR", "name": "Turkey", "stationcount": 420 },
          { "iso_3166_1": "tr", "name": "Türkiye", "stationcount": 512 },
          { "iso_3166_1": "DE", "name": "Germany", "stationcount": 3200 },
          { "iso_3166_1": "XX", "name": "Empty", "stationcount": 0 }
        ]"#,
    );
    assert_eq!(countries.len(), 2);
    assert_eq!(countries[0].code, "DE");
    assert_eq!(countries[0].station_count, 3200);
    assert_eq!(countries[1].code, "TR");
    assert_eq!(countries[1].station_count, 512);
    assert!(
        serde_json::to_string(&countries[1])
            .unwrap()
            .contains("\"stationCount\":512")
    );
}

#[test]
fn parse_stations_deduplicates_and_filters_non_http_streams() {
    let stations = parse_stations(
        r#"[
          { "name": "  Power   FM ", "url_resolved": "https://stream.example.com/power.mp3", "favicon": "https://example.com/logo.png", "clickcount": 1200, "votes": 80 },
          { "name": "Power FM", "url_resolved": "https://stream.example.com/power-mirror.mp3", "clickcount": 900, "votes": 50 },
          { "name": "Unsafe RTSP", "url_resolved": "rtsp://stream.example.com/live", "clickcount": 500, "votes": 10 },
          { "name": "Local Stream", "url_resolved": "http://127.0.0.1/local.mp3", "clickcount": 400, "votes": 10 },
          { "name": "Fallback FM", "url_resolved": "", "url": "https://stream.example.com/fallback.mp3", "favicon": "http://192.168.1.10/logo.png", "clickcount": 350, "votes": 25 },
          { "name": "Jazz Radio", "url_resolved": "https://stream.example.com/jazz.aac", "favicon": "ftp://bad", "clickcount": 300, "votes": 20 }
        ]"#,
        "TR",
        10,
    );
    assert_eq!(stations.len(), 3);
    assert_eq!(stations[0].id, "radio-tr-power-fm");
    assert_eq!(stations[0].label, "Power FM");
    assert_eq!(
        stations[0].logo_url.as_deref(),
        Some("https://example.com/logo.png")
    );
    assert_eq!(stations[0].click_count, 1200);
    assert_eq!(stations[1].id, "radio-tr-fallback-fm");
    assert_eq!(
        stations[1].stream_url,
        "https://stream.example.com/fallback.mp3"
    );
    assert_eq!(stations[1].logo_url, None);
    assert_eq!(stations[2].id, "radio-tr-jazz-radio");
    assert_eq!(stations[2].logo_url, None);
    assert_eq!(slugify("İstanbul FM"), "istanbul-fm");
    assert!(!is_safe_public_uri(&url("http://127.0.0.1:8765/api/state")));
    assert!(!is_safe_public_uri(&url("http://192.168.1.50/stream.mp3")));
}

#[test]
fn stream_identity_preserves_path_and_query_case() {
    let stations = parse_stations(
        r#"[
          { "name": "Upper path", "url": "https://stream.example.com/Live?channel=A" },
          { "name": "Lower path", "url": "https://stream.example.com/live?channel=A" },
          { "name": "Lower channel", "url": "https://stream.example.com/live?channel=a" },
          { "name": "Same upper path", "url": "HTTPS://STREAM.EXAMPLE.COM:443/Live?channel=A" }
        ]"#,
        "TR",
        10,
    );
    let labels: Vec<&str> = stations.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(labels, ["Upper path", "Lower path", "Lower channel"]);
}

#[test]
fn a_discarded_station_name_does_not_reserve_its_stream() {
    let stations = parse_stations(
        r#"[
          { "name": "First station", "url": "https://stream.example.com/first" },
          { "name": "First station", "url": "https://stream.example.com/second" },
          { "name": "Second station", "url": "https://stream.example.com/second" }
        ]"#,
        "TR",
        10,
    );
    let labels: Vec<&str> = stations.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(labels, ["First station", "Second station"]);
}

#[test]
fn hostnames_that_point_into_the_home_network_are_not_imported() {
    for unsafe_url in [
        "http://router/stream.mp3",
        "http://nas.lan/radio",
        "http://printer.home.arpa/x",
        "http://media.internal:8000/live",
        "http://speaker.local/",
        "http://LOCALHOST./a",
        "http://192.168.1.1.nip.io/live",
        "http://10-0-0-7.sslip.io/live",
        "http://app.127.0.0.1.nip.io/x",
        "http://[64:ff9b::7f00:1]/live",
        "http://[64:ff9b::c0a8:101]/live",
        "http://198.18.0.10/live",
        "http://192.0.0.5/live",
    ] {
        assert!(!is_safe_public_uri(&url(unsafe_url)), "{unsafe_url}");
    }
}

#[test]
fn public_stream_hosts_are_imported() {
    for safe in [
        "https://stream.example.com/live.mp3",
        "http://ice1.somafm.com/groovesalad-128-mp3",
        "https://8.8.8.8.nip.io/live",
        "https://radio-2024.example.org/128",
    ] {
        assert!(is_safe_public_uri(&url(safe)), "{safe}");
    }
}

/// `RadioDirectoryTests.FakeHandler`: a stream, an HTML page, a transport failure and three redirects.
fn streams() -> Fetch {
    fake(|u| {
        let path = u.path();
        let redirect = |to: &'static str| Ok((302, "", Some(to), vec![]));
        if path.ends_with("/net-error.mp3") {
            return Err(FetchError::Other("simulated failure".into()));
        }
        if path.ends_with("/downgrade.mp3") {
            return redirect("http://stream.example.com/plain.mp3");
        }
        if path.ends_with("/private-redirect.mp3") {
            return redirect("https://127.0.0.1/internal");
        }
        if path.ends_with("/ftp-redirect.mp3") {
            return redirect("ftp://files.example.com/stream.mp3");
        }
        let kind = if path.ends_with(".html") {
            "text/html; charset=utf-8"
        } else {
            "audio/mpeg"
        };
        Ok((200, kind, None, b"data".to_vec()))
    })
}

#[tokio::test]
async fn check_streams_reports_live_and_html_without_mutating_input() {
    let directory = RadioDirectory::with_fetch(streams());
    let items = vec![
        item("s1", "https://stream.example.com/live.mp3"),
        item("s2", "https://stream.example.com/page.html"),
        item("s3", "ftp://invalid"),
        item("s4", "http://127.0.0.1:8765/api/state"),
        item("s5", "https://stream.example.com/net-error.mp3"),
        item("s6", "https://stream.example.com/downgrade.mp3"),
        item("s7", "https://stream.example.com/private-redirect.mp3"),
    ];
    let results = directory.check(items).await;
    assert_eq!(results.len(), 7);
    assert!(results[0].ok);
    assert_eq!(results[0].status, "Live");
    assert!(!results[1].ok);
    assert!(results[1].status.contains("HTML"));
    assert_eq!(results[2].status, "Invalid URL");
    assert_eq!(results[3].status, "local address, not checked");
    assert_eq!(results[4].status, "Connection failed");
    assert_eq!(results[5].status, "Invalid redirect");
    assert_eq!(results[6].status, "local address, not checked");
    assert!(results[1..].iter().all(|r| !r.ok));
    let ids: Vec<Option<&str>> = results.iter().map(|r| r.id.as_deref()).collect();
    assert_eq!(
        ids,
        [
            Some("s1"),
            Some("s2"),
            Some("s3"),
            Some("s4"),
            Some("s5"),
            Some("s6"),
            Some("s7")
        ]
    );
}

#[tokio::test]
async fn get_countries_falls_back_across_mirrors_and_throws_readable_error_when_all_fail() {
    let mirrors = |fail_all: bool| {
        fake(move |u| {
            if !fail_all && u.host_str().unwrap_or("").starts_with("fi1.") {
                return Ok((
                    200,
                    "application/json",
                    None,
                    br#"[{ "iso_3166_1": "TR", "name": "Turkey", "stationcount": 512 }]"#.to_vec(),
                ));
            }
            Ok((502, "", None, vec![]))
        })
    };
    let countries = RadioDirectory::with_fetch(mirrors(false))
        .countries()
        .await
        .unwrap();
    assert_eq!(countries.len(), 1);
    assert_eq!(countries[0].code, "TR");
    assert_eq!(countries[0].station_count, 512);
    assert_eq!(
        RadioDirectory::with_fetch(mirrors(true))
            .countries()
            .await
            .unwrap_err(),
        UNAVAILABLE
    );
}

#[tokio::test]
async fn a_mirror_that_answers_without_the_list_is_skipped_like_a_failing_one() {
    for (body, kind) in [
        (
            "<html><body>Down for maintenance</body></html>",
            "text/html",
        ),
        (r#"{"error":"overloaded"}"#, "application/json"),
        ("", "application/json"),
    ] {
        let directory = RadioDirectory::with_fetch(fake(move |u| {
            if u.host_str().unwrap_or("").starts_with("de1.") {
                Ok((200, kind, None, body.as_bytes().to_vec()))
            } else {
                Ok((
                    200,
                    "application/json",
                    None,
                    br#"[{ "iso_3166_1": "TR", "name": "Turkey", "stationcount": 512 }]"#.to_vec(),
                ))
            }
        }));
        let countries = directory.countries().await.unwrap();
        assert_eq!(countries.len(), 1, "{body}");
        assert_eq!(countries[0].code, "TR");
        let all_bad = RadioDirectory::with_fetch(fake(move |_| {
            Ok((200, kind, None, body.as_bytes().to_vec()))
        }));
        assert_eq!(
            all_bad.countries().await.unwrap_err(),
            UNAVAILABLE,
            "{body}"
        );
    }
}

#[test]
fn private_tunnel_and_reserved_addresses_are_rejected_and_non_http_redirects_report_invalid_redirect()
 {
    for unsafe_url in [
        "http://[2002:7f00:1::1]/live",
        "http://[2002:c0a8:101::1]/live",
        "http://[2001:0:4136:e378:8000:63bf:80ff:fffe]/live",
        "http://[::7f00:1]/live",
        "http://[::ffff:0:c0a8:101]/live",
        "http://[2001:db8::1]/live",
        "http://[2001:2::1]/live",
        "http://[100::1]/live",
        "http://192.0.2.10/live",
        "http://198.51.100.10/live",
        "http://203.0.113.10/live",
        "http://192.88.99.1/live",
    ] {
        assert!(!is_safe_public_uri(&url(unsafe_url)), "{unsafe_url}");
    }
    assert!(is_safe_public_uri(&url("http://[2002:0808:0808::1]/live")));
}

#[tokio::test]
async fn non_http_redirect_scheme_reports_invalid_redirect_instead_of_local_address() {
    let directory = RadioDirectory::with_fetch(streams());
    let results = directory
        .check(vec![item(
            "s-ftp",
            "https://stream.example.com/ftp-redirect.mp3",
        )])
        .await;
    assert!(!results[0].ok);
    assert_eq!(results[0].status, "Invalid redirect");
}

#[tokio::test]
async fn a_list_with_a_utf8_byte_order_mark_is_read() {
    let mut body = vec![0xEF, 0xBB, 0xBF];
    body.extend_from_slice(br#"[{ "iso_3166_1": "TR", "name": "Turkey", "stationcount": 512 }]"#);
    let directory = RadioDirectory::with_fetch(fake(move |_| {
        Ok((200, "application/json", None, body.clone()))
    }));
    let countries = directory.countries().await.unwrap();
    assert_eq!(countries.len(), 1);
    assert_eq!(countries[0].code, "TR");
}

#[test]
fn documentation_prefixes_are_not_public() {
    for (address, expected) in [
        ("3fff::1", false),
        ("3fff:fff:ffff::1", false),
        ("3fff:1000::1", true),
        ("2001:db8::1", false),
        ("2606:4700:4700::1111", true),
    ] {
        assert_eq!(
            is_public_ip(address.parse::<IpAddr>().unwrap()),
            expected,
            "{address}"
        );
    }
}

// Rust-only: the embedded-address scan, .NET's IPv4 forms, the station search's request and limits, names and slugs,
// and the real transport's refusals.

#[test]
fn embedded_addresses_follow_the_dotnet_pattern_and_parse() {
    assert_eq!(embedded_ipv4("a.1.2.3.4.b"), ["1.2.3.4"]);
    assert_eq!(embedded_ipv4("10-0-0-7.sslip.io"), ["10-0-0-7"]);
    assert!(embedded_ipv4("1234.1.1.1.x").is_empty());
    assert!(embedded_ipv4("1.1.1.1234").is_empty());
    assert_eq!(embedded_ipv4("1.2.3.4444.5.6.7.8"), ["5.6.7.8"]);
    assert_eq!(
        dotnet_ipv4("010.0.0.1"),
        Some(std::net::Ipv4Addr::new(8, 0, 0, 1))
    );
    assert_eq!(
        dotnet_ipv4("192.168.001.001"),
        Some(std::net::Ipv4Addr::new(192, 168, 1, 1))
    );
    assert_eq!(dotnet_ipv4("08.1.1.1"), None);
    assert_eq!(dotnet_ipv4("256.1.1.1"), None);
    // a private address written in octal still counts
    assert!(!is_safe_public_uri(&url("http://012.0.0.1.nip.io/x")));
}

#[tokio::test]
async fn the_station_search_asks_once_with_its_limits() {
    let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let directory = RadioDirectory::with_fetch({
        let asked = asked.clone();
        fake(move |u| {
            asked.lock().unwrap().push(u.to_string());
            Ok((200, "application/json", None, b"[]".to_vec()))
        })
    });
    assert!(directory.stations("1x", 10).await.unwrap().is_empty());
    assert!(directory.stations("tur", 10).await.unwrap().is_empty());
    assert!(asked.lock().unwrap().is_empty());
    assert!(directory.stations(" tr ", 0).await.unwrap().is_empty());
    assert!(directory.stations("de", 500).await.unwrap().is_empty());
    assert_eq!(
        *asked.lock().unwrap(),
        [
            "https://de1.api.radio-browser.info/json/stations/search?countrycode=TR&hidebroken=true&order=clickcount&reverse=true&limit=80",
            "https://de1.api.radio-browser.info/json/stations/search?countrycode=DE&hidebroken=true&order=clickcount&reverse=true&limit=200",
        ]
    );
}

#[test]
fn station_names_and_slugs() {
    assert_eq!(clean_station_name("  A \t  B\n C "), "A B C");
    let long = format!("{} tail", "x".repeat(47));
    assert_eq!(clean_station_name(&long), "x".repeat(47));
    assert_eq!(slugify("Çağrı Şükür Ötesi"), "cagri-sukur-otesi");
    assert_eq!(slugify("***"), "station");
    assert_eq!(slugify(&"ab-".repeat(20)), "ab-ab-ab-ab-ab-ab-ab-ab-ab-a");
    assert_eq!(slugify("ÖZEL — FM!"), "ozel-fm");
}

#[tokio::test]
async fn the_real_transport_refuses_local_names_and_reports_refused_connections() {
    let fetch = crate::control_deck::fetch::public_fetch(std::time::Duration::from_secs(5));
    let refused = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        fetch(FetchRequest::get(url(&format!("http://127.0.0.1:{port}/")))).await
    };
    assert!(
        matches!(refused, Err(FetchError::Refused)),
        "{:?}",
        refused.err()
    );
    let local = fetch(FetchRequest::get(url("http://localhost:9/"))).await;
    assert!(
        matches!(local, Err(FetchError::NonPublic)),
        "{:?}",
        local.err()
    );
}
