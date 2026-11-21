//! The media data shared by the Windows and the Linux / macOS monitors (`Models.cs`), the deck seam the media routes
//! go through (`ControlDeck`'s radio stations, plan 6), and the Windows monitor (`MediaMonitor.cs`: the GSMTC session).

use std::sync::Arc;
use std::time::Instant;

/// What is playing. `sampled_at` dates `pos_ms`; the JSON adds `ageMs`, how old the position is when it is written.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaSnapshot {
    pub app: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    pub pos_ms: i64,
    pub dur_ms: i64,
    pub art_id: i32,
    pub can_seek: bool,
    /// When the position was read; None = not dated.
    pub sampled_at: Option<Instant>,
}

impl MediaSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        app: &str,
        title: &str,
        artist: &str,
        album: &str,
        playing: bool,
        pos_ms: i64,
        dur_ms: i64,
        art_id: i32,
        can_seek: bool,
    ) -> MediaSnapshot {
        MediaSnapshot {
            app: app.into(),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            playing,
            pos_ms,
            dur_ms,
            art_id,
            can_seek,
            sampled_at: None,
        }
    }

    /// How old `pos_ms` is now: the phone adds it to what it counts from the arrival, so a sample that waited for the
    /// next 1 s tick is not shown late.
    pub fn age_ms(&self) -> i64 {
        self.sampled_at
            .map_or(0, |t| t.elapsed().as_millis().min(i64::MAX as u128) as i64)
    }
}

impl serde::Serialize for MediaSnapshot {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("MediaSnapshot", 10)?;
        m.serialize_field("app", &self.app)?;
        m.serialize_field("title", &self.title)?;
        m.serialize_field("artist", &self.artist)?;
        m.serialize_field("album", &self.album)?;
        m.serialize_field("playing", &self.playing)?;
        m.serialize_field("posMs", &self.pos_ms)?;
        m.serialize_field("durMs", &self.dur_ms)?;
        m.serialize_field("artId", &self.art_id)?;
        m.serialize_field("canSeek", &self.can_seek)?;
        m.serialize_field("ageMs", &self.age_ms())?;
        m.end()
    }
}

/// A cover image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumArt {
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// The control deck's side of the media routes and the state: a playing (or paused) internet radio station stands in
/// for the system's media, and media changes take turns with the deck's own.
pub trait MediaDeck: Send + Sync {
    /// The media the phone sees: a radio station first, else the system's.
    fn effective_media(&self, system: Option<MediaSnapshot>) -> Option<MediaSnapshot>;
    /// Inside the media focus: Some(handled) when a station is the current media.
    fn try_control_radio<'a>(
        &'a self,
        action: &'a str,
        system: Option<&'a MediaSnapshot>,
    ) -> futures_util::future::BoxFuture<'a, Option<bool>>;
    fn stop_all_radios(&self);
    /// The deck's media focus (`_focusGate`): one media change at a time — radio starts and stops, the deck's media
    /// commands, `/api/media` and Spotify play — held across their awaits.
    fn media_focus(&self) -> Arc<tokio::sync::Mutex<()>>;
}

/// Runs `work` holding the deck's media focus.
pub async fn with_media_focus<T>(
    deck: &dyn MediaDeck,
    work: impl std::future::Future<Output = T>,
) -> T {
    let gate = deck.media_focus();
    let _focus = gate.lock().await;
    work.await
}

/// No control deck (until plan 6's deck replaces it): the system's media as it is.
#[derive(Default)]
pub struct NoDeck {
    focus: Arc<tokio::sync::Mutex<()>>,
}

impl MediaDeck for NoDeck {
    fn effective_media(&self, system: Option<MediaSnapshot>) -> Option<MediaSnapshot> {
        system
    }
    fn try_control_radio<'a>(
        &'a self,
        _action: &'a str,
        _system: Option<&'a MediaSnapshot>,
    ) -> futures_util::future::BoxFuture<'a, Option<bool>> {
        Box::pin(async { None })
    }
    fn stop_all_radios(&self) {}
    fn media_focus(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.focus.clone()
    }
}

