//! `WindowsDeckNative`: the deck's [`DeckNative`] on Windows.

use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;

use super::super::models::*;
use super::audio;
use crate::media_monitor::SystemMedia;

type Callback = Arc<dyn Fn(&str) + Send + Sync>;

pub struct WindowsDeckNative {
    /// The agent's own media monitor (pausing the current media goes through it, as in C#).
    media: Option<Arc<dyn SystemMedia>>,
    names: audio::Names,
    /// One lock for the audio calls, as C#'s `_gate`.
    gate: Mutex<()>,
    sound_stopped: Mutex<Option<Callback>>,
}

impl WindowsDeckNative {
    pub fn new(media: Option<Arc<dyn SystemMedia>>) -> WindowsDeckNative {
        WindowsDeckNative {
            media,
            names: Mutex::default(),
            gate: Mutex::new(()),
            sound_stopped: Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.gate.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl DeckNative for WindowsDeckNative {
    fn on_sound_stopped(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        *self.sound_stopped.lock().unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    fn audio(&self) -> DeckAudio {
        let _gate = self.lock();
        audio::read(&self.names)
    }

    fn device(
        &self,
        id: &str,
        volume: Option<i32>,
        muted: Option<bool>,
        channel: Option<i32>,
    ) -> bool {
        let _gate = self.lock();
        audio::set_device(id, volume, muted, channel)
    }

    fn session(&self, id: &str, volume: Option<i32>, muted: Option<bool>) -> bool {
        let _gate = self.lock();
        audio::set_session(id, volume, muted)
    }

    fn default_device(&self, id: &str, role: Option<i32>) -> bool {
        audio::set_default(&self.names, id, role)
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

    fn pause_current_media(&self) -> BoxFuture<'_, bool> {
        let media = self.media.clone();
        Box::pin(async move {
            match media {
                Some(media) => tokio::task::spawn_blocking(move || media.pause())
                    .await
                    .unwrap_or(false),
                None => false,
            }
        })
    }

    fn play(&self, _: &str, _: &str, _: Option<&str>, _: i32) -> Result<(), String> {
        Err("Playback is unavailable".into())
    }

    fn stop_sound(&self, _: Option<&str>) {}

    fn midi_ports(&self) -> Vec<DeckItem> {
        vec![]
    }

    fn midi(&self, _: i32, _: &str, _: i32) -> Result<(), String> {
        Err("MIDI is unavailable".into())
    }
}
