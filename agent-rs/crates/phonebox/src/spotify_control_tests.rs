//! `SpotifyControlTests.cs`, `SpotifyCredentialTests.cs` and `DeckAuditTests.SpotifyTimeout…`: a fake HTTP seam
//! stands in for C#'s `HttpMessageHandler`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::{Value, json};

use super::*;

type Reply = Result<SpotifyReply, String>;

fn http<F, Fut>(f: F) -> SpotifyHttp
where
    F: Fn(SpotifyRequest) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Reply> + Send + 'static,
{
    Arc::new(move |req| f(req).boxed())
}

fn reply(status: u16, body: &str) -> Reply {
    Ok(SpotifyReply {
        status,
        retry_after: None,
        body: body.into(),
    })
}

fn limited(seconds: u32) -> Reply {
    Ok(SpotifyReply {
        status: 429,
        retry_after: Some(seconds.to_string()),
        body: "{}".into(),
    })
}

fn playing(name: &str) -> Reply {
    reply(
        200,
        &json!({"is_playing": true, "item": {"uri": "spotify:track:1234567890123456789012", "name": name,
            "artists": []}})
        .to_string(),
    )
}

fn host(url: &str) -> &str {
    let rest = url.strip_prefix("https://").unwrap_or(url);
    &rest[..rest.find('/').unwrap_or(rest.len())]
}

fn path_and_query(url: &str) -> &str {
    let rest = url.strip_prefix("https://").unwrap_or(url);
    &rest[rest.find('/').unwrap_or(rest.len())..]
}

fn path(url: &str) -> &str {
    let pq = path_and_query(url);
    &pq[..pq.find('?').unwrap_or(pq.len())]
}

fn query_param(url: &str, name: &str) -> String {
    let query = &url[url.find('?').map_or(url.len(), |i| i + 1)..];
    query
        .split('&')
        .find_map(|p| p.strip_prefix(&format!("{name}=")))
        .map(urlencoding_decode)
        .unwrap_or_default()
}

