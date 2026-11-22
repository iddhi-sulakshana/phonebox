//! Sounds and radio stations on Windows (`ControlDeck.Native.cs:283-377`, `ControlDeck.Hls.cs:92-176`). Every file,
//! stream and HLS playlist plays through a WinRT `MediaPlayer` of its own (the plan's ruling, instead of Media
//! Foundation + WASAPI beside a second HLS player), kept out of the system media controls and on the chosen output. A
//! player that ends or fails stops its sound and tells the deck, on a thread of its own and outside every lock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use windows::Devices::Enumeration::DeviceInformation;
use windows::Foundation::{TypedEventHandler, Uri};
use windows::Media::Core::MediaSource;
use windows::Media::Devices::MediaDevice;
use windows::Media::Playback::{
    IMediaPlaybackSource, MediaPlayer, MediaPlayerAudioCategory, MediaPlayerFailedEventArgs,
};
use windows::Media::Streaming::Adaptive::{AdaptiveMediaSource, AdaptiveMediaSourceCreationStatus};
use windows::core::HSTRING;
use windows_future::{IAsyncAction, IAsyncOperation};

use super::super::fetch::Fetch;
use super::super::hls;
use super::{PlayTarget, adaptive_status_name, audio, play_target};

type Callback = Arc<dyn Fn(&str) + Send + Sync>;

/// How long a source may take to open (C#: 10 s for the adaptive source).
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long finding the output may take (C#: 2 s).
const DEVICE_TIMEOUT: Duration = Duration::from_secs(2);
/// .NET's text for a wait that ran out (C# waited through `AsTask(token)`).
const CANCELED: &str = "A task was canceled.";
const CATEGORY: &str = "ControlDeck";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn text(e: windows::core::Error) -> String {
    e.message()
}

/// A WinRT operation's result within `timeout`; past it the operation is cancelled.
fn wait<T>(op: IAsyncOperation<T>, timeout: Duration) -> Result<T, String>
where
    T: windows::core::RuntimeType + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    op.when(move |result| {
        let _ = tx.send(result);
    })
    .map_err(text)?;
    match rx.recv_timeout(timeout) {
        Ok(result) => result.map_err(text),
        Err(_) => {
            let _ = op.Cancel();
            Err(CANCELED.into())
        }
    }
}

fn wait_action(op: IAsyncAction, timeout: Duration) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    op.when(move |result| {
        let _ = tx.send(result);
    })
    .map_err(text)?;
    match rx.recv_timeout(timeout) {
        Ok(result) => result.map_err(text),
        Err(_) => {
            let _ = op.Cancel();
            Err(CANCELED.into())
        }
    }
}

#[derive(Default)]
struct Flags {
    stopped: AtomicBool,
    disposed: AtomicBool,
}

/// One playing sound: its player and what it plays (`HlsPlayer.Handle`).
struct Handle {
    player: MediaPlayer,
    source: MediaSource,
    adaptive: Option<AdaptiveMediaSource>,
    events: (i64, i64),
    flags: Arc<Flags>,
}

impl Handle {
    /// Pause, detach, close; errors are ignored and a second call does nothing.
    fn dispose(&self) {
        if self.flags.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.flags.stopped.store(true, Ordering::SeqCst);
        let _ = self.player.Pause();
        let _ = self.player.RemoveMediaEnded(self.events.0);
        let _ = self.player.RemoveMediaFailed(self.events.1);
        let _ = self.player.SetSource(None::<&IMediaPlaybackSource>);
        let _ = self.player.Close();
        let _ = self.source.Close();
        if let Some(adaptive) = &self.adaptive {
            let _ = adaptive.Close();
        }
    }
}

struct Sound {
    generation: u64,
    handle: Handle,
}

/// What a player's ended and failed events call: once, and never after the sound was stopped (`NotifyStopped`).
#[derive(Clone)]
struct Notify {
    owner: Weak<Playback>,
    id: String,
    generation: u64,
    flags: Arc<Flags>,
}

impl Notify {
    fn fire(&self) {
        if self.flags.stopped.swap(true, Ordering::SeqCst)
            || self.flags.disposed.load(Ordering::SeqCst)
        {
            return;
        }
        let (owner, id, generation) = (self.owner.clone(), self.id.clone(), self.generation);
        // off the player's event thread, as C#'s Task.Run
        let _ = std::thread::Builder::new()
            .name("deck-sound-ended".into())
            .spawn(move || {
                if let Some(owner) = owner.upgrade() {
                    owner.ended(&id, generation);
                }
            });
    }
}

#[derive(Default)]
pub(super) struct Playback {
    sounds: Mutex<HashMap<String, Sound>>,
    generation: AtomicU64,
    sound_stopped: Mutex<Option<Callback>>,
    /// The HLS probe's client (C#'s shared HttpClient), made on first use.
    fetch: OnceLock<Fetch>,
}

