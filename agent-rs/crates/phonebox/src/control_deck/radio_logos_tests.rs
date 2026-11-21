//! `RadioCardViewsTests.cs` (all but `TapToToggle…`, which is with the radio), and the monogram and cache key against
//! what the C# agent produces.

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Bytes;
use base64::Engine;
use futures_util::StreamExt;

use super::*;
use crate::control_deck::fetch::FetchError;
use crate::control_deck::radio_directory::is_public_ip;

fn png() -> Vec<u8> {
    let mut png = vec![0u8; 48];
    png[..8].copy_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    png
}

type Answer = Result<(u16, Option<&'static str>, Vec<u8>), FetchError>;

/// A fake transport counting its requests; `respond` gives (status, location, body).
fn fake(
    hits: &Arc<AtomicUsize>,
    respond: impl Fn(&str) -> Answer + Send + Sync + 'static,
) -> Fetch {
    let hits = hits.clone();
    Arc::new(move |req: FetchRequest| {
        hits.fetch_add(1, Ordering::SeqCst);
        let answer = respond(req.url.as_str());
        Box::pin(async move {
            let (status, location, body) = answer?;
            Ok(FetchResponse {
                status,
                location: location.map(str::to_string),
                content_type: None,
                content_length: Some(body.len() as u64),
                body: futures_util::stream::once(async move { Ok(Bytes::from(body)) }).boxed(),
            })
        })
    })
}

fn station(id: &str, label: &str, target: &str, icon: Option<&str>) -> DeckAction {
    DeckAction {
        id: id.into(),
        label: label.into(),
        kind: "radio".into(),
        target: target.into(),
        icon: icon.map(str::to_string),
        ..Default::default()
    }
}

#[tokio::test]
async fn radio_logo_cache_resolves_stations_caches_raster_and_falls_back_to_monogram_svg() {
    let url = |s: &str| reqwest::Url::parse(s).unwrap();
    assert!(!is_safe_public_uri(&url("http://127.0.0.1:8765/api/state")));
    assert!(!is_safe_public_uri(&url("http://192.168.1.10/logo.png")));
    assert!(!is_safe_public_uri(&url("http://localhost/logo.png")));
    assert!(is_safe_public_uri(&url(
        "https://cdn.radyofenomen.com/logo.png"
    )));
    assert!(crate::lan::allowed_over_lan("/api/controls/radio-logo"));

    let dir = tempfile::tempdir().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let cache = RadioLogoCache::with_fetch(
        dir.path().join("logos"),
        fake(&hits, |u| {
            if u == "https://cdn.example.com/logo.png" {
                Ok((200, None, png()))
            } else {
                Ok((404, None, vec![]))
            }
        }),
    );

    // offline and no icon: no request, the monogram
    let offline = station("r0", "PowerTürk", "https://example.com/powerturk.mp3", None);
    let logo = cache.get(&offline, false).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(logo.mime, SVG);
    assert!(logo.is_fallback);

    // a configured icon is downloaded (still public-only) and cached even with lookups off
    let explicit = station(
        "r1",
        "Radyo Fenomen",
        "https://live.radyofenomen.com/fenomen/128/icecast.audio",
        Some("https://cdn.example.com/logo.png"),
    );
    let first = cache.get(&explicit, false).await;
    let second = cache.get(&explicit, false).await;
    assert_eq!(first.mime, "image/png");
    assert!(!first.is_fallback);
    assert_eq!(*first.bytes, png());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(first.bytes, second.bytes);

    // a redirect from https to http is refused: the monogram
    let hops = Arc::new(AtomicUsize::new(0));
    let downgrade_hits = Arc::new(AtomicUsize::new(0));
    let downgrade = RadioLogoCache::with_fetch(dir.path().join("logos-dg"), {
        let hops = hops.clone();
        fake(&downgrade_hits, move |u| {
            if u == "https://cdn.example.com/redirect-downgrade.png" {
                return Ok((302, Some("http://cdn.example.com/insecure.png"), vec![]));
            }
            hops.fetch_add(1, Ordering::SeqCst);
            Ok((404, None, vec![]))
        })
    });
    let action = station(
        "r-downgrade",
        "Downgrade FM",
        "https://example.com/stream.mp3",
        Some("https://cdn.example.com/redirect-downgrade.png"),
    );
    let logo = downgrade.get(&action, false).await;
    assert_eq!(logo.mime, SVG);
    assert!(logo.is_fallback);
    assert_eq!(hops.load(Ordering::SeqCst), 0);

    // a monogram of a name with an emoji, without replacement characters
    let unknown = station("r2", "📻 Jazz", "https://example.com/live.mp3", None);
    let logo = cache.get(&unknown, false).await;
    assert_eq!(logo.mime, SVG);
    assert!(logo.is_fallback);
    let svg = String::from_utf8(logo.bytes.to_vec()).unwrap();
    assert!(svg.contains("<svg"));
    assert!(!svg.contains('\u{FFFD}'));

    // a new icon is a new key (and card rev)
    let mut edited = explicit.clone();
    edited.icon = Some("https://cdn.example.com/logo-v2.png".into());
    assert_ne!(cache_key(&explicit, false), cache_key(&edited, false));
}

#[tokio::test]
async fn radio_logo_cache_falls_back_to_monogram_when_budget_timeout_expires() {
    let dir = tempfile::tempdir().unwrap();
    let hanging: Fetch = Arc::new(|_| Box::pin(std::future::pending()));
    let cache = RadioLogoCache::with_fetch(dir.path().join("logos"), hanging);
    let action = station(
        "r-slow",
        "Slow Radio",
        "https://example.com/slow.mp3",
        Some("https://cdn.example.com/hanging-logo.png"),
    );
    let logo = cache.get(&action, false).await;
    assert_eq!(logo.mime, SVG);
    assert!(logo.is_fallback);
    assert!(String::from_utf8_lossy(&logo.bytes).contains("<svg"));
}

#[test]
fn is_public_ip_address_classifies_addresses_correctly() {
    for (raw, expected) in [
        ("::ffff:127.0.0.1", false),
        ("::1", false),
        ("fe80::1", false),
        ("fd00::1", false),
        ("fc00::1", false),
        ("::", false),
        ("169.254.169.254", false),
        ("100.64.0.1", false),
        ("100.127.255.254", false),
        ("0.0.0.0", false),
        ("10.0.0.1", false),
        ("127.0.0.1", false),
        ("172.16.0.1", false),
        ("192.168.1.1", false),
        ("224.0.0.1", false),
        ("1.1.1.1", true),
        ("8.8.8.8", true),
        ("100.63.255.255", true),
        ("2606:4700:4700::1111", true),
    ] {
        assert_eq!(
            is_public_ip(raw.parse::<IpAddr>().unwrap()),
            expected,
            "{raw}"
        );
    }
}

#[tokio::test]
async fn radio_logo_cache_evicts_oldest_memory_entries_without_clearing_whole_cache() {
    let dir = tempfile::tempdir().unwrap();
    let logos = dir.path().join("logos");
    let hits = Arc::new(AtomicUsize::new(0));
    let cache = RadioLogoCache::with_fetch(logos.clone(), fake(&hits, |_| Ok((200, None, png()))));
    let station_at = |i: usize| {
        station(
            &format!("r-{i}"),
            &format!("Station {i}"),
            &format!("https://example.com/stream-{i}.mp3"),
            Some(&format!("https://cdn.example.com/logo-{i}.png")),
        )
    };
    for i in 0..130 {
        cache.get(&station_at(i), false).await;
    }
    assert_eq!(hits.load(Ordering::SeqCst), 130);

    // only memory hits avoid a request now
    for file in std::fs::read_dir(&logos).unwrap() {
        std::fs::remove_file(file.unwrap().path()).unwrap();
    }
    // the newest entry stayed in memory, the oldest was evicted
    cache.get(&station_at(129), false).await;
    assert_eq!(hits.load(Ordering::SeqCst), 130);
    cache.get(&station_at(0), false).await;
    assert_eq!(hits.load(Ordering::SeqCst), 131);
}

fn set_modified(path: &Path, at: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
}

#[tokio::test]
async fn radio_logo_cache_prunes_expired_disk_entries_on_write() {
    let dir = tempfile::tempdir().unwrap();
    let logos = dir.path().join("logos");
    std::fs::create_dir_all(&logos).unwrap();
    let expired_bin = logos.join("expiredkey.bin");
    let expired_mime = logos.join("expiredkey.mime");
    std::fs::write(&expired_bin, png()).unwrap();
    std::fs::write(&expired_mime, "image/png").unwrap();
    let old = SystemTime::now() - Duration::from_secs(15 * 24 * 3600);
    set_modified(&expired_bin, old);
    set_modified(&expired_mime, old);

    let hits = Arc::new(AtomicUsize::new(0));
    let cache = RadioLogoCache::with_fetch(logos.clone(), fake(&hits, |_| Ok((200, None, png()))));
    let fresh = station(
        "r-fresh",
        "Fresh FM",
        "https://example.com/fresh.mp3",
        Some("https://cdn.example.com/fresh.png"),
    );
    assert!(!cache.get(&fresh, false).await.is_fallback);
    assert!(!expired_bin.exists());
    assert!(!expired_mime.exists());
    let key = cache_key(&fresh, false);
    assert!(logos.join(format!("{key}.bin")).exists());
    assert!(logos.join(format!("{key}.mime")).exists());
}

#[tokio::test]
async fn punctuation_only_station_label_skips_directory_downloads_and_returns_monogram() {
    let dir = tempfile::tempdir().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let cache = RadioLogoCache::with_fetch(
        dir.path().join("logos"),
        fake(&hits, |u| {
            if u.starts_with("https://de1.api.radio-browser.info/") {
                Ok((
                    200,
                    None,
                    br#"[{"name":"Any Station","favicon":"https://cdn.example.com/rb.png"}]"#
                        .to_vec(),
                ))
            } else if u.starts_with("https://opml.radiotime.com/") {
                Ok((
                    200,
                    None,
                    br#"{"body":[{"text":"Any Station","image":"https://cdn.example.com/tunein.png"}]}"#.to_vec(),
                ))
            } else {
                Ok((200, None, png()))
            }
        }),
    );
    let action = station("r-punct", "!!📻!!", "https://example.com/s.mp3", None);
    let logo = cache.get(&action, true).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert!(logo.is_fallback);
    assert_eq!(logo.mime, SVG);
}

/// C#'s null label and target are empty strings here (the config never holds a null for them).
#[tokio::test]
async fn null_action_strings_and_future_dated_disk_logos_fall_back_safely() {
    let dir = tempfile::tempdir().unwrap();
    let logos = dir.path().join("logos");
    std::fs::create_dir_all(&logos).unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let cache = RadioLogoCache::with_fetch(logos.clone(), fake(&hits, |_| Ok((404, None, vec![]))));

    let empty = station("r-null", "", "", None);
    let logo = cache.get(&empty, true).await;
    assert!(logo.is_fallback);
    assert_eq!(logo.mime, SVG);

    let ok = station("r-ok", "Jazz FM", "https://example.com/jazz.mp3", None);
    let key = cache_key(&ok, false);
    let bin = logos.join(format!("{key}.bin"));
    std::fs::write(&bin, png()).unwrap();
    std::fs::write(logos.join(format!("{key}.mime")), "image/png").unwrap();
    set_modified(&bin, SystemTime::now() + Duration::from_secs(600));
    let future = cache.get(&ok, false).await;
    assert!(future.is_fallback);
    assert!(!bin.exists());
}

// The monogram, the cache key and the name helpers against the C# agent's own output (RadioLogos.cs run under .NET 10).

#[test]
fn the_monogram_svg_is_byte_for_byte_the_csharp_one() {
    let engine = base64::engine::general_purpose::STANDARD;
    let fixture = include_str!("testdata/monogram-csharp.txt");
    let mut checked = 0;
    for line in fixture
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let (label, svg) = line.split_once(' ').unwrap();
        let label = if label == "-" {
            String::new()
        } else {
            String::from_utf8(engine.decode(label).unwrap()).unwrap()
        };
        let expected = engine.decode(svg).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&monogram_svg(&label)),
            String::from_utf8_lossy(&expected),
            "{label:?}"
        );
        checked += 1;
    }
    assert_eq!(checked, 7);
}

#[test]
fn cache_keys_and_name_helpers_match_the_csharp_ones() {
    let mut a = station("x", " Jazz FM ", "https://x/y", None);
    assert_eq!(cache_key(&a, false), "73d9ad00cc75b810704217e4");
    a.label = "Jazz FM".into();
    a.icon = Some(" https://i/l.png ".into());
    assert_eq!(cache_key(&a, true), "170cbbe6b71ff5a4c1f20f8a");
    assert_eq!(
        normalize_key("Ünlü Şarkı Çağ Âşık Îlk İ I 9"),
        "unlusarkicagasikilkii9"
    );
    assert_eq!(
        clean_search_name("Power FM (Turkey)  (128k) Live"),
        "Power FM  Live"
    );
    assert_eq!(
        detect_raster_mime(b"RIFF\0\0\0\0WEBPxxxx"),
        Some("image/webp")
    );
    assert_eq!(detect_raster_mime(b"GIF89a"), Some("image/gif"));
    assert_eq!(detect_raster_mime(&[0, 0, 1, 0]), Some("image/x-icon"));
    assert_eq!(detect_raster_mime(b"<svg"), None);
}
