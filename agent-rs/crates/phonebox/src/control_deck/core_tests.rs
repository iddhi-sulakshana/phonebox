//! `ControlDeckCommandTests.cs` (profiles, grouping, power, the phone's limits), `ControlDeckPanicTests.cs` and
//! `ControlDeckTests.PanicMuteDrops…`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde_json::Value;

use super::actions::shortcut_process;
use super::integrations::mixer_parameter;
use super::sleep::player_session_matches;
use super::*;
use crate::control_deck::testing::{Event, lock, store};

fn device(
    id: &str,
    name: &str,
    direction: &str,
    default: bool,
    volume: i32,
    muted: bool,
) -> DeckDevice {
    DeckDevice {
        id: id.into(),
        name: name.into(),
        direction: direction.into(),
        default,
        volume,
        muted,
        peak: 0.0,
        channels: if direction == "input" { 1 } else { 2 },
        can_control: true,
        channel_volumes: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn session_of(
    id: &str,
    name: &str,
    pid: i32,
    device_id: &str,
    volume: i32,
    muted: bool,
    peak: f32,
    process: Option<&str>,
    active: bool,
) -> DeckSession {
    DeckSession {
        id: id.into(),
        name: name.into(),
        pid,
        device_id: device_id.into(),
        volume,
        muted,
        peak: peak as f64,
        process_name: process.map(str::to_string),
        active,
    }
}

fn session(
    id: &str,
    name: &str,
    pid: i32,
    device_id: &str,
    volume: i32,
    muted: bool,
    peak: f32,
) -> DeckSession {
    session_of(id, name, pid, device_id, volume, muted, peak, None, true)
}

fn spotify() -> DeckMedia {
    DeckMedia {
        id: "Spotify.exe".into(),
        name: "Spotify".into(),
        title: "Track".into(),
        playing: true,
        position: 0,
        duration: 100_000,
        can_seek: true,
        can_shuffle: true,
        can_repeat: true,
        shuffle: false,
        repeat: "None".into(),
    }
}

/// `ControlDeckCommandTests.Native`: a speaker, a headset and a muted microphone; records every write.
struct Native {
    writes: Mutex<Vec<String>>,
    sessions: Mutex<Vec<DeckSession>>,
    defaults: IndexMap<String, String>,
    players: Mutex<Vec<DeckMedia>>,
    /// The microphone: muted, and what it hears.
    mic: Mutex<(bool, f64)>,
}

impl Native {
    fn new() -> Native {
        Native {
            writes: Mutex::default(),
            sessions: Mutex::default(),
            defaults: IndexMap::from([("output:1".to_string(), "speaker".to_string())]),
            players: Mutex::new(vec![spotify()]),
            mic: Mutex::new((true, 0.0)),
        }
    }
    fn set_volume(&self, id: &str, volume: i32) {
        for s in lock(&self.sessions).iter_mut().filter(|s| s.id == id) {
            s.volume = volume;
        }
    }
    fn volumes(&self) -> Vec<i32> {
        self.sessions().iter().map(|s| s.volume).collect()
    }
    fn write(&self, what: String) {
        lock(&self.writes).push(what);
    }
    fn writes(&self) -> Vec<String> {
        lock(&self.writes).clone()
    }
    fn set_sessions(&self, sessions: Vec<DeckSession>) {
        *lock(&self.sessions) = sessions;
    }
    fn sessions(&self) -> Vec<DeckSession> {
        lock(&self.sessions).clone()
    }
}

impl DeckNative for Native {
    fn audio(&self) -> DeckAudio {
        DeckAudio {
            available: true,
            error: None,
            devices: vec![
                device("speaker", "Speaker", "output", true, 40, false),
                device("headset", "Headset", "output", false, 60, false),
                {
                    let (muted, peak) = *lock(&self.mic);
                    let mut mic = device("mic", "Mic", "input", true, 70, muted);
                    mic.peak = peak;
                    mic
                },
            ],
            sessions: self.sessions(),
            defaults: Some(self.defaults.clone()),
        }
    }
    fn device(&self, id: &str, _: Option<i32>, _: Option<bool>, _: Option<i32>) -> bool {
        self.write(format!("device:{id}"));
        true
    }
    fn session(&self, id: &str, volume: Option<i32>, muted: Option<bool>) -> bool {
        self.write(format!("session:{id}"));
        for s in lock(&self.sessions).iter_mut().filter(|s| s.id == id) {
            s.volume = volume.unwrap_or(s.volume);
            s.muted = muted.unwrap_or(s.muted);
        }
        true
    }
    fn default_device(&self, id: &str, role: Option<i32>) -> bool {
        let role = role.map(|r| r.to_string()).unwrap_or_default();
        self.write(format!("default:{id}:{role}"));
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
        kind: &'a str,
        _: &'a str,
        _: Option<&'a str>,
        _: i32,
    ) -> BoxFuture<'a, bool> {
        self.write(format!("platform:{kind}"));
        Box::pin(async { true })
    }
    fn media(&self) -> BoxFuture<'_, Result<Vec<DeckMedia>, String>> {
        let players = lock(&self.players).clone();
        Box::pin(async move { Ok(players) })
    }
    fn media_action<'a>(&'a self, _: &'a str, action: &'a str, _: i64) -> BoxFuture<'a, bool> {
        self.write(format!("media:{action}"));
        Box::pin(async { true })
    }
    fn play(&self, _: &str, _: &str, _: Option<&str>, volume: i32) -> Result<(), String> {
        self.write(format!("sound:{volume}"));
        Ok(())
    }
    fn stop_sound(&self, id: Option<&str>) {
        self.write(format!("stop:{}", id.unwrap_or("")));
    }
    fn midi_ports(&self) -> Vec<DeckItem> {
        vec![]
    }
    fn midi(&self, _: i32, _: &str, _: i32) -> Result<(), String> {
        Ok(())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    native: Arc<Native>,
    deck: Arc<ControlDeck>,
    now: Arc<std::sync::atomic::AtomicI64>,
}

