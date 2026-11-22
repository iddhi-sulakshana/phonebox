//! `ControlDeck.Portable.cs`: the deck's native side on macOS and Linux. The system output is one device ("default",
//! from [`AudioControl`]) and the current media session one player ("current", from the media monitor); sounds and
//! radio play through an installed command-line player (ffplay, mpv, cvlc or vlc; afplay for files on macOS), one
//! process per sound, its whole tree killed on stop. Everything else answers false or empty. Built on every target, so
//! its tests also run on Windows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::future::BoxFuture;

use super::models::*;
use super::monotonic_ms;
use crate::audio_control::AudioControl;
use crate::media_monitor::SystemMedia;

/// How long a player lookup is reused.
const PLAYER_CACHE_MS: i64 = 15_000;
/// How often a player is checked for having ended by itself.
const EXIT_POLL: Duration = Duration::from_millis(100);

/// The players to look for, in order (afplay is added for files on macOS).
const PLAYERS: [&str; 4] = ["ffplay", "mpv", "cvlc", "vlc"];

/// Which player plays files (false) or streams (true): its kind and program. A seam for tests.
pub type FindPlayer = Arc<dyn Fn(bool) -> Option<(String, PathBuf)> + Send + Sync>;
type Callback = Arc<dyn Fn(&str) + Send + Sync>;
/// A player lookup: when it ran, and what it found.
type Lookup = (i64, Option<(String, PathBuf)>);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A playing sound's process: its watcher polls it under this lock, a stop kills it under the same lock, so its id is
/// never reused while either looks at it.
struct Sound {
    generation: u64,
    child: Arc<Mutex<std::process::Child>>,
    stopped: Arc<AtomicBool>,
}

#[derive(Default)]
struct Shared {
    sounds: Mutex<HashMap<String, Sound>>,
    sound_stopped: Mutex<Option<Callback>>,
}

impl Shared {
    /// Removes and kills the matching sounds (every sound for None).
    fn stop(&self, id: Option<&str>) {
        let stopped: Vec<Sound> = {
            let mut sounds = lock(&self.sounds);
            let keys: Vec<String> = sounds
                .keys()
                .filter(|k| id.is_none_or(|id| *k == id))
                .cloned()
                .collect();
            keys.iter().filter_map(|k| sounds.remove(k)).collect()
        };
        for sound in stopped {
            sound.stopped.store(true, Ordering::SeqCst);
            let mut child = lock(&sound.child);
            if matches!(child.try_wait(), Ok(None)) {
                crate::tool_process::kill_tree(Some(child.id()));
                let _ = child.wait();
            }
        }
    }
}

pub struct PortableDeckNative {
    audio: Arc<AudioControl>,
    media: Arc<dyn SystemMedia>,
    find_player: Option<FindPlayer>,
    shared: Arc<Shared>,
    generation: AtomicU64,
    /// The last lookups, for files and for streams: (when, what was found).
    players: Mutex<[Option<Lookup>; 2]>,
}

impl PortableDeckNative {
    pub fn new(
        audio: Arc<AudioControl>,
        media: Arc<dyn SystemMedia>,
        find_player: Option<FindPlayer>,
    ) -> PortableDeckNative {
        PortableDeckNative {
            audio,
            media,
            find_player,
            shared: Default::default(),
            generation: AtomicU64::new(0),
            players: Mutex::new([None, None]),
        }
    }

    /// The player for files or streams: the seam, else the installed one (looked up at most every 15 s).
    fn resolve_player(&self, stream: bool) -> Option<(String, PathBuf)> {
        if let Some(find) = &self.find_player {
            return find(stream);
        }
        let now = monotonic_ms();
        let mut players = lock(&self.players);
        let slot = &mut players[usize::from(stream)];
        if let Some((at, found)) = slot
            && now - *at < PLAYER_CACHE_MS
        {
            return found.clone();
        }
        let found = installed_player(stream);
        *slot = Some((now, found.clone()));
        found
    }
}

/// The first installed player (PATH, then the Homebrew and system folders); afplay for files on macOS last.
fn installed_player(stream: bool) -> Option<(String, PathBuf)> {
    for tool in PLAYERS {
        if let Some(found) = crate::tool_process::find(tool) {
            return Some((tool.to_string(), found));
        }
    }
    if !stream && cfg!(target_os = "macos") {
        return crate::tool_process::find("afplay").map(|found| ("afplay".to_string(), found));
    }
    None
}