/// A Windows media session's app id as a name: the usual players by name, else the file name of the id's first part.
pub fn friendly_app(id: &str) -> String {
    let l = id.to_lowercase();
    let known: [(&[&str], &str); 6] = [
        (&["spotify"], "Spotify"),
        (&["chrome"], "Chrome"),
        (&["firefox", "308046b0af4a39cb"], "Firefox"),
        (&["msedge"], "Edge"),
        (&["zune", "media"], "Media Player"),
        (&["vlc"], "VLC"),
    ];
    if let Some((_, name)) = known
        .iter()
        .find(|(parts, _)| parts.iter().any(|p| l.contains(p)))
    {
        return (*name).to_string();
    }
    // Path.GetFileNameWithoutExtension with Windows rules: after the last \ or / (or a leading drive's colon), before
    // the last dot
    let first = id.split('!').next().unwrap_or("");
    let file = first.rsplit(['\\', '/']).next().unwrap_or(first);
    let file = match file.as_bytes() {
        [d, b':', ..] if d.is_ascii_alphabetic() && file.len() == first.len() => &file[2..],
        _ => file,
    };
    file.rfind('.').map_or(file, |dot| &file[..dot]).to_string()
}

/// What the media routes and the state ask of the platform's monitor (a fake in tests).
pub trait SystemMedia: Send + Sync {
    fn snapshot(&self) -> Option<MediaSnapshot>;
    fn art(&self) -> Option<std::sync::Arc<AlbumArt>>;
    /// playpause, next, prev or seek (to `ms`); false when it did not work or the action is unknown.
    fn control(&self, action: &str, ms: Option<i64>) -> bool;
    fn pause(&self) -> bool;
}

/// `media` in the state: the deck's view of the system media (a playing station first).
pub struct MediaState {
    pub media: std::sync::Arc<dyn SystemMedia>,
    pub deck: std::sync::Arc<dyn MediaDeck>,
}

impl crate::state::StateSource for MediaState {
    fn snapshot(&self) -> serde_json::Value {
        serde_json::to_value(self.deck.effective_media(self.media.snapshot()))
            .unwrap_or(serde_json::Value::Null)
    }
}

/// The platform's monitor: GSMTC on Windows, playerctl / nowplaying-cli elsewhere.
#[cfg(not(windows))]
pub use crate::unix::media_monitor::MediaMonitor;
#[cfg(windows)]
pub use win::MediaMonitor;

macro_rules! system_media_for {
    ($t:ty) => {
        impl SystemMedia for $t {
            fn snapshot(&self) -> Option<MediaSnapshot> {
                <$t>::snapshot(self)
            }
            fn art(&self) -> Option<std::sync::Arc<AlbumArt>> {
                <$t>::art(self)
            }
            fn control(&self, action: &str, ms: Option<i64>) -> bool {
                <$t>::control(self, action, ms)
            }
            fn pause(&self) -> bool {
                <$t>::pause(self)
            }
        }
    };
}
system_media_for!(MediaMonitor);