impl Fixture {
    fn new(controls: &str) -> Fixture {
        Fixture::with_config(&format!("{{\"controls\":{controls}}}"))
    }

    fn with_config(json: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let cfg = store(&dir, json);
        let native = Arc::new(Native::new());
        let now = Arc::new(std::sync::atomic::AtomicI64::new(1000));
        let clock = now.clone();
        let deck = ControlDeck::new(
            cfg,
            native.clone(),
            DeckOptions {
                now: Some(Arc::new(move || clock.load(Ordering::SeqCst))),
                ..DeckOptions::default()
            },
        );
        Fixture {
            _dir: dir,
            native,
            deck,
            now,
        }
    }

    fn advance(&self, ms: i64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    async fn act(&self, kind: &str, id: &str, value: Option<i32>, op: Option<&str>) -> DeckResult {
        act(&self.deck, kind, Some(id), op, value, None, None).await
    }

    async fn snapshot(&self) -> Value {
        self.deck.snapshot().await.unwrap()
    }
}

async fn act(
    deck: &Arc<ControlDeck>,
    kind: &str,
    id: Option<&str>,
    op: Option<&str>,
    value: Option<i32>,
    muted: Option<bool>,
    lease: Option<&str>,
) -> DeckResult {
    let command = DeckCommand {
        kind: kind.into(),
        id: id.map(str::to_string),
        op: op.map(str::to_string),
        value,
        muted,
        lease: lease.map(str::to_string),
    };
    deck.act(command, &Stop::default()).await.unwrap()
}

fn sessions_of(snapshot: &Value) -> Vec<Value> {
    snapshot["audio"]["sessions"].as_array().unwrap().clone()
}

#[tokio::test]
async fn missing_explicit_profile_device_does_not_write_any_levels() {
    let f = Fixture::new(
        r#"{"profiles":[{"id":"profile","output":"unplugged","levels":[{"kind":"device","id":"speaker","volume":20,"muted":false}]}]}"#,
    );
    assert!(!f.act("profile", "profile", None, None).await.ok);
    assert!(f.native.writes().is_empty());
}

#[tokio::test]
async fn explicit_profile_output_overrides_captured_role_selection() {
    let f = Fixture::new(
        r#"{"profiles":[{"id":"profile","output":"headset","defaults":{"output:1":"speaker"},"levels":[]}]}"#,
    );
    assert!(f.act("profile", "profile", None, None).await.ok);
    assert_eq!(f.native.writes(), ["default:speaker:1", "default:headset:"]);
}

#[tokio::test]
async fn phone_cannot_provide_an_executable_or_unknown_target() {
    let f = Fixture::new("{}");
    assert!(!f.act("action", "powershell.exe", None, None).await.ok);
    assert!(!f.act("device", "unplugged", Some(50), None).await.ok);
    assert!(!f.act("session", "ended", Some(50), None).await.ok);
    assert!(!f.act("device", "speaker", Some(101), None).await.ok);
    assert!(!f.act("device", "speaker", None, None).await.ok);
    assert!(f.native.writes().is_empty());
}

#[tokio::test]
async fn grouped_application_volume_updates_every_matching_session_across_outputs() {
    let f = Fixture::new("{}");
    f.native.set_sessions(vec![
        session("chrome-speakers", "Chrome", 10, "speaker", 100, false, 0.0),
        session("chrome-headset", "chrome", 11, "headset", 32, true, 0.0),
        session("other", "OpenCode", 12, "speaker", 80, false, 0.0),
    ]);
    assert!(
        f.act("session", "application:CHROME", Some(55), None)
            .await
            .ok
    );
    assert_eq!(
        f.native.writes(),
        ["session:chrome-speakers", "session:chrome-headset"]
    );
    let sessions = f.native.sessions();
    assert!(
        sessions
            .iter()
            .filter(|s| s.name.eq_ignore_ascii_case("chrome"))
            .all(|s| s.volume == 55)
    );
    assert_eq!(
        sessions
            .iter()
            .find(|s| s.name == "OpenCode")
            .unwrap()
            .volume,
        80
    );
}

#[tokio::test]
async fn snapshot_groups_application_sessions_across_outputs() {
    let f = Fixture::new("{}");
    f.native.set_sessions(vec![
        session("chrome-speakers", "Chrome", 10, "speaker", 100, false, 0.2),
        session("chrome-headset", "chrome", 11, "headset", 32, true, 0.8),
        session("other", "OpenCode", 12, "speaker", 80, false, 0.1),
    ]);
    let snapshot = f.snapshot().await;
    let sessions = sessions_of(&snapshot);
    let chrome: Vec<&Value> = sessions
        .iter()
        .filter(|s| s["id"] == "application:chrome")
        .collect();
    assert_eq!(chrome.len(), 1);
    assert!((chrome[0]["peak"].as_f64().unwrap() - 0.8).abs() < 0.001);
    assert_eq!(chrome[0]["volume"], 100);
}

#[tokio::test]
async fn snapshot_hides_inactive_sessions_but_grouped_writes_still_reach_them() {
    let f = Fixture::new("{}");
    f.native.set_sessions(vec![
        session_of(
            "active",
            "Friendly app",
            10,
            "speaker",
            100,
            false,
            0.0,
            Some("rawapp"),
            true,
        ),
        session_of(
            "inactive",
            "Friendly app",
            10,
            "headset",
            40,
            false,
            0.0,
            Some("rawapp"),
            false,
        ),
    ]);
    assert_eq!(sessions_of(&f.snapshot().await).len(), 1);
    assert!(
        f.act("session", "application:friendly app", Some(55), None)
            .await
            .ok
    );
    assert_eq!(f.native.writes(), ["session:active", "session:inactive"]);
}

#[tokio::test]
async fn unknown_processes_do_not_collapse_into_one_application() {
    let f = Fixture::new("{}");
    f.native.set_sessions(vec![
        session_of(
            "unknown-one",
            "Application",
            10,
            "speaker",
            100,
            false,
            0.0,
            Some(""),
            true,
        ),
        session_of(
            "unknown-two",
            "Application",
            11,
            "speaker",
            80,
            false,
            0.0,
            Some(""),
            true,
        ),
    ]);
    assert_eq!(sessions_of(&f.snapshot().await).len(), 2);
}

#[tokio::test]
async fn grouped_session_id_from_an_existing_phone_updates_every_copy() {
    let f = Fixture::new("{}");
    f.native.set_sessions(vec![
        session(
            "system-speakers",
            "System sounds",
            0,
            "speaker",
            100,
            false,
            0.0,
        ),
        session(
            "system-headset",
            "System sounds",
            0,
            "headset",
            70,
            false,
            0.0,
        ),
    ]);
    assert!(
        f.act("session", "application:System sounds", Some(40), None)
            .await
            .ok
    );
    assert_eq!(
        f.native.writes(),
        ["session:system-speakers", "session:system-headset"]
    );
    assert!(f.native.sessions().iter().all(|s| s.volume == 40));
}

#[tokio::test]
async fn power_actions_cannot_bypass_opt_in_or_hold_nonce() {
    let f = Fixture::new(r#"{"allowPower":true}"#);
    assert!(!f.act("power", "shutdown", None, None).await.ok);
    assert!(!f.act("power-arm", "format-drive", None, None).await.ok);
    assert!(f.native.writes().is_empty());
}

#[tokio::test]
async fn sleep_fades_only_the_selected_player_then_pauses_and_restores_volume() {
    let f = Fixture::new("{}");
    f.native.set_sessions(vec![
        session("spotify-session", "Spotify", 123, "speaker", 80, false, 0.0),
        session("browser", "Chrome", 124, "speaker", 55, false, 0.0),
    ]);
    assert!(f.act("sleep", "Spotify.exe", Some(1), None).await.ok);
    f.advance(45_000);
    f.deck.tick_sleep(&Stop::default()).await;
    assert_eq!(f.native.volumes(), [40, 55]);
    f.advance(15_000);
    f.deck.tick_sleep(&Stop::default()).await;
    assert_eq!(f.native.volumes(), [80, 55]);
    assert!(f.native.writes().contains(&"media:pause".to_string()));
}

#[tokio::test]
async fn sleep_fade_preserves_manual_volume_changes() {
    let f = Fixture::new("{}");
    f.native.set_sessions(vec![session(
        "spotify-session",
        "Spotify",
        123,
        "speaker",
        80,
        false,
        0.0,
    )]);
    assert!(f.act("sleep", "Spotify.exe", Some(1), None).await.ok);
    f.advance(45_000);
    f.deck.tick_sleep(&Stop::default()).await;
    assert_eq!(f.native.volumes(), [40]);
    f.native.set_volume("spotify-session", 65);
    f.advance(15_000);
    f.deck.tick_sleep(&Stop::default()).await;
    assert_eq!(f.native.volumes(), [65]);
}

#[tokio::test]
async fn disabled_control_is_hidden_rejected_and_stopped() {
    let f = Fixture::new(
        r#"{"actions":[{"id":"r1","label":"Station 1","kind":"radio","target":"https://example.com/1","value":80,"enabled":false},{"id":"r2","label":"Station 2","kind":"radio","target":"https://example.com/2","value":80,"enabled":true},{"id":"s1","label":"Chime","kind":"sound","target":"chime.wav","value":50,"enabled":false}]}"#,
    );
    let snapshot = f.snapshot().await.to_string();
    assert!(!snapshot.contains("\"r1\""));
    assert!(snapshot.contains("\"r2\""));
    let refused = f.act("action", "r1", None, None).await;
    assert!(!refused.ok);
    assert_eq!(refused.error.as_deref(), Some("Control is disabled"));
    assert!(f.act("action", "r1", None, Some("stop")).await.ok);
    assert!(f.act("action", "s1", None, Some("stop")).await.ok);
    f.deck.stop_disabled_controls();
    let writes = f.native.writes();
    assert!(writes.contains(&"stop:r1".to_string()));
    assert!(writes.contains(&"stop:s1".to_string()));
    assert!(!writes.contains(&"stop:r2".to_string()));
}

#[test]
fn sleep_fade_matches_the_players_own_audio_session() {
    for (player, session, expected) in [
        ("Spotify.exe", "Spotify", true),
        (
            "SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify",
            "Spotify",
            true,
        ),
        (
            "Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic",
            "Microsoft.Photos",
            false,
        ),
    ] {
        assert_eq!(
            player_session_matches(player, session),
            expected,
            "{player} / {session}"
        );
    }
}

#[tokio::test]
async fn a_sleep_timer_can_be_cancelled_after_its_player_closed() {
    let f = Fixture::new("{}");
    assert!(f.act("sleep", "Spotify.exe", Some(30), None).await.ok);
    let player = lock(&f.native.players).remove(0); // the player is closed
    let cancel = f.act("sleep", "Spotify.exe", Some(0), None).await;
    lock(&f.native.players).push(player); // ... and reopened later
    f.advance(31 * 60_000);
    f.deck.tick_sleep(&Stop::default()).await;
    assert!(!f.native.writes().contains(&"media:pause".to_string()));
    assert!(cancel.ok);
}

// Rust-only: sleep needs a player that is there; ducking lowers the other sessions only while a microphone hears.

#[tokio::test]
async fn a_sleep_timer_needs_an_active_player_and_shows_on_the_phone() {
    let f = Fixture::new("{}");
    let r = f.act("sleep", "VLC.exe", Some(5), None).await;
    assert_eq!(r.error.as_deref(), Some("Select an active media player"));
    assert!(f.act("sleep", "Spotify.exe", Some(500), None).await.ok);
    // at most 6 hours
    assert_eq!(
        f.snapshot().await["sleep"],
        serde_json::json!([{"id":"Spotify.exe","at":1000 + 360 * 60_000}])
    );
}

#[tokio::test]
async fn ducking_lowers_other_sessions_while_a_microphone_hears_and_restores_them() {
    let f = Fixture::new("{}");
    let own = std::process::id() as i32;
    f.native.set_sessions(vec![
        session("chrome", "Chrome", 10, "speaker", 81, false, 0.0),
        session("agent", "PhoneBox", own, "speaker", 70, false, 0.0),
    ]);
    assert!(f.act("duck", "", Some(50), None).await.ok);
    // each tick reads the audio again (the deck reuses a reading for 400 ms)
    let tick = |f: &Fixture| {
        let deck = f.deck.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                deck.invalidate();
                deck.tick_ducking()
            })
            .await
            .unwrap()
        }
    };
    // a muted microphone hears nothing
    tick(&f).await;
    assert_eq!(f.native.volumes(), [81, 70]);
    *lock(&f.native.mic) = (false, 0.5);
    tick(&f).await;
    // 40.5 rounds to even; the agent's own session is left alone
    assert_eq!(f.native.volumes(), [40, 70]);
    tick(&f).await;
    assert_eq!(f.native.volumes(), [40, 70]);
    *lock(&f.native.mic) = (false, 0.01);
    tick(&f).await;
    assert_eq!(f.native.volumes(), [81, 70]);
    // ducked again, then the agent stops: the volume comes back
    *lock(&f.native.mic) = (false, 0.5);
    tick(&f).await;
    assert_eq!(f.native.volumes(), [40, 70]);
    f.deck.dispose();
    assert_eq!(f.native.volumes(), [81, 70]);
}

