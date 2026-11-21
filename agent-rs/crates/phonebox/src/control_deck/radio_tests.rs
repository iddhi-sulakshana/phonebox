//! The radio `ControlDeckTests` and `RadioCardViewsTests.TapToToggleStartsAndStopsRadioWithoutSeparateStopAction`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;

use super::super::*;
use crate::config::store::ConfigStore;
use crate::control_deck::testing::{Event, lock, store, wait_for};
use crate::media_monitor::MediaSnapshot;

type PauseHook = Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;
type SoundStopped = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
struct Radios {
    playing: Vec<String>,
    failing: HashSet<String>,
}

#[derive(Default)]
struct Media {
    sessions: Vec<DeckMedia>,
    current: i32,
    paused: Vec<usize>,
    queries: usize,
    direct: bool,
}

/// `ControlDeckTests.FakeRadioDeckNative`: stations "play" in a set, a play can block or fail, media sessions are a list
/// with a current one.
#[derive(Default)]
struct FakeRadio {
    /// C#'s `_gate`: held while a station opens (even while it is blocked) and while audio is read.
    radios: Mutex<Radios>,
    media: Mutex<Media>,
    on_pause: Mutex<Option<PauseHook>>,
    block_play: Mutex<Option<Arc<Event>>>,
    play_entered: Arc<Event>,
    sound_stopped: Mutex<Option<SoundStopped>>,
}

impl FakeRadio {
    fn new() -> Arc<FakeRadio> {
        let fake = FakeRadio::default();
        lock(&fake.media).current = -1;
        Arc::new(fake)
    }
    fn playing(&self) -> Vec<String> {
        lock(&self.radios).playing.clone()
    }
    fn fail(&self, id: &str) {
        lock(&self.radios).failing.insert(id.into());
    }
    fn set_sessions(&self, sessions: Vec<DeckMedia>, current: i32) {
        let mut media = lock(&self.media);
        media.sessions = sessions;
        media.current = current;
    }
    fn session_playing(&self, index: usize) -> bool {
        lock(&self.media).sessions[index].playing
    }
    fn callback(&self) -> Option<SoundStopped> {
        lock(&self.sound_stopped).clone()
    }
    fn simulate_stream_ended(&self, id: &str, invoke_callback: bool) {
        lock(&self.radios).playing.retain(|p| p != id);
        if invoke_callback && let Some(callback) = self.callback() {
            callback(id);
        }
    }
    fn fire_stale_sound_stopped(&self, id: &str) {
        if let Some(callback) = self.callback() {
            callback(id);
        }
    }
    fn on_pause(&self, hook: PauseHook) {
        *lock(&self.on_pause) = Some(hook);
    }
    fn resolve(media: &Media, id: &str) -> Option<usize> {
        let current = usize::try_from(media.current).ok();
        match current {
            Some(i) if i < media.sessions.len() && media.sessions[i].id == id => Some(i),
            _ => media.sessions.iter().position(|m| m.id == id),
        }
    }
    fn media_action_now(&self, id: &str, action: &str) -> bool {
        let mut media = lock(&self.media);
        let Some(i) = FakeRadio::resolve(&media, id) else {
            return false;
        };
        match action {
            "pause" => {
                media.paused.push(i);
                media.sessions[i].playing = false;
                true
            }
            "playpause" => {
                media.sessions[i].playing = !media.sessions[i].playing;
                true
            }
            "next" | "prev" | "seek" | "shuffle" | "repeat" => true,
            _ => false,
        }
    }
}

fn player(id: &str, name: &str, title: &str, playing: bool) -> DeckMedia {
    DeckMedia {
        id: id.into(),
        name: name.into(),
        title: title.into(),
        playing,
        position: 0,
        duration: 100_000,
        can_seek: true,
        can_shuffle: false,
        can_repeat: false,
        shuffle: false,
        repeat: "None".into(),
    }
}