/// `BuildStartInfo`'s arguments: each player's quiet, window-less form at the volume (0–100; a fraction for afplay and
/// vlc).
pub fn start_args(kind: &str, target: &str, volume: i32) -> Vec<String> {
    let clamped = volume.clamp(0, 100);
    let fraction = crate::dotnet_text::format_custom(f64::from(clamped) / 100.0, 2, false);
    let args: Vec<String> = match kind {
        "ffplay" => vec![
            "-nodisp".into(),
            "-autoexit".into(),
            "-vn".into(),
            "-sn".into(),
            "-loglevel".into(),
            "error".into(),
            "-volume".into(),
            clamped.to_string(),
            "-i".into(),
            target.into(),
        ],
        "mpv" => vec![
            "--no-config".into(),
            "--no-terminal".into(),
            "--no-video".into(),
            "--really-quiet".into(),
            format!("--volume={clamped}"),
            "--".into(),
            target.into(),
        ],
        "afplay" => vec!["-v".into(), fraction, target.into()],
        _ => vec![
            "--intf".into(),
            "dummy".into(),
            "--no-one-instance".into(),
            "--play-and-exit".into(),
            "--no-video".into(),
            "--quiet".into(),
            format!("--gain={fraction}"),
            "--".into(),
            target.into(),
        ],
    };
    args
}

/// An absolute http(s) URI (a stream); anything else is a file.
fn is_stream(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    ["http://", "https://"]
        .iter()
        .any(|scheme| lower.starts_with(scheme) && lower.len() > scheme.len())
}

impl DeckNative for PortableDeckNative {
    fn on_sound_stopped(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        *lock(&self.shared.sound_stopped) = Some(callback);
    }

    fn audio(&self) -> DeckAudio {
        match self.audio.snapshot() {
            Some(a) => DeckAudio {
                available: true,
                error: None,
                devices: vec![DeckDevice {
                    id: "default".into(),
                    name: "System output".into(),
                    direction: "output".into(),
                    default: true,
                    volume: a.volume,
                    muted: a.muted,
                    peak: 0.0,
                    channels: 1,
                    can_control: a.can_control,
                    channel_volumes: None,
                }],
                sessions: vec![],
                defaults: None,
            },
            None => DeckAudio {
                available: false,
                error: Some("Audio backend is unavailable".into()),
                devices: vec![],
                sessions: vec![],
                defaults: None,
            },
        }
    }