#[tokio::test]
async fn zero_sound_volume_is_preserved() {
    let f = Fixture::new(
        r#"{"actions":[{"id":"sound","kind":"sound","target":"configured.wav","value":0}]}"#,
    );
    assert!(f.act("action", "sound", None, None).await.ok);
    assert_eq!(f.native.writes(), ["sound:0"]);
}

#[tokio::test]
async fn disabled_playlist_profile_and_default_enablement_behave_safely() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = store(
        &dir,
        r#"{"controls":{
          "actions":[
            {"id":"pl1","label":"Chill","kind":"playlist","target":"spotify:playlist:37i9dQZF1DX4WYpdgoIcn6","enabled":false},
            {"id":"r1","label":"Disabled Radio","kind":"radio","target":"https://example.com/1","value":70,"enabled":false},
            {"id":"r2","label":"Enabled Radio","kind":"radio","target":"https://example.com/2","value":75},
            {"id":"t1","label":"Disabled Task","kind":"task","target":"missing-shortcut","enabled":false}
          ],
          "profiles":[{"id":"p1","label":"Mix","levels":[],"actions":["r1","r2"]}]
        }}"#,
    );
    assert!(
        cfg.get()
            .controls
            .actions
            .iter()
            .find(|a| a.id == "r2")
            .unwrap()
            .enabled
    );

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let http: crate::spotify_control::SpotifyHttp = {
        let calls = calls.clone();
        Arc::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err("no network in this test".to_string()) })
        })
    };
    let spotify = crate::spotify_control::SpotifyControl::new(cfg.clone(), 19885, http);
    let played = spotify.act("playlist", Some("pl1")).await;
    assert!(!played.ok);
    assert_eq!(played.error.as_deref(), Some("Control is disabled"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let native = Arc::new(Native::new());
    let deck = ControlDeck::new(cfg, native.clone(), DeckOptions::default());
    let task_stop = act(&deck, "action", Some("t1"), Some("stop"), None, None, None).await;
    assert!(!task_stop.ok);
    assert_eq!(task_stop.error.as_deref(), Some("This task is not running"));

    assert!(
        act(&deck, "profile", Some("p1"), None, None, None, None)
            .await
            .ok
    );
    let writes = native.writes();
    assert!(writes.contains(&"sound:75".to_string()));
    assert!(!writes.contains(&"sound:70".to_string()));
}

#[tokio::test]
async fn a_folder_shortcut_is_not_treated_as_a_running_application() {
    let dir = tempfile::tempdir().unwrap();
    // nothing is opened: the folder does not exist
    let folder = format!(
        "{}{}",
        dir.path().join("missing-project").display(),
        std::path::MAIN_SEPARATOR
    );
    let config = serde_json::json!({"shortcuts":[{"id":"project","label":"Project","cmd":folder}]});
    let cfg = store(&dir, &config.to_string());
    let native = Arc::new(Native::new());
    let deck = ControlDeck::new(cfg, native.clone(), DeckOptions::default());
    let state = deck.snapshot().await.unwrap();
    // what the App Dock sends
    let _ = act(
        &deck,
        "action",
        Some("shortcut:project"),
        Some("focus"),
        None,
        None,
        None,
    )
    .await;
    assert!(!native.writes().contains(&"platform:focus".to_string()));
    assert_ne!(state["shortcuts"][0]["state"], "Running");
}

/// C# ran this on Windows only (it read the attempted command from a launch failure); here the two commands are
/// shell commands that leave a file behind, on Linux and macOS.
#[tokio::test]
async fn deck_runs_the_off_command_of_a_toggle_whose_pushed_state_is_on() {
    if cfg!(windows) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_string_lossy().into_owned();
    let config = serde_json::json!({"shortcuts":[{"id":"lamp","label":"Lamp","toggle":true,"stateKey":"lamp",
        "cmd":"/bin/sh","args":"-c \"echo on > lamp-on\"","offCmd":"/bin/sh","offArgs":"-c \"echo off > lamp-off\"","cwd":cwd}]});
    let cfg = store(&dir, &config.to_string());
    let push = crate::push_store::PushStore::new(&dir.path().join("data.json"));
    push.set("lamp", serde_json::json!(true), None).unwrap();
    let deck = ControlDeck::new(
        cfg,
        Arc::new(Native::new()),
        DeckOptions {
            push: Some(push),
            ..DeckOptions::default()
        },
    );
    assert!(
        act(
            &deck,
            "action",
            Some("shortcut:lamp"),
            None,
            None,
            None,
            None
        )
        .await
        .ok
    );
    let off = dir.path().join("lamp-off");
    for _ in 0..100 {
        if off.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(off.exists());
    assert!(!dir.path().join("lamp-on").exists());
}

// Rust-only: what each kind of control answers when it cannot run.

#[tokio::test]
async fn each_kind_of_control_says_what_it_needs() {
    let f = Fixture::with_config(
        r#"{"shortcuts":[{"id":"s1","label":"Notes","cmd":"builtin:macro","steps":["bogus:x"]}],"controls":{"actions":[
            {"id":"midi","kind":"midi","device":"x","target":"90 40"},
            {"id":"midi2","kind":"midi","device":" 2 ","target":"90 40","value":7},
            {"id":"osc-bad","kind":"osc","device":"nohost","target":"/x"},
            {"id":"flow","kind":"workflow","target":"owner/repo/extra","argument":"ci.yml"},
            {"id":"flow2","kind":"workflow","target":"owner/repo","argument":"c i.yml"},
            {"id":"route","kind":"audio-route","target":"chrome.exe","device":"speaker"},
            {"id":"mix","kind":"mixer","target":"Strip[64].Gain"},
            {"id":"mix2","kind":"mixer-mute","target":"Bus[0].Mute"},
            {"id":"app","kind":"app","target":"gone"},
            {"id":"vol","kind":"volume-up","target":"x"},
            {"id":"pl","kind":"playlist","target":"spotify:playlist:x"}
        ]}}"#,
    );
    let error = |r: DeckResult| r.error.unwrap_or_default();
    assert_eq!(
        error(f.act("action", "midi", None, None).await),
        "Choose a MIDI output port"
    );
    assert!(f.act("action", "midi2", None, None).await.ok);
    assert_eq!(
        error(f.act("action", "osc-bad", None, None).await),
        "Control failed; check the computer's integration settings"
    );
    for id in ["flow", "flow2"] {
        assert_eq!(
            error(f.act("action", id, None, None).await),
            "Configure a repository and workflow filename"
        );
    }
    if !cfg!(windows) {
        assert_eq!(
            error(f.act("action", "route", None, None).await),
            "Configure SoundVolumeView on the computer to use application routing"
        );
    }
    for id in ["mix", "mix2"] {
        assert_eq!(
            error(f.act("action", id, Some(1), None).await),
            "Configure the installed Voicemeeter Remote DLL and start its audio engine"
        );
    }
    assert_eq!(
        error(f.act("action", "app", None, None).await),
        "Configured shortcut is missing"
    );
    assert_eq!(
        error(f.act("action", "shortcut:s1", None, None).await),
        "Configured shortcut failed"
    );
    assert_eq!(
        error(f.act("action", "shortcut:none", None, None).await),
        "Configured shortcut is missing"
    );
    // no Spotify in this deck
    assert_eq!(
        error(f.act("action", "pl", None, None).await),
        "Spotify is unavailable"
    );
    assert!(f.act("action", "vol", None, None).await.ok);
    assert_eq!(f.native.writes(), ["platform:volume-up"]);
}

