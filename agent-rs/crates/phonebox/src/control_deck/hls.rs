//! `ControlDeck.Hls.cs`, the portable part: telling an HLS radio stream (`.m3u8`, or a manifest behind another
//! address) from a plain one, which the Windows player opens differently, and matching a Windows audio endpoint to a
//! device interface path. The probe reads at most 512 bytes, so an endless audio stream or a stalled body never holds
//! it.

use std::time::Duration;

use futures_util::StreamExt;

use super::fetch::{Fetch, FetchRequest};

/// How long a probe may take by default.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
const PREFIX: usize = 512;

/// An http(s) address whose path ends in `.m3u8` (the query does not count).
pub fn is_hls_url(url: &reqwest::Url) -> bool {
    matches!(url.scheme(), "http" | "https") && url.path().to_ascii_lowercase().ends_with(".m3u8")
}

/// The two HLS media types, in any case.
pub fn is_hls_content_type(media_type: Option<&str>) -> bool {
    media_type.is_some_and(|t| {
        t.eq_ignore_ascii_case("application/vnd.apple.mpegurl")
            || t.eq_ignore_ascii_case("application/x-mpegurl")
    })
}

/// The text without byte-order marks and leading white space.
fn trim_start(text: &str) -> &str {
    text.trim_start_matches('\u{FEFF}').trim_start()
}

/// An HLS manifest (a media or a master playlist), not a classic `.m3u` list of stream addresses.
pub fn looks_like_hls_manifest(text: &str) -> bool {
    let trimmed = trim_start(text);
    trimmed.starts_with("#EXTM3U")
        && (trimmed.contains("#EXT-X-TARGETDURATION:") || trimmed.contains("#EXT-X-STREAM-INF:"))
}

/// Whether a device interface path (`\\?\SWD#MMDEVAPI#<id>#{…}`) is the Core Audio endpoint `mm_device_id`.
pub fn matches_audio_render_device(interface_id: Option<&str>, mm_device_id: Option<&str>) -> bool {
    let (Some(interface_id), Some(target)) = (
        interface_id.filter(|s| !s.trim().is_empty()),
        mm_device_id.filter(|s| !s.trim().is_empty()),
    ) else {
        return false;
    };
    let (interface_id, target) = (interface_id.to_uppercase(), target.trim().to_uppercase());
    interface_id == target || interface_id.contains(&format!("#{target}#"))
}

/// Whether an http(s) address serves an HLS manifest, from the first 512 bytes of its answer within `timeout`.
pub async fn probe_manifest(url: &reqwest::Url, fetch: &Fetch, timeout: Duration) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let probe = async {
        let res = fetch(FetchRequest::get(url.clone())).await.ok()?;
        if !(200..=299).contains(&res.status) {
            return None;
        }
        let media_type = res
            .content_type
            .as_deref()
            .and_then(|t| t.split(';').next())
            .map(|t| t.trim().to_string());
        let mut prefix = Vec::with_capacity(PREFIX);
        let mut body = res.body;
        while prefix.len() < PREFIX {
            match body.next().await {
                Some(Ok(chunk)) => {
                    let room = PREFIX - prefix.len();
                    prefix.extend_from_slice(&chunk[..chunk.len().min(room)]);
                }
                Some(Err(_)) => return None,
                None => break,
            }
        }
        let text = String::from_utf8_lossy(&prefix).into_owned();
        Some(
            looks_like_hls_manifest(&text)
                || (is_hls_content_type(media_type.as_deref())
                    && trim_start(&text).starts_with("#EXTM3U")
                    && text.contains("#EXT-X-")),
        )
    };
    matches!(tokio::time::timeout(timeout, probe).await, Ok(Some(true)))
}