impl DeckNative for FakeRadio {
    fn on_sound_stopped(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        *lock(&self.sound_stopped) = Some(callback);
    }
    fn audio(&self) -> DeckAudio {
        let _gate = lock(&self.radios);
        DeckAudio {
            available: true,
            error: None,
            devices: vec![],
            sessions: vec![],
            defaults: None,
        }
    }
    fn device(&self, _: &str, _: Option<i32>, _: Option<bool>, _: Option<i32>) -> bool {
        true
    }
    fn session(&self, _: &str, _: Option<i32>, _: Option<bool>) -> bool {
        true
    }
    fn default_device(&self, _: &str, _: Option<i32>) -> bool {
        true
    }
    fn monitors(&self) -> Vec<DeckMonitor> {
        vec![]
    }
    fn brightness(&self, _: &str, _: i32) -> bool {
        false
    }
    fn power_plans(&self) -> Vec<DeckItem> {
        vec![]
    }
    fn platform_action<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Option<&'a str>,
        _: i32,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    fn action_state(&self, action: &crate::config::DeckAction) -> Option<String> {
        if !matches!(action.kind.as_str(), "sound" | "radio") {
            return None;
        }
        let playing = lock(&self.radios).playing.contains(&action.id);
        Some(if playing { "Playing" } else { "Stopped" }.into())
    }
    fn media(&self) -> BoxFuture<'_, Result<Vec<DeckMedia>, String>> {
        let mut media = lock(&self.media);
        media.queries += 1;
        let sessions = media.sessions.clone();
        Box::pin(async move { Ok(sessions) })
    }
    fn media_action<'a>(&'a self, id: &'a str, action: &'a str, _: i64) -> BoxFuture<'a, bool> {
        let ok = self.media_action_now(id, action);
        Box::pin(async move { ok })
    }
    fn media_action_with_state<'a>(
        &'a self,
        id: &'a str,
        action: &'a str,
        value: i64,
    ) -> BoxFuture<'a, (bool, bool)> {
        Box::pin(async move {
            let direct = lock(&self.media).direct;
            if !direct {
                let was_playing = action == "playpause"
                    && self
                        .media()
                        .await
                        .unwrap()
                        .iter()
                        .find(|p| p.id == id)
                        .is_some_and(|p| p.playing);
                return (self.media_action(id, action, value).await, was_playing);
            }
            let selected = {
                let media = lock(&self.media);
                FakeRadio::resolve(&media, id).map(|i| media.sessions[i].playing)
            };
            let Some(selected_was_playing) = selected else {
                return (false, false);
            };
            let was_playing = action == "playpause" && selected_was_playing;
            (self.media_action(id, action, value).await, was_playing)
        })
    }
    fn pause_current_media(&self) -> BoxFuture<'_, bool> {
        if let Some(hook) = lock(&self.on_pause).clone() {
            return hook();
        }
        let mut media = lock(&self.media);
        let paused = match usize::try_from(media.current) {
            Ok(i) if i < media.sessions.len() && media.sessions[i].playing => {
                media.paused.push(i);
                media.sessions[i].playing = false;
                true
            }
            _ => false,
        };
        Box::pin(async move { paused })
    }
    fn play(&self, id: &str, _: &str, _: Option<&str>, _: i32) -> Result<(), String> {
        let mut radios = lock(&self.radios);
        self.play_entered.set();
        if let Some(block) = lock(&self.block_play).clone() {
            block.wait(Duration::from_secs(10));
        }
        if radios.failing.contains(id) {
            return Err("Stream open failed".into());
        }
        if !radios.playing.iter().any(|p| p == id) {
            radios.playing.push(id.into());
        }
        Ok(())
    }
    fn stop_sound(&self, id: Option<&str>) {
        let mut radios = lock(&self.radios);
        match id {
            None => radios.playing.clear(),
            Some(id) => radios.playing.retain(|p| p != id),
        }
    }
    fn midi_ports(&self) -> Vec<DeckItem> {
        vec![]
    }
    fn midi(&self, _: i32, _: &str, _: i32) -> Result<(), String> {
        Ok(())
    }
}

struct Deck {
    _dir: tempfile::TempDir,
    store: Arc<ConfigStore>,
    deck: Arc<ControlDeck>,
}

fn deck_with(json: &str, native: &Arc<FakeRadio>, options: DeckOptions) -> Deck {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir, json);
    let deck = ControlDeck::new(store.clone(), native.clone(), options);
    Deck {
        _dir: dir,
        store,
        deck,
    }
}

fn clock(now: &Arc<AtomicI64>) -> DeckOptions {
    let now = now.clone();
    DeckOptions {
        now: Some(Arc::new(move || now.load(Ordering::SeqCst))),
        ..DeckOptions::default()
    }
}

async fn act(deck: &ControlDeck, kind: &str, id: &str, op: &str, value: Option<i32>) -> DeckResult {
    act_until(deck, kind, id, op, value, &Stop::default()).await
}