#[tokio::test]
async fn osc_sends_one_packet_to_the_configured_host() {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = socket.local_addr().unwrap().port();
    let f = Fixture::new(&format!(
        r#"{{"actions":[{{"id":"osc","kind":"osc","device":"127.0.0.1:{port}","target":"/fader","value":3}}]}}"#
    ));
    assert!(f.act("action", "osc", Some(5), None).await.ok);
    let mut buf = [0u8; 64];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        &buf[..n],
        crate::control_deck::osc_packet("/fader", 5.0).unwrap()
    );
}

#[test]
fn mixer_parameters_and_shortcut_process_names() {
    for (name, ok) in [
        ("Strip[0].Gain", true),
        ("Strip[63].Mute", true),
        ("Bus[7].A5", true),
        ("Bus[1].B3", true),
        ("Strip[64].Gain", false),
        ("Bus[1].B4", false),
        ("Strip[1].Gain\n", false),
        ("strip[1].Gain", false),
    ] {
        assert_eq!(mixer_parameter(name), ok, "{name:?}");
    }
    for (cmd, name) in [
        ("builtin:keys", None),
        ("https://example.com", None),
        ("steam://run/1", None),
        ("C:\\Windows\\notepad.exe", Some("notepad")),
        ("file:///C:/Tools/app.exe", Some("app")),
        ("code", Some("code")),
        ("C:\\Projects\\site\\", None),
        ("/opt/app.v2.bin", Some("app.v2")),
    ] {
        assert_eq!(shortcut_process(cmd).as_deref(), name, "{cmd}");
    }
}

