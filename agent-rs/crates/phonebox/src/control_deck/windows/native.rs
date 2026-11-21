//! `WindowsDeckNative`: the deck's [`DeckNative`] on Windows.

use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;

use super::super::models::*;
use super::{audio, hardware, listen, media, playback, service_state_name, winmm};
use crate::media_monitor::SystemMedia;

pub struct WindowsDeckNative {
    /// The agent's own media monitor (pausing the current media goes through it, as in C#).
    media: Option<Arc<dyn SystemMedia>>,
    names: audio::Names,
    /// One lock for the audio calls, as C#'s `_gate`.
    gate: Mutex<()>,
    sessions: Arc<media::Sessions>,
    playback: Arc<playback::Playback>,
    hardware: Arc<hardware::Hardware>,
}

impl WindowsDeckNative {
    pub fn new(media: Option<Arc<dyn SystemMedia>>) -> WindowsDeckNative {
        WindowsDeckNative {
            media,
            names: Mutex::default(),
            gate: Mutex::new(()),
            sessions: Arc::default(),
            playback: Arc::default(),
            hardware: Arc::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.gate.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl DeckNative for WindowsDeckNative {
    fn on_sound_stopped(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        self.playback.on_sound_stopped(callback);
    }

    fn action_state(&self, action: &crate::config::DeckAction) -> Option<String> {
        match action.kind.as_str() {
            "sound" | "radio" => Some(
                if self.playback.playing(&action.id) {
                    "Playing"
                } else {
                    "Stopped"
                }
                .into(),
            ),
            "service" => Some(service_state_name(hardware::service_state(&action.target)).into()),
            _ => None,
        }
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
        self.hardware.monitors()
    }

    fn brightness(&self, id: &str, value: i32) -> bool {
        self.hardware.brightness(id, value)
    }

    fn power_plans(&self) -> Vec<DeckItem> {
        self.hardware.power_plans()
    }

    fn platform_action<'a>(
        &'a self,
        kind: &'a str,
        target: &'a str,
        argument: Option<&'a str>,
        value: i32,
    ) -> BoxFuture<'a, bool> {
        Box::pin(self.hardware.clone().act(
            kind.to_string(),
            target.to_string(),
            argument.map(str::to_string),
            value,
        ))
    }

    fn media(&self) -> BoxFuture<'_, Result<Vec<DeckMedia>, String>> {
        let sessions = self.sessions.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || sessions.list())
                .await
                .map_err(|e| e.to_string())?
        })
    }

    fn media_action<'a>(&'a self, id: &'a str, action: &'a str, value: i64) -> BoxFuture<'a, bool> {
        Box::pin(async move { self.media_action_with_state(id, action, value).await.0 })
    }

    /// The selected session's own state (not the first of its app id) decides "was playing".
    fn media_action_with_state<'a>(
        &'a self,
        id: &'a str,
        action: &'a str,
        value: i64,
    ) -> BoxFuture<'a, (bool, bool)> {
        let (sessions, id, action) = (self.sessions.clone(), id.to_string(), action.to_string());
        Box::pin(async move {
            tokio::task::spawn_blocking(move || sessions.act(&id, &action, value))
                .await
                .unwrap_or((false, false))
        })
    }

    fn pause_current_media(&self) -> BoxFuture<'_, bool> {
        let (media, sessions) = (self.media.clone(), self.sessions.clone());
        Box::pin(async move {
            // the agent's own media monitor when there is one, as in C#
            tokio::task::spawn_blocking(move || match media {
                Some(media) => media.pause(),
                None => sessions.pause_current(),
            })
            .await
            .unwrap_or(false)
        })
    }

    fn play(&self, id: &str, file: &str, device: Option<&str>, volume: i32) -> Result<(), String> {
        self.playback.play(id, file, device, volume)
    }

    fn stop_sound(&self, id: Option<&str>) {
        self.playback.stop(id);
    }

    fn dispose(&self) {
        self.playback.stop(None);
    }

    fn midi_ports(&self) -> Vec<DeckItem> {
        winmm::ports()
    }

    fn midi(&self, port: i32, message: &str, value: i32) -> Result<(), String> {
        winmm::send(port, message, value)
    }

    fn listen(&self) -> Option<futures_util::stream::BoxStream<'static, DeckAudioPacket>> {
        listen::start()
    }
}