async fn act_until(
    deck: &ControlDeck,
    kind: &str,
    id: &str,
    op: &str,
    value: Option<i32>,
    stop: &Stop,
) -> DeckResult {
    let command = DeckCommand {
        kind: kind.into(),
        id: Some(id.into()),
        op: Some(op.into()),
        value,
        ..DeckCommand::default()
    };
    deck.act(command, stop).await.unwrap()
}

async fn radio(deck: &ControlDeck, action: &str, system: &MediaSnapshot) -> Option<bool> {
    deck.try_control_radio_media(action, Some(system), &Stop::default())
        .await
        .unwrap()
}

fn idle(pos: i64) -> MediaSnapshot {
    MediaSnapshot::new(
        "Spotify", "Track 1", "Artist 1", "Album 1", false, pos, 180_000, 1, true,
    )
}

fn save(store: &ConfigStore, json: &str) {
    store.save(json, None).unwrap();
}

#[tokio::test]
async fn next_and_previous_skip_disabled_stations_and_a_disabled_station_is_not_kept_as_paused() {
    let config = r#"{"controls":{"actions":[
        {"id":"r-a","label":"A","kind":"radio","target":"https://example.com/a.mp3","value":80},
        {"id":"r-b","label":"B","kind":"radio","target":"https://example.com/b.mp3","value":80,"enabled":false},
        {"id":"r-c","label":"C","kind":"radio","target":"https://example.com/c.mp3","value":80}]}}"#;
    let native = FakeRadio::new();
    let d = deck_with(config, &native, DeckOptions::default());
    let idle_sys = idle(0);

    assert!(act(&d.deck, "action", "r-a", "", None).await.ok);
    assert_eq!(radio(&d.deck, "next", &idle_sys).await, Some(true));
    assert_eq!(native.playing(), ["r-c"]); // B is disabled
    assert_eq!(radio(&d.deck, "prev", &idle_sys).await, Some(true));
    assert_eq!(native.playing(), ["r-a"]);

    // disabling the playing station stops it and leaves nothing to resume
    save(
        &d.store,
        &config.replace(
            r#""value":80},
        {"id":"r-b""#,
            r#""value":80,"enabled":false},
        {"id":"r-b""#,
        ),
    );
    assert!(!d.store.get().controls.actions[0].enabled);
    d.deck.stop_disabled_controls();
    d.deck.reconcile_radios(true);
    assert!(native.playing().is_empty());
    assert_eq!(
        d.deck.effective_media(Some(idle_sys.clone())),
        Some(idle_sys.clone())
    );
    assert_eq!(radio(&d.deck, "playpause", &idle_sys).await, None);
}

#[tokio::test]
async fn now_playing_shows_active_radio_and_next_prev_play_pause_controls_cycle_through_stations() {
    let config = r#"{"controls":{"actions":[
        {"id":"r-fenomen","label":"Radyo Fenomen","kind":"radio","target":"https://live.radyofenomen.com/fenomen/128/icecast.audio","value":80},
        {"id":"r-kafa","label":"Kafa Radyo","kind":"radio","target":"https://stream.radyotvonline.net/hls/play/kafaradyo.m3u8","value":75},
        {"id":"r-power","label":"PowerTürk","kind":"radio","target":"https://listen.powerapp.com.tr/powerturk/mpeg/icecast.audio","value":85}]}}"#;
    let now = Arc::new(AtomicI64::new(10_000));
    let native = FakeRadio::new();
    let d = deck_with(config, &native, clock(&now));
    let deck = &d.deck;
    let idle_sys = idle(12_000);
    let title = |deck: &ControlDeck| deck.effective_media(Some(idle_sys.clone())).unwrap().title;
    assert_eq!(
        deck.effective_media(Some(idle_sys.clone())),
        Some(idle_sys.clone())
    );
    assert_eq!(radio(deck, "next", &idle_sys).await, None);

    // 1. Start Radyo Fenomen -> the station stands in for the system media
    assert!(act(deck, "action", "r-fenomen", "", None).await.ok);
    now.store(15_000, Ordering::SeqCst);
    let m1 = deck.effective_media(Some(idle_sys.clone())).unwrap();
    assert_eq!(m1.app, "Internet Radio");
    assert_eq!(m1.title, "Radyo Fenomen");
    assert_eq!(m1.artist, "Live stream");
    assert!(m1.playing);
    assert_eq!(m1.pos_ms, 5_000);
    assert_eq!(m1.dur_ms, 0);

    // 2-4. next cycles through the stations and wraps around
    for (id, name) in [
        ("r-kafa", "Kafa Radyo"),
        ("r-power", "PowerTürk"),
        ("r-fenomen", "Radyo Fenomen"),
    ] {
        assert_eq!(radio(deck, "next", &idle_sys).await, Some(true));
        assert_eq!(native.playing(), [id]);
        assert_eq!(title(deck), name);
    }
    // 5-6. prev wraps backwards
    for (id, name) in [("r-power", "PowerTürk"), ("r-kafa", "Kafa Radyo")] {
        assert_eq!(radio(deck, "prev", &idle_sys).await, Some(true));
        assert_eq!(native.playing(), [id]);
        assert_eq!(title(deck), name);
    }

    // 7. play/pause pauses the station (still shown, as Paused), then resumes it
    now.store(22_000, Ordering::SeqCst);
    assert_eq!(radio(deck, "playpause", &idle_sys).await, Some(true));
    assert!(native.playing().is_empty());
    let paused = deck.effective_media(Some(idle_sys.clone())).unwrap();
    assert_eq!(paused.app, "Internet Radio");
    assert_eq!(paused.title, "Kafa Radyo");
    assert_eq!(paused.artist, "Paused");
    assert!(!paused.playing);
    assert_eq!(radio(deck, "playpause", &idle_sys).await, Some(true));
    assert_eq!(native.playing(), ["r-kafa"]);
    assert!(
        deck.effective_media(Some(idle_sys.clone()))
            .unwrap()
            .playing
    );

    // 8. a station that fails to open is skipped
    native.fail("r-power");
    assert_eq!(radio(deck, "next", &idle_sys).await, Some(true));
    assert_eq!(native.playing(), ["r-fenomen"]);
    assert_eq!(title(deck), "Radyo Fenomen");

    // 9. the deck's stop and sound-stop clear Now Playing
    assert!(act(deck, "action", "r-fenomen", "stop", None).await.ok);
    assert_eq!(
        deck.effective_media(Some(idle_sys.clone())),
        Some(idle_sys.clone())
    );
    assert!(act(deck, "action", "r-fenomen", "", None).await.ok);
    assert!(act(deck, "sound-stop", "r-fenomen", "", None).await.ok);
    assert_eq!(
        deck.effective_media(Some(idle_sys.clone())),
        Some(idle_sys.clone())
    );
    assert!(act(deck, "action", "r-fenomen", "", None).await.ok);
    assert!(act(deck, "sound-stop", "", "", None).await.ok);
    assert_eq!(
        deck.effective_media(Some(idle_sys.clone())),
        Some(idle_sys.clone())
    );

    // 10. a stream that ends by itself shows as Paused (by the callback, and by the reconcile)
    assert!(act(deck, "action", "r-kafa", "", None).await.ok);
    now.store(30_000, Ordering::SeqCst);
    native.simulate_stream_ended("r-kafa", true);
    let ended = deck.effective_media(Some(idle_sys.clone())).unwrap();
    assert!(!ended.playing);
    assert_eq!(ended.artist, "Paused");
    assert!(act(deck, "action", "r-kafa", "", None).await.ok);
    now.store(35_000, Ordering::SeqCst);
    native.simulate_stream_ended("r-kafa", false);
    deck.reconcile_radios(false);
    let ended = deck.effective_media(Some(idle_sys.clone())).unwrap();
    assert!(!ended.playing);
    assert_eq!(ended.artist, "Paused");

    // 11. a cancelled request does not cycle stations
    assert!(act(deck, "action", "r-fenomen", "", None).await.ok);
    let cancelled = Stop::default();
    cancelled.stop();
    assert_eq!(
        deck.try_control_radio_media("next", Some(&idle_sys), &cancelled)
            .await,
        Err(Cancelled)
    );

    // 12. starting a station pauses only the current session, and only once it plays
    lock(&native.radios).failing = HashSet::from(["r-power".to_string()]);
    native.set_sessions(
        vec![
            player("chrome.exe", "Chrome", "Tab 1 (paused)", false),
            player("chrome.exe", "Chrome", "Tab 2 (active)", true),
            player("vlc.exe", "VLC", "Background", true),
        ],
        1,
    );
    lock(&native.media).paused.clear();
    assert!(!act(deck, "action", "r-power", "", None).await.ok);
    assert!(lock(&native.media).paused.is_empty());
    assert!(act(deck, "action", "r-kafa", "", None).await.ok);
    assert_eq!(lock(&native.media).paused, [1]);
    assert!(!native.session_playing(1));
    assert!(native.session_playing(2));

    // 13. an unknown op keeps the station; seek, shuffle and repeat ask nothing; playpause of a paused player stops it
    native.set_sessions(vec![player("Spotify.exe", "Spotify", "Song", false)], 0);
    assert!(!act(deck, "media", "Spotify.exe", "play", None).await.ok);
    assert_eq!(native.playing(), ["r-kafa"]);
    lock(&native.media).queries = 0;
    assert!(
        act(deck, "media", "Spotify.exe", "seek", Some(5_000))
            .await
            .ok
    );
    assert!(
        act(deck, "media", "Spotify.exe", "shuffle", Some(1))
            .await
            .ok
    );
    assert!(
        act(deck, "media", "Spotify.exe", "repeat", Some(1))
            .await
            .ok
    );
    assert_eq!(lock(&native.media).queries, 0);
    assert_eq!(native.playing(), ["r-kafa"]);
    assert!(
        act(deck, "media", "Spotify.exe", "playpause", None)
            .await
            .ok
    );
    assert_eq!(lock(&native.media).queries, 1);
    assert!(native.playing().is_empty());

    // 14. next on a player stops the station without listing the sessions
    assert!(act(deck, "action", "r-kafa", "", None).await.ok);
    lock(&native.media).queries = 0;
    assert!(act(deck, "media", "Spotify.exe", "next", None).await.ok);
    assert_eq!(lock(&native.media).queries, 0);
    assert!(native.playing().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effective_media_does_not_stall_behind_audio_lock_or_state_lock() {
    let config = r#"{"controls":{"actions":[{"id":"r-fenomen","label":"Radyo Fenomen","kind":"radio","target":"https://live.radyofenomen.com/fenomen/128/icecast.audio","value":80}]}}"#;
    let native = FakeRadio::new();
    let gate = Arc::new(Event::default());
    *lock(&native.block_play) = Some(gate.clone());
    let d = deck_with(config, &native, DeckOptions::default());
    let idle_sys = idle(0);

    let play = tokio::spawn({
        let deck = d.deck.clone();
        async move { act(&deck, "action", "r-fenomen", "", None).await }
    });
    assert!(wait_for(&native.play_entered, Duration::from_secs(2)).await);
    // the snapshot holds the deck's state lock in its audio read, which waits on the fake's lock that Play holds
    let snapshot = tokio::spawn({
        let deck = d.deck.clone();
        async move { deck.snapshot().await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let media = tokio::task::spawn_blocking({
        let (deck, idle_sys) = (d.deck.clone(), idle_sys.clone());
        move || deck.effective_media(Some(idle_sys))
    });
    let answered = tokio::time::timeout(Duration::from_millis(500), media).await;
    gate.set();
    let _ = tokio::time::timeout(Duration::from_secs(10), play).await;
    let _ = tokio::time::timeout(Duration::from_secs(10), snapshot).await;
    assert_eq!(
        answered.expect("effective_media waited").unwrap(),
        Some(idle_sys)
    );
}

#[tokio::test]
async fn reconcile_radios_stops_removed_station_and_preserves_restarted_same_id_on_delayed_callback()
 {
    let config = r#"{"controls":{"actions":[
        {"id":"r-1","label":"Radyo Fenomen","kind":"radio","target":"https://example.com/fenomen.mp3","value":80},
        {"id":"r-2","label":"Kafa Radyo","kind":"radio","target":"https://example.com/kafa.mp3","value":75}]}}"#;
    let native = FakeRadio::new();
    let d = deck_with(config, &native, DeckOptions::default());

    assert!(act(&d.deck, "action", "r-1", "", None).await.ok);
    assert!(native.playing().contains(&"r-1".to_string()));

    // a late "stopped" for r-1 while it still plays keeps it
    native.fire_stale_sound_stopped("r-1");
    let still = d.deck.effective_media(None).unwrap();
    assert!(still.playing);
    assert_eq!(still.title, "Radyo Fenomen");

    // removing the playing station from the settings stops it and clears Now Playing
    save(
        &d.store,
        r#"{"controls":{"actions":[{"id":"r-2","label":"Kafa Radyo","kind":"radio","target":"https://example.com/kafa.mp3","value":75}]}}"#,
    );
    d.deck.reconcile_radios(true);
    assert!(native.playing().is_empty());
    assert_eq!(d.deck.effective_media(None), None);
}

#[tokio::test]
async fn shared_aumid_media_play_pause_checks_selected_target_session_before_stopping_radio() {
    let config = r#"{"controls":{"actions":[{"id":"r-1","label":"Radyo Fenomen","kind":"radio","target":"https://example.com/fenomen.mp3","value":80}]}}"#;
    let native = FakeRadio::new();
    let d = deck_with(config, &native, DeckOptions::default());

    // the default path: the selected (paused) session first, a playing one under the same app id after it
    assert!(act(&d.deck, "action", "r-1", "", None).await.ok);
    native.set_sessions(
        vec![
            player("chrome.exe", "Chrome", "Selected Tab (paused)", false),
            player("chrome.exe", "Chrome", "Background Tab (playing)", true),
        ],
        0,
    );
    assert!(
        act(&d.deck, "media", "chrome.exe", "playpause", None)
            .await
            .ok
    );
    assert!(native.playing().is_empty());
    assert!(native.session_playing(0));

    // the selected session read directly (as the Windows native does): the playing one comes first
    assert!(act(&d.deck, "action", "r-1", "", None).await.ok);
    lock(&native.media).direct = true;
    native.set_sessions(
        vec![
            player("chrome.exe", "Chrome", "Background Tab (playing)", true),
            player("chrome.exe", "Chrome", "Selected Tab (paused)", false),
        ],
        1,
    );
    assert!(
        act(&d.deck, "media", "chrome.exe", "playpause", None)
            .await
            .ok
    );
    assert!(native.playing().is_empty());
    assert!(native.session_playing(1));
}

#[tokio::test]
async fn cancelling_the_request_does_not_stop_a_station_that_started() {
    let config = r#"{"controls":{"actions":[{"id":"r-1","label":"Radyo Fenomen","kind":"radio","target":"https://example.com/fenomen.mp3","value":80}]}}"#;
    let native = FakeRadio::new();
    let pause_started = Arc::new(tokio::sync::Notify::new());
    native.on_pause({
        let started = pause_started.clone();
        Arc::new(move || {
            let started = started.clone();
            Box::pin(async move {
                started.notify_one();
                std::future::pending::<()>().await;
                false
            })
        })
    });
    let d = deck_with(
        config,
        &native,
        DeckOptions {
            media_pause: Some(Duration::from_millis(200)),
            ..DeckOptions::default()
        },
    );
    let stop = Stop::default();
    let request = tokio::spawn({
        let (deck, stop) = (d.deck.clone(), stop.clone());
        async move { act_until(&deck, "action", "r-1", "", None, &stop).await }
    });
    tokio::time::timeout(Duration::from_secs(2), pause_started.notified())
        .await
        .unwrap();
    assert!(native.playing().contains(&"r-1".to_string()));

    // the panel's request gives up: the station keeps playing, and the stuck pause gives up on its own budget
    stop.stop();
    tokio::time::timeout(Duration::from_secs(2), request)
        .await
        .unwrap()
        .unwrap();
    assert!(native.playing().contains(&"r-1".to_string()));
    assert_eq!(d.deck.effective_media(None).unwrap().app, "Internet Radio");
}

/// A pause that marks the session paused, says it began, and waits for the test to let it end.
fn held_pause(
    native: &Arc<FakeRadio>,
    mark_paused: bool,
) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
    let (entered, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let weak = Arc::downgrade(native);
    native.on_pause({
        let (entered, release) = (entered.clone(), release.clone());
        Arc::new(move || {
            if mark_paused && let Some(native) = weak.upgrade() {
                lock(&native.media).sessions[0].playing = false;
            }
            let (entered, release) = (entered.clone(), release.clone());
            Box::pin(async move {
                entered.notify_one();
                let _ = tokio::time::timeout(Duration::from_secs(10), release.notified()).await;
                true
            })
        })
    });
    (entered, release)
}

#[tokio::test]
async fn concurrent_radio_start_and_media_play_pause_serialize_without_both_playing() {
    let config = r#"{"controls":{"actions":[{"id":"r-1","label":"Radyo Fenomen","kind":"radio","target":"https://example.com/fenomen.mp3","value":80}]}}"#;
    let native = FakeRadio::new();
    native.set_sessions(vec![player("Spotify.exe", "Spotify", "Song", true)], 0);
    let (entered, release) = held_pause(&native, true);
    let d = deck_with(config, &native, DeckOptions::default());

    let start = tokio::spawn({
        let deck = d.deck.clone();
        async move { act(&deck, "action", "r-1", "", None).await }
    });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();

    // Spotify resumes while the station's start holds the media focus
    let play_pause = tokio::spawn({
        let deck = d.deck.clone();
        async move { act(&deck, "media", "Spotify.exe", "playpause", None).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!play_pause.is_finished());

    release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), start)
            .await
            .unwrap()
            .unwrap()
            .ok
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(10), play_pause)
            .await
            .unwrap()
            .unwrap()
            .ok
    );
    assert!(native.session_playing(0));
    assert!(native.playing().is_empty());
}