#[test]
fn listen_lines_escape_as_dotnet_does() {
    let packet = DeckAudioPacket {
        rate: 48_000,
        channels: 2,
        bits: 32,
        encoding: "IeeeFloat".into(),
        data: Some("AA+/".into()),
    };
    assert_eq!(
        listen_line(&packet),
        "{\"rate\":48000,\"channels\":2,\"bits\":32,\"encoding\":\"IeeeFloat\",\"data\":\"AA\\u002B/\"}\n"
    );
    let keep_alive = DeckAudioPacket {
        data: None,
        ..packet
    };
    assert!(listen_line(&keep_alive).ends_with("\"data\":null}\n"));
}

// Rust-only: the phone's request limits, the generic outcome texts and what the snapshot says about each control.

#[tokio::test]
async fn the_phones_request_limits_and_outcome_texts() {
    let f = Fixture::new(r#"{"allowPower":true}"#);
    let long = "x".repeat(2049);
    assert_eq!(
        f.act("device", &long, Some(5), None).await.error.as_deref(),
        Some("Invalid control request")
    );
    assert_eq!(
        f.act("device", "speaker", Some(5), Some(&"o".repeat(65)))
            .await
            .error
            .as_deref(),
        Some("Invalid control request")
    );
    assert_eq!(
        f.act("foo", "", None, None).await.error.as_deref(),
        Some("Unknown control action")
    );
    assert_eq!(
        f.act("device", "speaker", None, None)
            .await
            .error
            .as_deref(),
        Some("Choose a volume or mute state")
    );
    assert_eq!(
        f.act("session", "x", Some(101), None)
            .await
            .error
            .as_deref(),
        Some("Choose a valid volume")
    );
    assert_eq!(
        f.act("default", "speaker", None, Some("bogus"))
            .await
            .error
            .as_deref(),
        Some("Unknown audio role")
    );
    // a refused write: the generic text
    let brightness = f.act("brightness", "m1", Some(50), None).await;
    assert_eq!(
        brightness.error.as_deref(),
        Some("This control is unavailable on the selected device")
    );
    assert_eq!(
        f.act("default", "speaker", None, Some("media")).await,
        DeckResult::ok()
    );
    assert!(f.act("device", "speaker", Some(30), Some("left")).await.ok);
    assert_eq!(f.native.writes(), ["default:speaker:1", "device:speaker"]);
    // armed, then confirmed too early: the confirmation is delayed
    let armed = f.act("power-arm", "sleep", None, None).await;
    assert!(armed.ok && armed.lease.is_some());
    let early = act(
        &f.deck,
        "power",
        Some("sleep"),
        None,
        None,
        None,
        armed.lease.as_deref(),
    )
    .await;
    assert_eq!(
        early.error.as_deref(),
        Some("Hold the power button to confirm")
    );
    assert!(f.act("desktop", "", Some(2), None).await.error.is_some());
    assert!(f.act("desktop", "", Some(-1), None).await.ok);
    // no power plans here
    assert!(!f.act("power-plan", "balanced", None, None).await.ok);
    assert!(f.act("duck", "", Some(150), None).await.ok);
    assert_eq!(f.snapshot().await["ducking"], true);
    assert!(f.act("duck", "", None, Some("off")).await.ok);
    assert_eq!(f.snapshot().await["ducking"], false);
}

#[tokio::test]
async fn power_needs_the_desktop_opt_in() {
    let f = Fixture::new("{}");
    let r = f.act("power-arm", "sleep", None, None).await;
    assert_eq!(
        r.error.as_deref(),
        Some("Enable power controls in desktop settings")
    );
}

#[tokio::test]
async fn the_snapshot_keeps_the_phones_keys_and_describes_controls_that_need_setup() {
    let f = Fixture::with_config(
        r#"{"shortcuts":[{"id":"s1","label":"Notes","cmd":"builtin:keys","args":"ctrl+n"}],"controls":{"actions":[
            {"id":"pl","label":"Chill","kind":"playlist","target":"spotify:playlist:x"},
            {"id":"m","label":"Pad","kind":"midi","device":"x","target":"90 40"},
            {"id":"app","label":"App","kind":"app","target":"missing"},
            {"id":"app2","label":"App 2","kind":"app","target":"s1"},
            {"id":"svc","label":"Svc","kind":"service","target":"x"},
            {"id":"route","label":"Route","kind":"audio-route","target":"x"},
            {"id":"off","label":"Off","kind":"keys","target":"ctrl+a","enabled":false}
        ],"profiles":[{"id":"p1","label":"Mix"}]}}"#,
    );

    let snapshot = f.snapshot().await;
    let keys: Vec<&str> = snapshot
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "platform",
            "audio",
            "media",
            "mediaError",
            "monitors",
            "powerPlans",
            "midi",
            "hardwareLoading",
            "actions",
            "shortcuts",
            "profiles",
            "allowListening",
            "allowPower",
            "ducking",
            "sleep"
        ]
    );
    assert_eq!(snapshot["platform"], crate::app_paths::os());
    assert_eq!(snapshot["media"][0]["id"], "Spotify.exe");
    assert!(snapshot["mediaError"].is_null());
    let item = |id: &str| {
        snapshot["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .cloned()
    };
    let state = |id: &str| item(id).unwrap()["state"].clone();
    let available = |id: &str| item(id).unwrap()["available"].clone();
    assert_eq!(state("pl"), "Connect Spotify in desktop settings");
    assert_eq!(available("pl"), false);
    assert_eq!(state("m"), "MIDI port unavailable");
    assert_eq!(state("app"), "Shortcut is missing");
    assert_eq!(available("app"), false);
    assert_eq!(state("app2"), "Ready");
    assert_eq!(available("app2"), true);
    assert_eq!(state("svc"), "Service unavailable");
    assert_eq!(available("svc"), false);
    assert_eq!(state("route"), "Configure SoundVolumeView");
    // a disabled control is not on the phone
    assert!(item("off").is_none());
    assert_eq!(
        snapshot["shortcuts"],
        serde_json::json!([{"id":"s1","name":"Notes","kind":"keys","state":null,"available":true,"rev":null}])
    );
    assert_eq!(
        snapshot["profiles"],
        serde_json::json!([{"id":"p1","name":"Mix","kind":"profile","state":null,"available":true,"rev":null}])
    );
    assert_eq!(snapshot["sleep"], serde_json::json!([]));
}