    fn device(
        &self,
        id: &str,
        volume: Option<i32>,
        muted: Option<bool>,
        channel: Option<i32>,
    ) -> bool {
        if id != "default" || channel.is_some() {
            return false;
        }
        let Some(current) = self.audio.current().filter(|a| a.can_control) else {
            return false;
        };
        if let Some(v) = volume
            && !self.audio.set_volume(v)
        {
            return false;
        }
        match muted {
            None => true,
            Some(m) if m == current.muted => true,
            Some(_) => self.audio.toggle_mute(),
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

    fn action_state(&self, action: &crate::config::DeckAction) -> Option<String> {
        if !matches!(action.kind.as_str(), "sound" | "radio") {
            return None;
        }
        self.resolve_player(action.kind == "radio")?;
        let playing = lock(&self.shared.sounds).contains_key(&action.id);
        Some(if playing { "Playing" } else { "Stopped" }.into())
    }

    fn media(&self) -> BoxFuture<'_, Result<Vec<DeckMedia>, String>> {
        let current = self.media.snapshot().map(|m| DeckMedia {
            id: "current".into(),
            name: m.app,
            title: m.title,
            playing: m.playing,
            position: m.pos_ms,
            duration: m.dur_ms,
            can_seek: m.can_seek,
            can_shuffle: false,
            can_repeat: false,
            shuffle: false,
            repeat: "None".into(),
        });
        Box::pin(async move { Ok(current.into_iter().collect()) })
    }

    fn media_action<'a>(&'a self, id: &'a str, action: &'a str, value: i64) -> BoxFuture<'a, bool> {
        if id != "current" {
            return Box::pin(async { false });
        }
        let (media, action) = (self.media.clone(), action.to_string());
        Box::pin(async move {
            // the media monitor runs a helper (playerctl, nowplaying-cli): off the async workers
            tokio::task::spawn_blocking(move || {
                if action == "pause" {
                    media.pause()
                } else {
                    media.control(&action, Some(value))
                }
            })
            .await
            .unwrap_or(false)
        })
    }

    fn pause_current_media(&self) -> BoxFuture<'_, bool> {
        let media = self.media.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || media.pause())
                .await
                .unwrap_or(false)
        })
    }

    fn play(&self, id: &str, file: &str, _device: Option<&str>, volume: i32) -> Result<(), String> {
        if file.trim().is_empty() {
            return Err("Playback target is empty".into());
        }
        let stream = is_stream(file);
        let target = if stream {
            file.to_string()
        } else {
            if !Path::new(file).is_file() {
                return Err("Sound file not found".into());
            }
            std::path::absolute(file)
                .unwrap_or_else(|_| PathBuf::from(file))
                .to_string_lossy()
                .into_owned()
        };
        let (kind, program) = self
            .resolve_player(stream)
            .ok_or("Install ffplay, mpv, or vlc for audio playback")?;
        let mut command = std::process::Command::new(&program);
        command
            .args(start_args(&kind, &target, volume))
            .current_dir(crate::app_paths::neutral_cwd())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }

        // one player per sound: the one playing under this id stops first
        self.shared.stop(Some(id));
        let child = command
            .spawn()
            .map_err(|_| "Could not start audio player".to_string())?;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let child = Arc::new(Mutex::new(child));
        let stopped = Arc::new(AtomicBool::new(false));
        lock(&self.shared.sounds).insert(
            id.to_string(),
            Sound {
                generation,
                child: child.clone(),
                stopped: stopped.clone(),
            },
        );
        // the player ended by itself (the stream dropped, the file finished): the deck shows the sound stopped
        let (shared, id) = (self.shared.clone(), id.to_string());
        std::thread::Builder::new()
            .name("deck-player".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(EXIT_POLL);
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    if !matches!(lock(&child).try_wait(), Ok(None)) {
                        break;
                    }
                }
                let ended = {
                    let mut sounds = lock(&shared.sounds);
                    let current = sounds.get(&id).is_some_and(|s| s.generation == generation);
                    if current {
                        sounds.remove(&id);
                    }
                    current
                };
                let callback = lock(&shared.sound_stopped).clone();
                if ended && let Some(callback) = callback {
                    callback(&id);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn stop_sound(&self, id: Option<&str>) {
        self.shared.stop(id);
    }

    fn midi_ports(&self) -> Vec<DeckItem> {
        vec![]
    }
    fn midi(&self, _: i32, _: &str, _: i32) -> Result<(), String> {
        Err("Native MIDI requires Windows".into())
    }
    fn dispose(&self) {
        self.shared.stop(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_control::AudioSnapshot;
    use crate::media_monitor::{AlbumArt, MediaSnapshot};

    #[derive(Default)]
    struct Media {
        calls: Mutex<Vec<String>>,
        playing: Option<MediaSnapshot>,
    }

    impl SystemMedia for Media {
        fn snapshot(&self) -> Option<MediaSnapshot> {
            self.playing.clone()
        }
        fn art(&self) -> Option<Arc<AlbumArt>> {
            None
        }
        fn control(&self, action: &str, ms: Option<i64>) -> bool {
            lock(&self.calls).push(format!("{action}:{}", ms.unwrap_or(-1)));
            true
        }
        fn pause(&self) -> bool {
            lock(&self.calls).push("pause".into());
            true
        }
    }

    fn native(find: Option<FindPlayer>) -> PortableDeckNative {
        let audio = AudioControl::with(Arc::new(|| None), None, None);
        PortableDeckNative::new(audio, Arc::new(Media::default()), find)
    }

    fn player(kind: &'static str, path: &'static str) -> Option<FindPlayer> {
        Some(Arc::new(move |_| {
            Some((kind.to_string(), PathBuf::from(path)))
        }))
    }

    fn radio() -> crate::config::DeckAction {
        crate::config::DeckAction {
            id: "radio-1".into(),
            kind: "radio".into(),
            label: "Test FM".into(),
            target: "https://stream.example/live.mp3".into(),
            ..Default::default()
        }
    }

    #[test]
    fn build_start_info_formats_each_player_and_volume_deterministically() {
        assert_eq!(
            start_args("ffplay", "https://stream.example/live.mp3", 35),
            [
                "-nodisp",
                "-autoexit",
                "-vn",
                "-sn",
                "-loglevel",
                "error",
                "-volume",
                "35",
                "-i",
                "https://stream.example/live.mp3"
            ]
        );
        assert_eq!(
            start_args("mpv", "/tmp/bell.wav", 120),
            [
                "--no-config",
                "--no-terminal",
                "--no-video",
                "--really-quiet",
                "--volume=100",
                "--",
                "/tmp/bell.wav"
            ]
        );
        assert_eq!(
            start_args("cvlc", "https://stream.example/live", 35),
            [
                "--intf",
                "dummy",
                "--no-one-instance",
                "--play-and-exit",
                "--no-video",
                "--quiet",
                "--gain=0.35",
                "--",
                "https://stream.example/live"
            ]
        );
        assert_eq!(
            start_args("afplay", "/tmp/bell.wav", 50),
            ["-v", "0.5", "/tmp/bell.wav"]
        );
    }

    #[test]
    fn action_state_reflects_resolved_player_availability() {
        let unavailable = native(Some(Arc::new(|_| None)));
        assert_eq!(unavailable.action_state(&radio()), None);
        let available = native(player("ffplay", "/usr/bin/ffplay"));
        assert_eq!(available.action_state(&radio()).as_deref(), Some("Stopped"));
    }

    #[test]
    fn play_rejects_missing_files_and_invalid_targets_before_spawning() {
        let native = native(player("ffplay", "/usr/bin/ffplay"));
        assert_eq!(
            native.play("s1", "  ", None, 50).unwrap_err(),
            "Playback target is empty"
        );
        let missing =
            std::env::temp_dir().join(format!("missing-{}.wav", super::super::leases::nonce()));
        assert_eq!(
            native
                .play("s1", &missing.to_string_lossy(), None, 50)
                .unwrap_err(),
            "Sound file not found"
        );
    }

    #[test]
    fn play_throws_not_supported_when_no_player_is_installed() {
        let native = native(Some(Arc::new(|_| None)));
        assert_eq!(
            native
                .play("r1", "https://stream.example/live.mp3", None, 60)
                .unwrap_err(),
            "Install ffplay, mpv, or vlc for audio playback"
        );
    }

    // Rust-only: the system output and the current player as the deck sees them; a player that ends by itself and one
    // that is stopped.

    #[tokio::test]
    async fn the_system_output_and_the_current_player() {
        let level = Arc::new(Mutex::new((40, false)));
        let read = {
            let level = level.clone();
            Arc::new(move || {
                let (volume, muted) = *lock(&level);
                Some(AudioSnapshot::new(volume, muted, true))
            })
        };
        let set = {
            let level = level.clone();
            Arc::new(move |v: i32| {
                lock(&level).0 = v;
                Some(v)
            })
        };
        let toggle = {
            let level = level.clone();
            Arc::new(move || {
                let mut level = lock(&level);
                level.1 = !level.1;
                true
            })
        };
        let audio = AudioControl::with(read, Some(set), Some(toggle));
        let media = Arc::new(Media {
            calls: Mutex::default(),
            playing: Some(MediaSnapshot::new(
                "Spotify", "Song", "A", "", true, 5, 100, 0, true,
            )),
        });
        // the deck's audio is the cached reading (refreshed in the background): read it once first
        let _ = audio.current();
        let native = PortableDeckNative::new(audio, media.clone(), None);
        let devices = native.audio().devices;
        assert_eq!(devices.len(), 1);
        assert_eq!(
            (devices[0].id.as_str(), devices[0].volume, devices[0].muted),
            ("default", 40, false)
        );
        assert!(!native.device("other", Some(10), None, None));
        assert!(!native.device("default", Some(10), None, Some(0)));
        assert!(native.device("default", Some(25), Some(true), None));
        assert_eq!(*lock(&level), (25, true));
        let players = native.media().await.unwrap();
        assert_eq!(
            (
                players[0].id.as_str(),
                players[0].name.as_str(),
                players[0].position
            ),
            ("current", "Spotify", 5)
        );
        assert!(!native.media_action("other", "next", 0).await);
        assert!(native.media_action("current", "seek", 7).await);
        assert!(native.pause_current_media().await);
        assert_eq!(*lock(&media.calls), ["seek:7", "pause"]);
        assert_eq!(
            native.midi(0, "90 40", 1).unwrap_err(),
            "Native MIDI requires Windows"
        );
        assert!(
            native.monitors().is_empty()
                && native.power_plans().is_empty()
                && native.midi_ports().is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_player_that_ends_by_itself_says_so_and_a_stopped_one_does_not() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let quick = script("quick", "exit 0");
        let slow = script("slow", "sleep 30");
        let program = Arc::new(Mutex::new(quick));
        let find: FindPlayer = {
            let program = program.clone();
            Arc::new(move |_| Some(("mpv".to_string(), lock(&program).clone())))
        };
        let native = native(Some(find));
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tx = Mutex::new(tx);
        native.on_sound_stopped(Arc::new(move |id| {
            let _ = lock(&tx).send(id.to_string());
        }));

        native
            .play("r1", "https://stream.example/live", None, 50)
            .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "r1");
        assert_eq!(native.action_state(&radio()).as_deref(), Some("Stopped"));

        *lock(&program) = slow;
        let mut station = radio();
        station.id = "r2".into();
        native
            .play("r2", "https://stream.example/live", None, 50)
            .unwrap();
        assert_eq!(native.action_state(&station).as_deref(), Some("Playing"));
        native.stop_sound(Some("r2"));
        assert_eq!(native.action_state(&station).as_deref(), Some("Stopped"));
        assert!(rx.recv_timeout(Duration::from_millis(400)).is_err());
    }
}