#[tokio::test]
async fn playlist_action_stops_active_radio_only_when_spotify_succeeds() {
    let config = r#"{"controls":{"spotifyToken":"test-access-token","actions":[
        {"id":"r-1","label":"Radyo Fenomen","kind":"radio","target":"https://example.com/fenomen.mp3","value":80},
        {"id":"pl-1","label":"Focus Mix","kind":"playlist","target":"spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"}]}}"#;
    let native = FakeRadio::new();
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir, config);
    let status = Arc::new(std::sync::atomic::AtomicU16::new(403));
    let http: crate::spotify_control::SpotifyHttp = {
        let status = status.clone();
        Arc::new(move |_| {
            let status = status.load(Ordering::SeqCst);
            Box::pin(async move {
                Ok(crate::spotify_control::SpotifyReply {
                    status,
                    retry_after: None,
                    body: String::new(),
                })
            })
        })
    };
    let spotify = Arc::new(crate::spotify_control::SpotifyControl::new(
        store.clone(),
        19885,
        http,
    ));
    let deck = ControlDeck::new(
        store,
        native.clone(),
        DeckOptions {
            spotify: Some(spotify),
            ..DeckOptions::default()
        },
    );

    assert!(act(&deck, "action", "r-1", "", None).await.ok);
    assert!(native.playing().contains(&"r-1".to_string()));
    assert!(!act(&deck, "action", "pl-1", "", None).await.ok);
    assert!(native.playing().contains(&"r-1".to_string()));
    status.store(204, Ordering::SeqCst);
    assert!(act(&deck, "action", "pl-1", "", None).await.ok);
    assert!(native.playing().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_or_removing_a_radio_across_focus_and_play_gates_never_leaves_it_playing() {
    // 1. stopping disabled controls alone clears the playback and Now Playing
    let native = FakeRadio::new();
    let d = deck_with(
        r#"{"controls":{"actions":[{"id":"r-1","label":"S1","kind":"radio","target":"https://example.com/1.mp3","value":80}]}}"#,
        &native,
        DeckOptions::default(),
    );
    assert!(act(&d.deck, "action", "r-1", "", None).await.ok);
    save(
        &d.store,
        r#"{"controls":{"actions":[{"id":"r-1","label":"S1","kind":"radio","target":"https://example.com/1.mp3","value":80,"enabled":false}]}}"#,
    );
    d.deck.stop_disabled_controls();
    assert!(native.playing().is_empty());
    assert_eq!(d.deck.effective_media(None), None);

    // 2. a start queued behind the media focus while its station is disabled is refused once the focus is free
    let native = FakeRadio::new();
    let (entered, release) = held_pause(&native, false);
    let both = r#"{"controls":{"actions":[
        {"id":"r-1","label":"S1","kind":"radio","target":"https://example.com/1.mp3","value":80},
        {"id":"r-2","label":"S2","kind":"radio","target":"https://example.com/2.mp3","value":80}]}}"#;
    let d = deck_with(both, &native, DeckOptions::default());
    let first = tokio::spawn({
        let deck = d.deck.clone();
        async move { act(&deck, "action", "r-1", "", None).await }
    });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let second = tokio::spawn({
        let deck = d.deck.clone();
        async move { act(&deck, "action", "r-2", "", None).await }
    });
    save(
        &d.store,
        &both.replace(
            r#"2.mp3","value":80}"#,
            r#"2.mp3","value":80,"enabled":false}"#,
        ),
    );
    let deck = d.deck.clone();
    tokio::task::spawn_blocking(move || {
        deck.stop_disabled_controls();
        deck.reconcile_radios(true);
    })
    .await
    .unwrap();
    release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), first)
            .await
            .unwrap()
            .unwrap()
            .ok
    );
    let refused = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap();
    assert!(!refused.ok);
    assert_eq!(refused.error.as_deref(), Some("Control is disabled"));
    assert_eq!(native.playing(), ["r-1"]);

    // 3. disabled while the station opens: it is stopped before it counts as playing
    let native = FakeRadio::new();
    let block = Arc::new(Event::default());
    *lock(&native.block_play) = Some(block.clone());
    let d = deck_with(
        r#"{"controls":{"actions":[{"id":"r-3","label":"S3","kind":"radio","target":"https://example.com/3.mp3","value":80}]}}"#,
        &native,
        DeckOptions::default(),
    );
    let play = tokio::spawn({
        let deck = d.deck.clone();
        async move { act(&deck, "action", "r-3", "", None).await }
    });
    let entered = wait_for(&native.play_entered, Duration::from_secs(5)).await;
    save(
        &d.store,
        r#"{"controls":{"actions":[{"id":"r-3","label":"S3","kind":"radio","target":"https://example.com/3.mp3","value":80,"enabled":false}]}}"#,
    );
    block.set();
    assert!(entered);
    let result = tokio::time::timeout(Duration::from_secs(5), play)
        .await
        .unwrap()
        .unwrap();
    assert!(!result.ok);
    assert!(native.playing().is_empty());
    assert_eq!(d.deck.effective_media(None), None);
}