#[tokio::test]
async fn capture_lists_controllable_devices_then_every_application() {
    let f = Fixture::new("{}");
    f.native.set_sessions(vec![
        session("chrome-1", "Chrome", 10, "speaker", 90, false, 0.0),
        session_of(
            "chrome-2", "chrome", 11, "headset", 30, true, 0.0, None, false,
        ),
    ]);
    let profile = f.deck.capture().unwrap();
    assert!(profile.id.starts_with("profile-"));
    assert_eq!(profile.id.len(), "profile-".len() + 12);
    assert!(
        profile.id["profile-".len()..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert_eq!(profile.label, "Audio profile");
    let levels: Vec<(String, String, Option<i32>, Option<bool>)> = profile
        .levels
        .iter()
        .map(|l| (l.kind.clone(), l.id.clone(), l.volume, l.muted))
        .collect();
    assert_eq!(
        levels,
        [
            ("device".into(), "speaker".into(), Some(40), Some(false)),
            ("device".into(), "headset".into(), Some(60), Some(false)),
            ("device".into(), "mic".into(), Some(70), Some(true)),
            ("application".into(), "chrome".into(), Some(90), Some(false)),
        ]
    );
    assert_eq!(
        profile.defaults.get("output:1").map(String::as_str),
        Some("speaker")
    );
    assert!(
        ControlDeck::new(
            f.deck.cfg.clone(),
            Arc::new(NoNative),
            DeckOptions::default()
        )
        .capture()
        .is_none()
    );
}

// ControlDeckPanicTests

/// `ControlDeckPanicTests.PanicNative`: a speaker whose mute can be held mid-way, and a microphone whose mute can fail.
struct PanicNative {
    mic_muted: AtomicBool,
    speaker_muted: AtomicBool,
    fail_mic_mute: AtomicBool,
    hold_speaker_mute: bool,
    speaker_mute_entered: Event,
    release_speaker_mute: Event,
}

impl PanicNative {
    fn new(mic_muted: bool, hold_speaker_mute: bool) -> PanicNative {
        PanicNative {
            mic_muted: AtomicBool::new(mic_muted),
            speaker_muted: AtomicBool::new(false),
            fail_mic_mute: AtomicBool::new(false),
            hold_speaker_mute,
            speaker_mute_entered: Event::default(),
            release_speaker_mute: Event::default(),
        }
    }
    fn mic_muted(&self) -> bool {
        self.mic_muted.load(Ordering::SeqCst)
    }
}

impl DeckNative for PanicNative {
    fn audio(&self) -> DeckAudio {
        DeckAudio {
            available: true,
            error: None,
            devices: vec![
                device(
                    "spk-1",
                    "Speakers",
                    "output",
                    true,
                    60,
                    self.speaker_muted.load(Ordering::SeqCst),
                ),
                device("mic-1", "Desk Mic", "input", true, 80, self.mic_muted()),
            ],
            sessions: vec![],
            defaults: None,
        }
    }
    fn device(&self, id: &str, _: Option<i32>, muted: Option<bool>, _: Option<i32>) -> bool {
        match id {
            "spk-1" => {
                if muted == Some(true)
                    && self.hold_speaker_mute
                    && !self.speaker_mute_entered.is_set()
                {
                    self.speaker_mute_entered.set();
                    self.release_speaker_mute.wait(Duration::from_secs(10));
                }
                if let Some(m) = muted {
                    self.speaker_muted.store(m, Ordering::SeqCst);
                }
                true
            }
            "mic-1" => {
                if muted == Some(true) && self.fail_mic_mute.load(Ordering::SeqCst) {
                    return false;
                }
                if let Some(m) = muted {
                    self.mic_muted.store(m, Ordering::SeqCst);
                }
                true
            }
            _ => false,
        }
    }
    fn session(&self, _: &str, _: Option<i32>, _: Option<bool>) -> bool {
        false
    }
    fn default_device(&self, _: &str, _: Option<i32>) -> bool {
        false
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
    fn media(&self) -> BoxFuture<'_, Result<Vec<DeckMedia>, String>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn media_action<'a>(&'a self, _: &'a str, _: &'a str, _: i64) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    fn play(&self, _: &str, _: &str, _: Option<&str>, _: i32) -> Result<(), String> {
        Ok(())
    }
    fn stop_sound(&self, _: Option<&str>) {}
    fn midi_ports(&self) -> Vec<DeckItem> {
        vec![]
    }
    fn midi(&self, _: i32, _: &str, _: i32) -> Result<(), String> {
        Ok(())
    }
}

fn panic_deck(native: &Arc<PanicNative>) -> (tempfile::TempDir, Arc<ControlDeck>) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = store(&dir, "{}");
    let deck = ControlDeck::new(cfg, native.clone(), DeckOptions::default());
    (dir, deck)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_that_begins_during_panic_cannot_reopen_the_microphone() {
    let native = Arc::new(PanicNative::new(false, true));
    let (_dir, deck) = panic_deck(&native);
    let panic = tokio::spawn({
        let deck = deck.clone();
        async move { act(&deck, "panic", None, None, None, None, None).await }
    });
    let entered = {
        let native = native.clone();
        tokio::task::spawn_blocking(move || {
            native.speaker_mute_entered.wait(Duration::from_secs(5))
        })
    };
    // panic is between its first and last mute
    assert!(entered.await.unwrap());
    let mut begin = tokio::spawn({
        let deck = deck.clone();
        async move { act(&deck, "ptt", Some("mic-1"), Some("begin"), None, None, None).await }
    });
    let _ = tokio::time::timeout(Duration::from_millis(300), &mut begin).await;
    native.release_speaker_mute.set();
    let panicked = tokio::time::timeout(Duration::from_secs(10), panic)
        .await
        .unwrap()
        .unwrap();
    assert!(panicked.ok);
    let hold = tokio::time::timeout(Duration::from_secs(10), begin)
        .await
        .unwrap()
        .unwrap();
    assert!(hold.ok);
    let lease = hold.lease.unwrap();
    assert!(
        act(
            &deck,
            "ptt",
            Some("mic-1"),
            Some("end"),
            None,
            None,
            Some(&lease)
        )
        .await
        .ok
    );
    assert!(native.mic_muted());
}

#[tokio::test]
async fn a_failed_panic_mute_stays_pending_and_blocks_push_to_talk_until_it_lands() {
    let native = Arc::new(PanicNative::new(true, false));
    let (_dir, deck) = panic_deck(&native);
    let hold = act(&deck, "ptt", Some("mic-1"), Some("begin"), None, None, None).await;
    assert!(hold.ok);
    assert!(!native.mic_muted());

    native.fail_mic_mute.store(true, Ordering::SeqCst);
    assert!(!act(&deck, "panic", None, None, None, None, None).await.ok);
    assert!(!native.mic_muted());
    assert!(
        !act(&deck, "ptt", Some("mic-1"), Some("begin"), None, None, None)
            .await
            .ok
    );

    native.fail_mic_mute.store(false, Ordering::SeqCst);
    // the hold ended with panic; the pending mute lands on the next push-to-talk request
    let lease = hold.lease.unwrap();
    assert!(
        !act(
            &deck,
            "ptt",
            Some("mic-1"),
            Some("end"),
            None,
            None,
            Some(&lease)
        )
        .await
        .ok
    );
    assert!(native.mic_muted());
}

#[tokio::test]
async fn a_failed_manual_mute_keeps_the_panic_mute_pending() {
    let native = Arc::new(PanicNative::new(true, false));
    let (_dir, deck) = panic_deck(&native);
    let hold = act(&deck, "ptt", Some("mic-1"), Some("begin"), None, None, None).await;
    assert!(hold.ok);

    native.fail_mic_mute.store(true, Ordering::SeqCst);
    assert!(!act(&deck, "panic", None, None, None, None, None).await.ok);
    // the user taps mute on the panel, and that fails too: the panic mute must stay pending
    assert!(
        !act(&deck, "device", Some("mic-1"), None, None, Some(true), None)
            .await
            .ok
    );
    assert!(
        !act(&deck, "ptt", Some("mic-1"), Some("begin"), None, None, None)
            .await
            .ok
    );
    assert!(!native.mic_muted());
}

/// `ControlDeckTests.FakePttDeckNative`: a speaker, a microphone and, optionally, a read-only HDMI output.
struct PttNative {
    mic_muted: AtomicBool,
    speaker_muted: AtomicBool,
    include_read_only: bool,
    disposed: AtomicBool,
}

impl PttNative {
    fn new(mic_muted: bool, include_read_only: bool) -> Arc<PttNative> {
        Arc::new(PttNative {
            mic_muted: AtomicBool::new(mic_muted),
            speaker_muted: AtomicBool::new(false),
            include_read_only,
            disposed: AtomicBool::new(false),
        })
    }
    fn mic_muted(&self) -> bool {
        self.mic_muted.load(Ordering::SeqCst)
    }
}

impl DeckNative for PttNative {
    fn audio(&self) -> DeckAudio {
        let mut devices = vec![
            device(
                "spk-1",
                "Speakers",
                "output",
                true,
                60,
                self.speaker_muted.load(Ordering::SeqCst),
            ),
            device("mic-1", "Desk Mic", "input", true, 80, self.mic_muted()),
        ];
        if self.include_read_only {
            let mut hdmi = device("hdmi-ro", "HDMI", "output", false, 100, false);
            hdmi.can_control = false;
            devices.push(hdmi);
        }
        DeckAudio {
            available: true,
            error: None,
            devices,
            sessions: vec![],
            defaults: None,
        }
    }
    fn device(&self, id: &str, _: Option<i32>, muted: Option<bool>, _: Option<i32>) -> bool {
        let target = match id {
            "spk-1" => &self.speaker_muted,
            "mic-1" => &self.mic_muted,
            _ => return false,
        };
        if let Some(m) = muted {
            target.store(m, Ordering::SeqCst);
        }
        true
    }
    fn session(&self, _: &str, _: Option<i32>, _: Option<bool>) -> bool {
        false
    }
    fn default_device(&self, _: &str, _: Option<i32>) -> bool {
        false
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
    fn media(&self) -> BoxFuture<'_, Result<Vec<DeckMedia>, String>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn media_action<'a>(&'a self, _: &'a str, _: &'a str, _: i64) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    fn play(&self, _: &str, _: &str, _: Option<&str>, _: i32) -> Result<(), String> {
        Ok(())
    }
    fn stop_sound(&self, _: Option<&str>) {}
    fn midi_ports(&self) -> Vec<DeckItem> {
        vec![]
    }
    fn midi(&self, _: i32, _: &str, _: i32) -> Result<(), String> {
        Ok(())
    }
    fn dispose(&self) {
        self.disposed.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn panic_mute_drops_active_ptt_leases_and_ptt_samples_uncached_audio_and_sweeps_on_dispose() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = store(&dir, "{}");

    let n1 = PttNative::new(false, true);
    let deck = ControlDeck::new(cfg.clone(), n1.clone(), DeckOptions::default());
    let begin = act(&deck, "ptt", Some("mic-1"), Some("begin"), None, None, None).await;
    assert!(begin.ok);
    let lease = begin.lease.unwrap();
    assert!(act(&deck, "panic", None, None, None, None, None).await.ok);
    assert!(n1.mic_muted());
    assert!(
        !act(
            &deck,
            "ptt",
            Some("mic-1"),
            Some("end"),
            None,
            None,
            Some(&lease)
        )
        .await
        .ok
    );
    assert!(n1.mic_muted());
    deck.dispose();

    let n2 = PttNative::new(true, false);
    let deck = ControlDeck::new(cfg.clone(), n2.clone(), DeckOptions::default());
    let _ = deck.snapshot().await;
    n2.mic_muted.store(false, Ordering::SeqCst);
    let lease = act(&deck, "ptt", Some("mic-1"), Some("begin"), None, None, None)
        .await
        .lease
        .unwrap();
    assert!(
        act(
            &deck,
            "ptt",
            Some("mic-1"),
            Some("end"),
            None,
            None,
            Some(&lease)
        )
        .await
        .ok
    );
    assert!(!n2.mic_muted());
    deck.dispose();

    let n3 = PttNative::new(true, false);
    let deck = ControlDeck::new(cfg, n3.clone(), DeckOptions::default());
    let _ = act(&deck, "ptt", Some("mic-1"), Some("begin"), None, None, None).await;
    assert!(!n3.mic_muted());
    deck.dispose();
    assert!(n3.mic_muted());
    assert!(n3.disposed.load(Ordering::SeqCst));
}