/// Windows: the current GSMTC media session (Spotify, browsers, ...). One thread owns the WinRT objects: it reads the
/// session every second, and control and pause requests reach it over a channel.
#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use windows::Foundation::TypedEventHandler;
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSession as Session,
        GlobalSystemMediaTransportControlsSessionManager as Manager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    };
    use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};

    use super::{AlbumArt, MediaSnapshot, friendly_app};
    use crate::media_artwork_cache::{ArtError, MediaArtworkCache};

    const CATEGORY: &str = "PhoneBox.Agent.MediaMonitor";
    const MAX_THUMBNAIL: u64 = 8 * 1024 * 1024;
    /// .NET ticks (100 ns) from 1601-01-01 to 1970-01-01, and to 2001-01-01 (a timeline older than that is undated).
    const UNIX_EPOCH_FILETIME: i64 = 116_444_736_000_000_000;
    const YEAR_2001_FILETIME: i64 = 126_227_808_000_000_000;
    const VK_MEDIA_PLAY_PAUSE: u8 = 0xB3;

    enum Command {
        Control(String, Option<i64>, mpsc::Sender<bool>),
        Pause(mpsc::Sender<bool>),
    }

    pub struct MediaMonitor {
        snap: Mutex<Option<MediaSnapshot>>,
        art: MediaArtworkCache,
        /// Bumped by the session-changed and properties-changed events: the cover is read again at once.
        revision: Arc<AtomicI64>,
        commands: Mutex<Option<mpsc::Sender<Command>>>,
    }

    /// What the monitor thread holds: the manager, and the session whose property events it listens to.
    #[derive(Default)]
    struct Owned {
        manager: Option<(Manager, i64)>,
        observed: Option<(Session, i64)>,
    }

    impl MediaMonitor {
        pub fn new() -> Arc<MediaMonitor> {
            Arc::new(MediaMonitor {
                snap: Mutex::new(None),
                art: MediaArtworkCache::new(),
                revision: Arc::new(AtomicI64::new(0)),
                commands: Mutex::new(None),
            })
        }

        pub fn snapshot(&self) -> Option<MediaSnapshot> {
            self.snap.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        pub fn art(&self) -> Option<Arc<AlbumArt>> {
            self.art.current()
        }

        fn set_snapshot(&self, snap: Option<MediaSnapshot>) {
            *self.snap.lock().unwrap_or_else(|e| e.into_inner()) = snap;
        }

        pub fn start(self: &Arc<Self>, stop: crate::server::Stop) -> std::thread::JoinHandle<()> {
            let (tx, rx) = mpsc::channel();
            *self.commands.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
            let me = self.clone();
            std::thread::Builder::new()
                .name("media".into())
                .spawn(move || me.run(rx, stop))
                .expect("media thread")
        }

        fn ask(&self, make: impl FnOnce(mpsc::Sender<bool>) -> Command) -> bool {
            let Some(tx) = self
                .commands
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            else {
                return false;
            };
            let (reply, answer) = mpsc::channel();
            tx.send(make(reply)).is_ok() && answer.recv().unwrap_or(false)
        }

        /// playpause, next, prev or seek (to `ms`); without a session, playpause sends the media key (it wakes Spotify,
        /// for example).
        pub fn control(&self, action: &str, ms: Option<i64>) -> bool {
            self.ask(|reply| Command::Control(action.to_string(), ms, reply))
        }

        /// Pauses the session if it plays (a radio station starts); false when nothing played.
        pub fn pause(&self) -> bool {
            self.ask(Command::Pause)
        }

        fn run(self: Arc<Self>, rx: mpsc::Receiver<Command>, stop: crate::server::Stop) {
            use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
            // SAFETY: initializes COM for this thread, which owns every WinRT object below
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let cancel = AtomicBool::new(false);
            let mut owned = Owned::default();
            while !stop.is_stopped() {
                if let Err(e) = self
                    .ensure_manager(&mut owned)
                    .and_then(|m| self.poll(&m, &mut owned, &cancel))
                {
                    crate::log::debug(CATEGORY, &format!("media: {}", e.message()));
                    self.set_snapshot(None);
                }
                let deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() || stop.is_stopped() {
                        break;
                    }
                    match rx.recv_timeout(left) {
                        Ok(command) => self.handle(command, &mut owned, &cancel),
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
            cancel.store(true, Ordering::SeqCst);
            if let Some((m, token)) = owned.manager.take() {
                let _ = m.RemoveCurrentSessionChanged(token);
            }
            self.observe(None, &mut owned);
            self.art.clear();
        }

        fn ensure_manager(&self, owned: &mut Owned) -> windows::core::Result<Manager> {
            if let Some((m, _)) = &owned.manager {
                return Ok(m.clone());
            }
            let m = Manager::RequestAsync()?.join()?;
            let revision = self.revision.clone();
            let token = m.CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
                revision.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))?;
            owned.manager = Some((m.clone(), token));
            Ok(m)
        }

        fn observe(&self, session: Option<&Session>, owned: &mut Owned) {
            if owned.observed.as_ref().map(|(s, _)| s) == session {
                return;
            }
            if let Some((old, token)) = owned.observed.take() {
                let _ = old.RemoveMediaPropertiesChanged(token);
            }
            if let Some(s) = session {
                let revision = self.revision.clone();
                if let Ok(token) = s.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                    revision.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })) {
                    owned.observed = Some((s.clone(), token));
                }
            }
        }

        fn handle(&self, command: Command, owned: &mut Owned, cancel: &AtomicBool) {
            let session = owned
                .manager
                .as_ref()
                .and_then(|(m, _)| m.GetCurrentSession().ok());
            let (ok, reply) = match command {
                Command::Control(action, ms, reply) => {
                    let ok = match &session {
                        None if action == "playpause" => {
                            crate::key_sender::tap(VK_MEDIA_PLAY_PAUSE);
                            let _ = reply.send(true);
                            return;
                        }
                        None => false,
                        Some(s) => {
                            let op = match (action.as_str(), ms) {
                                ("playpause", _) => s.TryTogglePlayPauseAsync(),
                                ("next", _) => s.TrySkipNextAsync(),
                                ("prev", _) => s.TrySkipPreviousAsync(),
                                ("seek", Some(m)) => {
                                    s.TryChangePlaybackPositionAsync(m.saturating_mul(10_000))
                                }
                                _ => {
                                    let _ = reply.send(false);
                                    return;
                                }
                            };
                            op.and_then(|op| op.join()).unwrap_or(false)
                        }
                    };
                    (ok, reply)
                }
                Command::Pause(reply) => {
                    let playing = session.as_ref().is_some_and(|s| {
                        s.GetPlaybackInfo()
                            .and_then(|p| p.PlaybackStatus())
                            .is_ok_and(|st| st == Status::Playing)
                    });
                    if !playing {
                        let _ = reply.send(false);
                        return;
                    }
                    let ok = session
                        .as_ref()
                        .map(|s| s.TryPauseAsync().and_then(|op| op.join()).unwrap_or(false))
                        .unwrap_or(false);
                    (ok, reply)
                }
            };
            if let Some((m, _)) = owned.manager.clone() {
                let _ = self.poll(&m, owned, cancel);
            }
            let _ = reply.send(ok);
        }

        /// Reads the session; a new track, a pause or a seek reaches the phone now, not at its next whole-second state.
        fn poll(
            &self,
            m: &Manager,
            owned: &mut Owned,
            cancel: &AtomicBool,
        ) -> windows::core::Result<()> {
            let before = self.snapshot();
            let result = self.poll_core(m, owned, cancel);
            if crate::state_signal::media_changed(before.as_ref(), self.snapshot().as_ref()) {
                crate::state_signal::poke();
            }
            result
        }

        fn poll_core(
            &self,
            m: &Manager,
            owned: &mut Owned,
            cancel: &AtomicBool,
        ) -> windows::core::Result<()> {
            let session = m.GetCurrentSession().ok();
            self.observe(session.as_ref(), owned);
            let Some(s) = session else {
                self.set_snapshot(None);
                self.art.clear();
                return Ok(());
            };
            let revision = self.revision.load(Ordering::SeqCst);
            let props = s.TryGetMediaPropertiesAsync()?.join()?;
            let pb = s.GetPlaybackInfo()?;
            let tl = s.GetTimelineProperties()?;
            let playing = pb.PlaybackStatus()? == Status::Playing;
            let app_id = s.SourceAppUserModelId()?.to_string();
            let title = props.Title().map(|t| t.to_string()).unwrap_or_default();
            let artist = props.Artist().map(|t| t.to_string()).unwrap_or_default();
            let album = props
                .AlbumTitle()
                .map(|t| t.to_string())
                .unwrap_or_default();

            // refreshed after metadata events and while a new track's thumbnail is still settling
            let key = format!("{app_id}|{title}|{artist}");
            let thumb = props.Thumbnail().ok();
            let mut read = |_: &AtomicBool| -> Result<Option<AlbumArt>, ArtError> {
                thumb
                    .as_ref()
                    .map_or(Ok(None), read_thumbnail)
                    .map_err(|e| ArtError::Failed(e.message()))
            };
            let reader: Option<crate::media_artwork_cache::ArtRead> = if thumb.is_some() {
                Some(&mut read)
            } else {
                None
            };
            if let Err(ArtError::Failed(msg)) = self.art.update(&key, revision, reader, cancel) {
                crate::log::debug(CATEGORY, &format!("cover art: {msg}"));
            }

            let sampled_at = Instant::now();
            let mut pos = tl.Position()?.Duration;
            let updated = tl.LastUpdatedTime()?.UniversalTime;
            if playing && updated >= YEAR_2001_FILETIME {
                pos += now_filetime() - updated;
            }
            let dur = tl.EndTime()?.Duration - tl.StartTime()?.Duration;
            if dur > 0 && pos > dur {
                pos = dur;
            }
            pos = pos.max(0);
            let can_seek = pb.Controls()?.IsPlaybackPositionEnabled()?;
            let art_id = if self.art.current().is_some() {
                self.art.id()
            } else {
                0
            };
            self.set_snapshot(Some(MediaSnapshot {
                app: friendly_app(&app_id),
                title,
                artist,
                album,
                playing,
                pos_ms: pos / 10_000,
                dur_ms: dur.max(0) / 10_000,
                art_id,
                can_seek,
                sampled_at: Some(sampled_at),
            }));
            Ok(())
        }
    }

    fn now_filetime() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| (d.as_nanos() / 100) as i64)
            .unwrap_or(0)
            + UNIX_EPOCH_FILETIME
    }

    /// The session's thumbnail; one over 8 MiB is skipped (None, a failure the cache backs off from).
    fn read_thumbnail(
        thumb: &IRandomAccessStreamReference,
    ) -> windows::core::Result<Option<AlbumArt>> {
        let stream = thumb.OpenReadAsync()?.join()?;
        let size = stream.Size()?;
        if size > MAX_THUMBNAIL {
            return Ok(None);
        }
        let content_type = stream
            .ContentType()
            .map(|t| t.to_string())
            .unwrap_or_default();
        let reader = DataReader::CreateDataReader(&stream)?;
        let loaded = reader.LoadAsync(size as u32)?.join()?;
        let mut bytes = vec![0u8; loaded as usize];
        reader.ReadBytes(&mut bytes)?;
        Ok((!bytes.is_empty()).then(|| AlbumArt {
            bytes,
            mime: if content_type.is_empty() {
                "image/png".into()
            } else {
                content_type
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_app_names_follow_csharp() {
        for (id, name) in [
            ("Spotify.exe", "Spotify"),
            ("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify", "Spotify"),
            ("Chrome", "Chrome"),
            ("308046B0AF4A39CB", "Firefox"),
            ("firefox.exe", "Firefox"),
            ("MSEdge", "Edge"),
            (
                "Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic",
                "Media Player",
            ),
            ("Microsoft.Media.Player", "Media Player"),
            ("vlc.exe", "VLC"),
            ("Some.App_abc123!App", "Some"),
            (r"C:\Program Files\Foo\foo.exe", "foo"),
            ("music/player.bin", "player"),
            ("plain", "plain"),
            ("", ""),
        ] {
            assert_eq!(friendly_app(id), name, "{id}");
        }
    }

    #[test]
    fn the_json_is_the_records_properties_then_the_age() {
        let s = MediaSnapshot::new("Spotify", "One", "A", "B", true, 1000, 200000, 3, true);
        assert_eq!(s.age_ms(), 0);
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"app":"Spotify","title":"One","artist":"A","album":"B","playing":true,"posMs":1000,"durMs":200000,"artId":3,"canSeek":true,"ageMs":0}"#
        );
        let dated = MediaSnapshot {
            sampled_at: Some(Instant::now() - std::time::Duration::from_millis(1500)),
            ..s
        };
        let age = dated.age_ms();
        assert!((1500..3000).contains(&age), "{age}");
        let v = serde_json::to_value(&dated).unwrap();
        assert!(v["ageMs"].as_i64().unwrap() >= 1500);
    }

    #[tokio::test]
    async fn no_deck_passes_the_system_media_through() {
        let deck = NoDeck::default();
        let s = MediaSnapshot::new("VLC", "x", "", "", false, 0, 0, 0, false);
        assert_eq!(deck.effective_media(Some(s.clone())), Some(s.clone()));
        assert_eq!(deck.try_control_radio("next", Some(&s)).await, None);
        assert!(with_media_focus(&deck, async { true }).await);
        // one gate: media changes take turns
        assert!(Arc::ptr_eq(&deck.media_focus(), &deck.media_focus()));
    }
}