#[tokio::test]
async fn tap_to_toggle_starts_and_stops_radio_without_separate_stop_action() {
    let config = r#"{"controls":{"actions":[
        {"id":"r-1","label":"Jazz FM","kind":"radio","target":"https://example.com/jazz.mp3","value":80},
        {"id":"r-2","label":"Classic FM","kind":"radio","target":"https://example.com/classic.mp3","value":75}]}}"#;
    let native = FakeRadio::new();
    let d = deck_with(config, &native, DeckOptions::default());

    assert!(act(&d.deck, "action", "r-1", "toggle", None).await.ok);
    assert_eq!(native.playing(), ["r-1"]);
    // an empty op (a profile) keeps it playing instead of toggling it off
    assert!(act(&d.deck, "action", "r-1", "", None).await.ok);
    assert_eq!(native.playing(), ["r-1"]);
    assert!(act(&d.deck, "action", "r-2", "toggle", None).await.ok);
    assert_eq!(native.playing(), ["r-2"]);
    assert!(act(&d.deck, "action", "r-2", "toggle", None).await.ok);
    assert!(native.playing().is_empty());
}

// Rust-only: the deck as the media routes and the state see it.

#[tokio::test]
async fn the_deck_is_the_media_routes_radio_and_stops_on_settings_saves() {
    use crate::media_monitor::MediaDeck;
    let config = r#"{"controls":{"actions":[{"id":"r-1","label":"Jazz","kind":"radio","target":"https://example.com/jazz.mp3","value":80}]}}"#;
    let native = FakeRadio::new();
    let d = deck_with(config, &native, DeckOptions::default());
    let media: Arc<dyn MediaDeck> = d.deck.clone();
    assert_eq!(media.try_control_radio("next", None).await, None);
    assert!(act(&d.deck, "action", "r-1", "", None).await.ok);
    assert_eq!(media.effective_media(None).unwrap().title, "Jazz");
    // the station is the current media: playpause pauses it
    assert_eq!(media.try_control_radio("playpause", None).await, Some(true));
    assert!(native.playing().is_empty());
    assert_eq!(media.effective_media(None).unwrap().artist, "Paused");
    // a playing system player takes over Now Playing and forgets the paused station
    let playing = MediaSnapshot::new("Spotify", "Song", "A", "", true, 0, 1, 0, true);
    assert_eq!(media.effective_media(Some(playing.clone())), Some(playing));
    assert_eq!(media.effective_media(None), None);
    assert!(act(&d.deck, "action", "r-1", "", None).await.ok);
    media.stop_all_radios();
    assert!(native.playing().is_empty());
}