/// The probe's own client: redirects followed, the system proxy, 5 s to connect, 6 s per exchange,
/// `User-Agent: PhoneBox/1.0`.
pub fn hls_fetch() -> Fetch {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(6))
        .user_agent("PhoneBox/1.0")
        .build()
        .expect("the HLS client");
    std::sync::Arc::new(move |req: FetchRequest| {
        let client = client.clone();
        Box::pin(async move {
            let res = client
                .get(req.url)
                .send()
                .await
                .map_err(|e| super::fetch::FetchError::Other(e.to_string()))?;
            let content_type = res
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            Ok(super::fetch::FetchResponse {
                status: res.status().as_u16(),
                location: None,
                content_type,
                content_length: res.content_length(),
                body: res
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(|e| super::fetch::FetchError::Other(e.to_string())))
                    .boxed(),
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Bytes;

    use super::*;
    use crate::control_deck::fetch::FetchResponse;

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    /// `HlsRadioTests.FakeHandler`: three playlists by path, and an endless audio stream for anything else.
    fn playlists() -> Fetch {
        Arc::new(|req: FetchRequest| {
            let text = |body: &'static str, kind: &'static str| (Some(kind), Some(body));
            let (kind, body) = match req.url.path() {
                "/live-hls" => text(
                    "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n",
                    "application/octet-stream",
                ),
                "/master-hls" => text(
                    "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=96000\nlive.m3u8\n",
                    "application/vnd.apple.mpegurl",
                ),
                "/classic-m3u" => text(
                    "#EXTM3U\n#EXTINF:-1,Station\nhttps://radio.example/stream.mp3\n",
                    "audio/x-mpegurl",
                ),
                _ => (None, None),
            };
            Box::pin(async move {
                let body = match body {
                    Some(body) => {
                        futures_util::stream::once(async move { Ok(Bytes::from(body)) }).boxed()
                    }
                    // an endless audio stream
                    None => {
                        futures_util::stream::repeat_with(|| Ok(Bytes::from(vec![0xFFu8; 4096])))
                            .boxed()
                    }
                };
                Ok(FetchResponse {
                    status: 200,
                    location: None,
                    content_type: kind.map(|k| format!("{k}; charset=utf-8")),
                    content_length: None,
                    body,
                })
            })
        })
    }

    #[tokio::test]
    async fn detects_hls_urls_with_query_strings_and_probes_extensionless_manifests_without_reading_infinite_streams()
     {
        assert!(is_hls_url(&url(
            "https://stream.radyotvonline.net/hls/play/kafaradyo.m3u8?UserCode=123"
        )));
        assert!(!is_hls_url(&url(
            "https://live.radyofenomen.com/fenomen/128/listen.m3u"
        )));
        assert!(!is_hls_url(&url(
            "https://live.radyofenomen.com/fenomen/128/icecast.audio"
        )));
        assert!(!is_hls_url(&url("file:///tmp/test.m3u8")));

        assert!(looks_like_hls_manifest(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\nseg0.ts\n"
        ));
        assert!(looks_like_hls_manifest(
            "\u{FEFF}#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000\naudio.m3u8\n"
        ));
        assert!(!looks_like_hls_manifest(
            "#EXTM3U\n#EXTINF:-1,Classic Radio\nhttps://radio.example/stream.mp3\n"
        ));

        let fetch = playlists();
        assert!(
            probe_manifest(
                &url("https://radio.example/live-hls"),
                &fetch,
                PROBE_TIMEOUT
            )
            .await
        );
        assert!(
            probe_manifest(
                &url("https://radio.example/master-hls"),
                &fetch,
                PROBE_TIMEOUT
            )
            .await
        );
        assert!(
            !probe_manifest(
                &url("https://radio.example/classic-m3u"),
                &fetch,
                PROBE_TIMEOUT
            )
            .await
        );
        assert!(
            !probe_manifest(
                &url("https://radio.example/infinite-icecast"),
                &fetch,
                PROBE_TIMEOUT
            )
            .await
        );
    }

    #[tokio::test]
    async fn probe_manifest_aborts_when_response_body_stalls_after_headers() {
        let stalled: Fetch = Arc::new(|_| {
            Box::pin(async {
                Ok(FetchResponse {
                    status: 200,
                    location: None,
                    content_type: None,
                    content_length: None,
                    body: futures_util::stream::pending().boxed(),
                })
            })
        });
        let started = std::time::Instant::now();
        let ok = probe_manifest(
            &url("https://radio.example/stalled"),
            &stalled,
            Duration::from_millis(150),
        )
        .await;
        assert!(!ok);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn matches_audio_render_device_matches_core_audio_ids_against_swd_interface_paths() {
        let id = "{0.0.0.00000000}.{11111111-2222-3333-4444-555555555555}";
        for (interface, device, expected) in [
            (
                Some(
                    r"\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{11111111-2222-3333-4444-555555555555}#{e6327cad-dcec-4949-ae8a-991e976a79d2}",
                ),
                id,
                true,
            ),
            (
                Some(
                    r"\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}#{e6327cad-dcec-4949-ae8a-991e976a79d2}",
                ),
                "{0.0.0.00000000}.{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}",
                true,
            ),
            (Some(id), id, true),
            (
                Some(
                    r"\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{99999999-2222-3333-4444-555555555555}#{e6327cad-dcec-4949-ae8a-991e976a79d2}",
                ),
                id,
                false,
            ),
            (None, id, false),
        ] {
            assert_eq!(
                matches_audio_render_device(interface, Some(device)),
                expected,
                "{interface:?}"
            );
        }
    }
}