impl Playback {
    pub(super) fn on_sound_stopped(&self, callback: Callback) {
        *lock(&self.sound_stopped) = Some(callback);
    }

    pub(super) fn playing(&self, id: &str) -> bool {
        lock(&self.sounds).contains_key(id)
    }

    /// None stops every sound.
    pub(super) fn stop(&self, id: Option<&str>) {
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
            sound.handle.dispose();
        }
    }

    /// A player ended or failed: stop it if it is still this id's sound, then tell the deck.
    fn ended(&self, id: &str, generation: u64) {
        let removed = {
            let mut sounds = lock(&self.sounds);
            if sounds.get(id).is_some_and(|s| s.generation == generation) {
                sounds.remove(id)
            } else {
                None
            }
        };
        let Some(sound) = removed else {
            return;
        };
        sound.handle.dispose();
        let callback = lock(&self.sound_stopped).clone();
        if let Some(callback) = callback {
            callback(id);
        }
    }

    /// Starts sound `id` (the one playing under it stops first). Blocks while the source opens.
    pub(super) fn play(
        self: &Arc<Self>,
        id: &str,
        file: &str,
        device: Option<&str>,
        volume: i32,
    ) -> Result<(), String> {
        self.stop(Some(id));
        let device = device.filter(|d| !d.trim().is_empty());
        if device.is_some_and(|d| !audio::device_active(d)) {
            return Err("Selected audio output device is unavailable".into());
        }
        let (source, adaptive) = self.open(file)?;
        let is_hls = adaptive.is_some();
        let flags = Arc::new(Flags::default());
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let notify = Notify {
            owner: Arc::downgrade(self),
            id: id.to_string(),
            generation,
            flags: flags.clone(),
        };
        let (player, events) = match player(&source, device, volume, notify) {
            Ok(made) => made,
            Err(e) => {
                let _ = source.Close();
                if let Some(adaptive) = adaptive {
                    let _ = adaptive.Close();
                }
                return Err(e);
            }
        };
        let handle = Handle {
            player: player.clone(),
            source,
            adaptive,
            events,
            flags: flags.clone(),
        };
        let previous = {
            let mut sounds = lock(&self.sounds);
            // another play of this id may have finished meanwhile (C# stopped it again under its gate)
            let previous = sounds.remove(id);
            if !flags.stopped.load(Ordering::SeqCst) {
                sounds.insert(id.to_string(), Sound { generation, handle });
                previous
            } else {
                drop(sounds);
                handle.dispose();
                if let Some(previous) = previous {
                    previous.handle.dispose();
                }
                return Err(if is_hls {
                    "HLS playback failed"
                } else {
                    "Playback failed"
                }
                .into());
            }
        };
        if let Some(previous) = previous {
            previous.handle.dispose();
        }
        // from here a failure arrives as an event and stops the sound
        if let Err(e) = player.Play() {
            let removed = {
                let mut sounds = lock(&self.sounds);
                if sounds.get(id).is_some_and(|s| s.generation == generation) {
                    sounds.remove(id)
                } else {
                    None
                }
            };
            if let Some(sound) = removed {
                sound.handle.dispose();
            }
            return Err(text(e));
        }
        Ok(())
    }

    /// The source for a target: an `.m3u8` address as HLS; another address as a stream, or as HLS when it does not
    /// open and serves a manifest; anything else as a file.
    fn open(&self, file: &str) -> Result<(MediaSource, Option<AdaptiveMediaSource>), String> {
        match play_target(file) {
            PlayTarget::Hls(url) => adaptive(url.as_str()),
            PlayTarget::Stream(url) => match source(url.as_str()) {
                Ok(source) => Ok((source, None)),
                // C#: `catch when (ProbeManifest(url))`, so the HLS player's own error is the one reported
                Err(_) if self.probe(&url) => adaptive(url.as_str()),
                Err(e) => Err(e),
            },
            PlayTarget::File(path) => {
                let full = std::path::absolute(&path).unwrap_or(path);
                if !full.is_file() {
                    return Err(format!("Could not find file '{}'.", full.display()));
                }
                let url = reqwest::Url::from_file_path(&full)
                    .map_err(|_| format!("Could not find file '{}'.", full.display()))?;
                Ok((source(url.as_str())?, None))
            }
        }
    }

    /// Whether an address that did not open serves an HLS manifest (4 s, 512 bytes).
    fn probe(&self, url: &reqwest::Url) -> bool {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        let fetch = self.fetch.get_or_init(hls::hls_fetch).clone();
        // a thread of its own, so the wait never runs inside an async task
        std::thread::scope(|s| {
            s.spawn(|| runtime.block_on(hls::probe_manifest(url, &fetch, hls::PROBE_TIMEOUT)))
                .join()
                .unwrap_or(false)
        })
    }
}