fn urlencoding_decode(v: &str) -> String {
    let bytes = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push(u8::from_str_radix(&v[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

struct Fixture {
    _dir: tempfile::TempDir,
    cfg: Arc<ConfigStore>,
}

impl Fixture {
    fn new(manual: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let token = if manual {
            json!("manual-token")
        } else {
            Value::Null
        };
        std::fs::write(
            &path,
            json!({"controls": {"spotifyClientId": "test-client", "spotifyToken": token}})
                .to_string(),
        )
        .unwrap();
        Fixture {
            cfg: Arc::new(ConfigStore::new(path)),
            _dir: dir,
        }
    }

    fn empty() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        Fixture {
            cfg: Arc::new(ConfigStore::new(path)),
            _dir: dir,
        }
    }

    fn create(&self, http: SpotifyHttp) -> Arc<SpotifyControl> {
        Arc::new(SpotifyControl::new(self.cfg.clone(), 19885, http))
    }

    fn configure(&self, client: &str, manual: Option<&str>) {
        self.cfg
            .save(
                &json!({"controls": {"spotifyClientId": client, "spotifyToken": manual}})
                    .to_string(),
                None,
            )
            .unwrap();
    }
}

fn state_of(link: &str) -> String {
    query_param(link, "state")
}

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("finished in time")
}

#[test]
fn tracks_carry_the_smallest_usable_cover_from_spotifys_image_server() {
    let item = json!({"uri": "spotify:track:0000000000000000000001", "name": "Song",
        "artists": [{"name": "A"}, {"name": "B"}],
        "album": {"images": [{"url": "https://i.scdn.co/image/big640", "width": 640},
            {"url": "https://i.scdn.co/image/mid300", "width": 300}, {"url": "https://i.scdn.co/image/tiny32", "width": 32}]},
        "external_urls": {"spotify": "https://open.spotify.com/x"}});
    let track = track_of(Some(&item));
    assert_eq!(track["artist"], "A, B");
    assert_eq!(track["image"], "https://i.scdn.co/image/mid300");
    // only id, name, artist and cover reach the phone
    assert_eq!(
        track,
        json!({"id": "spotify:track:0000000000000000000001", "name": "Song", "artist": "A, B",
            "image": "https://i.scdn.co/image/mid300"})
    );
}

#[tokio::test]
async fn the_playing_episode_and_queued_episodes_are_asked_for() {
    let fixture = Fixture::new(true);
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = asked.clone();
    let spotify = fixture.create(http(move |r| {
        seen.lock().unwrap().push(path_and_query(&r.url).to_string());
        let queue = path(&r.url).ends_with("/queue");
        async move {
            if queue {
                reply(200, r#"{"queue":[]}"#)
            } else {
                reply(
                    200,
                    r#"{"is_playing":true,"item":{"uri":"spotify:episode:0000000000000000000002","name":"Ep","show":{"name":"Show"},"images":[]}}"#,
                )
            }
        }
    }));
    let snap = spotify.snapshot().await;
    assert_eq!(snap["current"]["name"], "Ep");
    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert!(
        asked
            .iter()
            .all(|p| p.contains("additional_types=track,episode")),
        "{asked:?}"
    );
}

#[test]
fn covers_from_any_other_host_are_dropped() {
    let item = json!({"uri": "spotify:track:0000000000000000000001", "name": "Song", "artists": [],
        "album": {"images": [{"url": "https://evil.example/i.scdn.co/image/x", "width": 300},
            {"url": "http://i.scdn.co/image/abc", "width": 300}, {"url": "https://i.scdn.co/image/abc?x=1", "width": 300}]}});
    assert!(track_of(Some(&item))["image"].is_null());
}

#[test]
fn episodes_use_the_show_name_and_their_own_images() {
    let item = json!({"uri": "spotify:episode:0000000000000000000002", "name": "Ep 1", "show": {"name": "The Show"},
        "images": [{"url": "https://i.scdn.co/image/ep64", "width": 64}]});
    let episode = track_of(Some(&item));
    assert_eq!(episode["artist"], "The Show");
    assert_eq!(episode["image"], "https://i.scdn.co/image/ep64");
}

#[tokio::test]
async fn pkce_uses_single_use_state_and_does_not_write_tokens_into_settings() {
    let fixture = Fixture::new(false);
    let calls = Arc::new(AtomicUsize::new(0));
    let token_body = Arc::new(Mutex::new(String::new()));
    let (c, b) = (calls.clone(), token_body.clone());
    let spotify = fixture.create(http(move |r| {
        c.fetch_add(1, Ordering::SeqCst);
        assert_eq!(host(&r.url), "accounts.spotify.com");
        assert_eq!(r.content_type, Some("application/x-www-form-urlencoded"));
        *b.lock().unwrap() = r.body.clone().unwrap_or_default();
        async { reply(200, r#"{"access_token":"secret-access","refresh_token":"secret-refresh","expires_in":3600}"#) }
    }));
    let link = spotify.begin().unwrap();
    assert!(link.starts_with(
        "https://accounts.spotify.com/authorize?client_id=test-client&response_type=code&"
    ));
    assert_eq!(query_param(&link, "code_challenge_method"), "S256");
    assert_eq!(query_param(&link, "code_challenge").len(), 43);
    assert_eq!(
        query_param(&link, "redirect_uri"),
        "http://127.0.0.1:19885/api/controls/spotify/callback"
    );
    assert_eq!(
        query_param(&link, "scope"),
        "user-read-playback-state user-read-currently-playing user-modify-playback-state user-library-modify"
    );
    let state = state_of(&link);
    assert!(!spotify.complete("wrong-state", "code").await.unwrap());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(spotify.complete(&state, "code").await.unwrap());
    assert!(token_body.lock().unwrap().contains("code_verifier="));
    assert!(!spotify.has_pending_state(Some(&state)));
    assert!(!spotify.complete(&state, "code").await.unwrap());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        !std::fs::read_to_string(fixture.cfg.path())
            .unwrap()
            .contains("secret-access")
    );
    let public = crate::config::public::to_public(&fixture.cfg.get(), None, None);
    assert!(!public.to_string().contains("secret-refresh"));
}

#[tokio::test]
async fn starting_another_sign_in_invalidates_the_previous_state() {
    let fixture = Fixture::new(false);
    let spotify = fixture.create(http(|_| async { panic!("No token exchange expected") }));
    let first = state_of(&spotify.begin().unwrap());
    let second = state_of(&spotify.begin().unwrap());
    assert!(!spotify.has_pending_state(Some(&first)));
    assert!(spotify.has_pending_state(Some(&second)));
    spotify.disconnect().await.unwrap();
    assert!(!spotify.has_pending_state(Some(&second)));
}

#[tokio::test]
async fn disconnect_persists_across_restart_and_disables_manual_token_fallback() {
    let fixture = Fixture::new(true);
    let calls = Arc::new(AtomicUsize::new(0));
    let handler = |calls: Arc<AtomicUsize>| {
        http(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { reply(200, "{}") }
        })
    };
    fixture
        .create(handler(calls.clone()))
        .disconnect()
        .await
        .unwrap();
    let restarted = fixture.create(handler(calls.clone()));
    assert_eq!(restarted.snapshot().await["connected"], false);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn queue_commands_reject_unobserved_items_and_save_uses_current_item() {
    let fixture = Fixture::new(true);
    let paths = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = paths.clone();
    let spotify = fixture.create(http(move |r| {
        seen.lock().unwrap().push(format!("{} {}", r.method, path_and_query(&r.url)));
        let (put, queue) = (r.method == "PUT", path(&r.url).ends_with("queue"));
        async move {
            if put {
                reply(204, "")
            } else if queue {
                reply(200, r#"{"queue":[{"uri":"spotify:track:abcdefghijklmnopqrstuv","name":"Queued","artists":[]}]}"#)
            } else {
                reply(
                    200,
                    r#"{"item":{"uri":"spotify:track:1234567890123456789012","name":"Current","artists":[]},"is_playing":true}"#,
                )
            }
        }
    }));
    let queued = spotify
        .act("play-queued", Some("spotify:track:0000000000000000000000"))
        .await;
    assert!(!queued.ok);
    assert_eq!(
        queued.error.as_deref(),
        Some("That queue item is no longer available")
    );
    assert!(!paths.lock().unwrap().iter().any(|p| p.starts_with("PUT")));
    assert!(spotify.act("favorite", None).await.ok);
    assert!(
        paths
            .lock()
            .unwrap()
            .iter()
            .any(|p| p
                .starts_with("PUT /v1/me/library?uris=spotify%3Atrack%3A1234567890123456789012")),
        "{:?}",
        paths.lock().unwrap()
    );
}

#[tokio::test]
async fn rate_limit_suppresses_repeated_http_requests() {
    let fixture = Fixture::new(true);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let spotify = fixture.create(http(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
        async { limited(30) }
    }));
    assert_eq!(
        spotify.snapshot().await,
        json!({"connected": false, "error": "Spotify is rate limited; try again later"})
    );
    assert!(!spotify.act("favorite", None).await.ok);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn playback_targets_are_spotify_uris_only() {
    for (uri, valid) in [
        ("spotify:track:1234567890123456789012", true),
        ("https://example.com/", false),
        ("spotify:playlist:../../token", false),
    ] {
        assert_eq!(playback_uri(uri), valid, "{uri}");
    }
}

#[tokio::test]
async fn client_changes_invalidate_snapshot_reconnect_clears_rate_limit_and_empty_refresh_preserves_token()
 {
    let fixture = Fixture::new(true);
    let rate_limit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let token_calls = Arc::new(AtomicUsize::new(0));
    let refreshes = Arc::new(Mutex::new(Vec::<String>::new()));
    let (limit, calls, bodies) = (rate_limit.clone(), token_calls.clone(), refreshes.clone());
    let spotify = fixture.create(http(move |r| {
        let answer = if host(&r.url) == "accounts.spotify.com" {
            bodies.lock().unwrap().push(r.body.clone().unwrap_or_default());
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                reply(200, r#"{"access_token":"a1","refresh_token":"keep-refresh","expires_in":30}"#)
            } else {
                reply(200, r#"{"access_token":"a2","refresh_token":"","expires_in":30}"#)
            }
        } else if limit.load(Ordering::SeqCst) {
            limited(120)
        } else if path(&r.url).ends_with("/queue") {
            reply(200, r#"{"queue":[]}"#)
        } else {
            reply(200, r#"{"is_playing":true,"item":{"uri":"spotify:track:1111111111111111111111","name":"Song","artists":[]}}"#)
        };
        async move { answer }
    }));
    assert_eq!(spotify.snapshot().await["connected"], true);

    // changing the client id invalidates the cached snapshot at once
    fixture.configure("other-client", None);
    assert_eq!(spotify.snapshot().await["connected"], false);

    // a reconnect clears the rate limit, and an empty refresh_token keeps the one saved before
    fixture.configure("pkce-client", Some("temp-tok"));
    rate_limit.store(true, Ordering::SeqCst);
    assert_eq!(spotify.snapshot().await["connected"], false);
    rate_limit.store(false, Ordering::SeqCst);
    spotify.disconnect().await.unwrap();
    fixture.configure("pkce-client", None);
    let state = state_of(&spotify.begin().unwrap());
    assert!(spotify.complete(&state, "code").await.unwrap());
    assert_eq!(spotify.snapshot().await["connected"], true);
    assert_eq!(token_calls.load(Ordering::SeqCst), 3); // 1 Complete + 2 refreshes
    let refreshes = refreshes.lock().unwrap();
    assert!(
        refreshes[2].contains("refresh_token=keep-refresh"),
        "{refreshes:?}"
    );
}

#[tokio::test]
async fn a_rotated_manual_token_replaces_the_old_one_when_another_clients_credential_file_exists() {
    let fixture = Fixture::empty();
    fixture.configure("client-b", Some("manual-1"));
    let mut file = fixture.cfg.path().as_os_str().to_owned();
    file.push(".spotify.json");
    std::fs::write(
        &file,
        json!({"ClientId": "client-a", "Access": "file-a", "Refresh": "refresh-a", "Expires": now() + 3_600_000})
            .to_string(),
    )
    .unwrap();
    let bearers = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = bearers.clone();
    let spotify = fixture.create(http(move |r| {
        seen.lock()
            .unwrap()
            .push(r.bearer.clone().unwrap_or_default());
        let queue = path(&r.url).ends_with("/queue");
        async move {
            if queue {
                reply(200, r#"{"queue":[]}"#)
            } else {
                playing("Song")
            }
        }
    }));
    assert_eq!(within(spotify.snapshot()).await["connected"], true);
    assert!(bearers.lock().unwrap().iter().all(|b| b == "manual-1"));
    bearers.lock().unwrap().clear();

    fixture.configure("client-b", Some("manual-2"));
    assert_eq!(within(spotify.snapshot()).await["connected"], true);
    let bearers = bearers.lock().unwrap();
    assert!(!bearers.is_empty());
    assert!(bearers.iter().all(|b| b == "manual-2"), "{bearers:?}");
}

#[tokio::test]
async fn a_settings_change_during_a_snapshot_does_not_mix_two_accounts() {
    let fixture = Fixture::empty();
    fixture.configure("client", Some("manual-1"));
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, released) = tokio::sync::watch::channel(false);
    let e = entered.clone();
    let spotify = fixture.create(http(move |r| {
        let (e, mut released) = (e.clone(), released.clone());
        async move {
            if path(&r.url).ends_with("/queue") {
                return reply(200, r#"{"queue":[]}"#);
            }
            if r.bearer.as_deref() != Some("manual-1") {
                return playing("New account");
            }
            e.notify_one();
            let _ = released.wait_for(|r| *r).await;
            playing("Old account")
        }
    }));
    let snapshot = tokio::spawn({
        let spotify = spotify.clone();
        async move { spotify.snapshot().await }
    });
    within(entered.notified()).await;
    fixture.configure("client", Some("manual-2"));
    release.send(true).unwrap();
    let snap = within(snapshot).await.unwrap();
    assert_eq!(snap["current"]["name"], "New account");
}

#[tokio::test]
async fn a_disconnect_during_a_snapshot_is_not_undone_by_its_answer() {
    let fixture = Fixture::empty();
    fixture.configure("client", Some("manual-1"));
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, released) = tokio::sync::watch::channel(false);
    let e = entered.clone();
    let spotify = fixture.create(http(move |r| {
        let (e, mut released) = (e.clone(), released.clone());
        async move {
            if !path(&r.url).ends_with("/queue") {
                return playing("Song");
            }
            e.notify_one();
            let _ = released.wait_for(|r| *r).await;
            reply(200, r#"{"queue":[]}"#)
        }
    }));
    let snapshot = tokio::spawn({
        let spotify = spotify.clone();
        async move { spotify.snapshot().await }
    });
    within(entered.notified()).await;
    within(spotify.disconnect()).await.unwrap();
    release.send(true).unwrap();
    assert_eq!(within(snapshot).await.unwrap()["connected"], false);
    assert_eq!(within(spotify.snapshot()).await["connected"], false);
}

#[tokio::test]
async fn a_rate_limit_answered_to_the_previous_account_does_not_block_the_new_connection() {
    let fixture = Fixture::empty();
    fixture.configure("client", Some("manual-1"));
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, released) = tokio::sync::watch::channel(false);
    let e = entered.clone();
    let spotify = fixture.create(http(move |r| {
        let (e, mut released) = (e.clone(), released.clone());
        async move {
            if host(&r.url) == "accounts.spotify.com" {
                return reply(
                    200,
                    r#"{"access_token":"fresh","refresh_token":"fresh-refresh","expires_in":3600}"#,
                );
            }
            if r.bearer.as_deref() == Some("manual-1") {
                e.notify_one();
                let _ = released.wait_for(|r| *r).await;
                return limited(120);
            }
            if r.method == "PUT" {
                return reply(204, "");
            }
            if path(&r.url).ends_with("/queue") {
                reply(200, r#"{"queue":[]}"#)
            } else {
                playing("Song")
            }
        }
    }));
    let snapshot = tokio::spawn({
        let spotify = spotify.clone();
        async move { spotify.snapshot().await }
    });
    within(entered.notified()).await;
    let state = state_of(&spotify.begin().unwrap());
    assert!(within(spotify.complete(&state, "code")).await.unwrap());
    release.send(true).unwrap();
    within(snapshot).await.unwrap();
    let favorite = within(spotify.act("favorite", None)).await;
    assert!(favorite.ok, "{:?}", favorite.error);
}

#[tokio::test]
async fn spotify_timeout_is_reported_as_unavailable_instead_of_throwing() {
    let fixture = Fixture::new(true);
    let spotify = SpotifyControl::new(
        fixture.cfg.clone(),
        19885,
        http(|_| futures_util::future::pending::<Reply>()),
    )
    .with_timeout(Duration::from_millis(200));
    assert_eq!(
        spotify.snapshot().await,
        json!({"connected": false, "error": "Spotify is unavailable"})
    );
    assert!(!spotify.act("favorite", None).await.ok);
}

#[test]
fn a_token_answer_with_an_odd_lifetime_or_no_new_refresh_keeps_sane_values() {
    // expires_in clamps to 30..=86400 s (3600 when it is not an int); an absent or empty refresh_token keeps the old one
    let at = 1_000_000;
    for (answer, refresh, expires) in [
        (r#"{"access_token":"a","expires_in":5}"#, "old", at + 30_000),
        (
            r#"{"access_token":"a","expires_in":999999}"#,
            "old",
            at + 86_400_000,
        ),
        (
            r#"{"access_token":"a","expires_in":"60"}"#,
            "old",
            at + 3_600_000,
        ),
        (
            r#"{"access_token":"a","expires_in":60.5,"refresh_token":""}"#,
            "old",
            at + 3_600_000,
        ),
        (
            r#"{"access_token":"a","refresh_token":"new"}"#,
            "new",
            at + 3_600_000,
        ),
    ] {
        let token = token_of(answer, "c", "old", at).unwrap();
        assert_eq!(
            (token.refresh.as_str(), token.expires),
            (refresh, expires),
            "{answer}"
        );
    }
    assert_eq!(
        token_of(r#"{"access_token":"  "}"#, "c", "", at),
        Err(Fail::Io("Spotify returned no access token".into()))
    );
    assert_eq!(
        token_of(r#"{"access_token":5}"#, "c", "", at),
        Err(Fail::Io("Spotify returned no access token".into()))
    );
    assert_eq!(token_of("not json", "c", "", at), Err(Fail::Unavailable));
}

#[test]
fn retry_after_is_whole_seconds_clamped_to_one_to_300() {
    for (header, ms) in [
        (Some("30"), 30_000),
        (Some("0"), 1_000),
        (Some("100000"), 300_000),
        (Some("+5"), 30_000),
        (Some("-5"), 30_000),
        (Some("Wed, 21 Oct 2015 07:28:00 GMT"), 30_000),
        (None, 30_000),
    ] {
        assert_eq!(retry_after_ms(header), ms, "{header:?}");
    }
}