/// A file or stream source, opened (so a missing or undecodable one fails here, as C#'s reader did).
fn source(url: &str) -> Result<MediaSource, String> {
    let uri = Uri::CreateUri(&HSTRING::from(url)).map_err(text)?;
    let source = MediaSource::CreateFromUri(&uri).map_err(text)?;
    match source
        .OpenAsync()
        .map_err(text)
        .and_then(|op| wait_action(op, OPEN_TIMEOUT))
    {
        Ok(()) => Ok(source),
        Err(e) => {
            let _ = source.Close();
            Err(e)
        }
    }
}

/// An HLS source (`HlsPlayer.Open`'s first steps).
fn adaptive(url: &str) -> Result<(MediaSource, Option<AdaptiveMediaSource>), String> {
    let uri = Uri::CreateUri(&HSTRING::from(url)).map_err(text)?;
    let created = wait(
        AdaptiveMediaSource::CreateFromUriAsync(&uri).map_err(text)?,
        OPEN_TIMEOUT,
    )?;
    let status = created.Status().map_err(text)?;
    let adaptive = match created.MediaSource() {
        Ok(adaptive) if status == AdaptiveMediaSourceCreationStatus::Success => adaptive,
        _ => {
            return Err(format!(
                "HLS adaptive source failed ({})",
                adaptive_status_name(status.0)
            ));
        }
    };
    match MediaSource::CreateFromAdaptiveMediaSource(&adaptive) {
        Ok(source) => Ok((source, Some(adaptive))),
        Err(e) => {
            let _ = adaptive.Close();
            Err(text(e))
        }
    }
}

/// A player for the source: media category, the volume, out of the system media controls, on the chosen output (or
/// the default one, with a warning, when it is not found), its ended and failed events wired.
fn player(
    source: &MediaSource,
    device: Option<&str>,
    volume: i32,
    notify: Notify,
) -> Result<(MediaPlayer, (i64, i64)), String> {
    let player = MediaPlayer::new().map_err(text)?;
    let setup = || -> windows::core::Result<(i64, i64)> {
        player.SetAudioCategory(MediaPlayerAudioCategory::Media)?;
        player.SetVolume(f64::from(volume.clamp(0, 100)) / 100.0)?;
        player.CommandManager()?.SetIsEnabled(false)?;
        if let Some(device) = device {
            select_output(&player, device);
        }
        let on_end = notify.clone();
        let ended = player.MediaEnded(&TypedEventHandler::new(move |_, _| {
            on_end.fire();
            Ok(())
        }))?;
        let on_fail = notify;
        let failed = player.MediaFailed(&TypedEventHandler::new(
            move |_, args: windows::core::Ref<MediaPlayerFailedEventArgs>| {
                if let Some(args) = args.as_ref() {
                    let message = args
                        .ErrorMessage()
                        .map(|m| m.to_string())
                        .unwrap_or_default();
                    crate::log::debug(
                        CATEGORY,
                        &format!(
                            "playback failed ({:?}): {message}",
                            args.Error().map(|e| e.0)
                        ),
                    );
                }
                on_fail.fire();
                Ok(())
            },
        ))?;
        player.SetSource(source)?;
        Ok((ended, failed))
    };
    match setup() {
        Ok(events) => Ok((player, events)),
        Err(e) => {
            let _ = player.Close();
            Err(text(e))
        }
    }
}

/// The render endpoint whose interface path names this Core Audio device.
fn select_output(player: &MediaPlayer, device: &str) {
    let found = (|| -> Result<Option<DeviceInformation>, String> {
        let selector = MediaDevice::GetAudioRenderSelector().map_err(text)?;
        let renders = wait(
            DeviceInformation::FindAllAsyncAqsFilter(&selector).map_err(text)?,
            DEVICE_TIMEOUT,
        )?;
        Ok(renders.into_iter().find(|d| {
            d.Id()
                .is_ok_and(|i| hls::matches_audio_render_device(Some(&i.to_string()), Some(device)))
        }))
    })();
    match found {
        Ok(Some(output)) => {
            if let Err(e) = player.SetAudioDevice(&output) {
                crate::log::warn(
                    CATEGORY,
                    &format!(
                        "Failed to select output device {device}; using default output: {}",
                        text(e)
                    ),
                );
            }
        }
        Ok(None) => crate::log::warn(
            CATEGORY,
            &format!(
                "Output device {device} was not found among active render endpoints; using default output"
            ),
        ),
        Err(e) => crate::log::warn(
            CATEGORY,
            &format!("Failed to select output device {device}; using default output: {e}"),
        ),
    }
}
