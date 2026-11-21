//! `ControlDeck.Models.cs`: the records the deck answers with (camelCase, in this order, nulls written) and the
//! native seam every platform implements and every test fakes.

use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use serde::Serialize;
use std::sync::Arc;

/// An action's outcome; shared with Spotify (`SpotifyControl.Act`) and OBS (`ObsMonitor.DeckAct`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeckResult {
    pub ok: bool,
    pub error: Option<String>,
    pub lease: Option<String>,
}

impl DeckResult {
    pub fn ok() -> DeckResult {
        DeckResult {
            ok: true,
            error: None,
            lease: None,
        }
    }
    pub fn fail(error: impl Into<String>) -> DeckResult {
        DeckResult {
            ok: false,
            error: Some(error.into()),
            lease: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeckLevel {
    pub kind: String,
    pub id: String,
    pub volume: Option<i32>,
    pub muted: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeckItem {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub state: Option<String>,
    pub available: bool,
    pub rev: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckDevice {
    pub id: String,
    pub name: String,
    /// "output" or "input".
    pub direction: String,
    pub default: bool,
    pub volume: i32,
    pub muted: bool,
    /// A float on Windows, widened without rounding.
    pub peak: f64,
    pub channels: i32,
    pub can_control: bool,
    pub channel_volumes: Option<Vec<i32>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckSession {
    pub id: String,
    pub name: String,
    pub pid: i32,
    pub device_id: String,
    pub volume: i32,
    pub muted: bool,
    pub peak: f64,
    pub process_name: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeckAudio {
    pub available: bool,
    pub error: Option<String>,
    pub devices: Vec<DeckDevice>,
    pub sessions: Vec<DeckSession>,
    pub defaults: Option<indexmap::IndexMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckMedia {
    pub id: String,
    pub name: String,
    pub title: String,
    pub playing: bool,
    pub position: i64,
    pub duration: i64,
    pub can_seek: bool,
    pub can_shuffle: bool,
    pub can_repeat: bool,
    pub shuffle: bool,
    /// "None", "Track" or "List".
    pub repeat: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeckMonitor {
    pub id: String,
    pub name: String,
    pub brightness: Option<i32>,
    pub width: i32,
    pub height: i32,
    pub hz: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeckAudioPacket {
    pub rate: i32,
    pub channels: i32,
    pub bits: i32,
    pub encoding: String,
    pub data: Option<String>,
}

/// `POST /api/controls/capture`'s profile: unlike the settings writer, `output` and `input` are written as null.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CapturedProfile {
    pub id: String,
    pub label: String,
    pub levels: Vec<DeckLevel>,
    pub actions: Vec<String>,
    pub output: Option<String>,
    pub input: Option<String>,
    pub defaults: indexmap::IndexMap<String, String>,
}

/// `IDeckNative`: what the deck asks of the platform (Windows: Core Audio, WinRT, P/Invoke; elsewhere: one fake
/// device, the current media session and command-line players). Blocking methods run off the async workers.
pub trait DeckNative: Send + Sync {
    /// Called with a sound's id when it stops by itself (ended, failed, the player exited).
    fn on_sound_stopped(&self, _callback: Arc<dyn Fn(&str) + Send + Sync>) {}
    fn audio(&self) -> DeckAudio;
    fn device(
        &self,
        id: &str,
        volume: Option<i32>,
        muted: Option<bool>,
        channel: Option<i32>,
    ) -> bool;
    fn session(&self, id: &str, volume: Option<i32>, muted: Option<bool>) -> bool;
    /// role 0 console, 1 multimedia, 2 communications; None = all three.
    fn default_device(&self, id: &str, role: Option<i32>) -> bool;
    fn monitors(&self) -> Vec<DeckMonitor>;
    fn brightness(&self, id: &str, value: i32) -> bool;
    fn power_plans(&self) -> Vec<DeckItem>;
    fn platform_action<'a>(
        &'a self,
        kind: &'a str,
        target: &'a str,
        argument: Option<&'a str>,
        value: i32,
    ) -> BoxFuture<'a, bool>;
    fn action_state(&self, _action: &crate::config::DeckAction) -> Option<String> {
        None
    }
    /// The media sessions; an error when they cannot be read (the phone shows "Media sessions are unavailable").
    fn media(&self) -> BoxFuture<'_, Result<Vec<DeckMedia>, String>>;
    fn media_action<'a>(&'a self, id: &'a str, action: &'a str, value: i64) -> BoxFuture<'a, bool>;
    /// `MediaActionWithState`: for `playpause`, whether that session was playing before (read first; a failed read
    /// counts as not playing), then the action.
    fn media_action_with_state<'a>(
        &'a self,
        id: &'a str,
        action: &'a str,
        value: i64,
    ) -> BoxFuture<'a, (bool, bool)> {
        Box::pin(async move {
            let was_playing = if action == "playpause" {
                self.media()
                    .await
                    .unwrap_or_default()
                    .iter()
                    .find(|m| m.id == id)
                    .is_some_and(|m| m.playing)
            } else {
                false
            };
            (self.media_action(id, action, value).await, was_playing)
        })
    }
    fn pause_current_media(&self) -> BoxFuture<'_, bool> {
        Box::pin(async { false })
    }
    /// Starts sound `id` (a file or a stream); an error says why it could not.
    fn play(&self, id: &str, file: &str, device: Option<&str>, volume: i32) -> Result<(), String>;
    /// None stops every sound.
    fn stop_sound(&self, id: Option<&str>);
    fn midi_ports(&self) -> Vec<DeckItem>;
    fn midi(&self, port: i32, message: &str, value: i32) -> Result<(), String>;
    /// Loopback audio for `/api/controls/listen`; None where it is not supported.
    fn listen(&self) -> Option<BoxStream<'static, DeckAudioPacket>> {
        None
    }
    /// The deck is going away: stop every player and release what the platform holds.
    fn dispose(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_records_keep_their_names_order_and_nulls() {
        let r = serde_json::to_string(&DeckResult::fail("x")).unwrap();
        assert_eq!(r, r#"{"ok":false,"error":"x","lease":null}"#);
        let item = DeckItem {
            id: "a".into(),
            name: "A".into(),
            kind: "radio".into(),
            state: None,
            available: true,
            rev: None,
        };
        assert_eq!(
            serde_json::to_string(&item).unwrap(),
            r#"{"id":"a","name":"A","kind":"radio","state":null,"available":true,"rev":null}"#
        );
        let profile = CapturedProfile {
            id: "p".into(),
            label: "P".into(),
            levels: vec![],
            actions: vec![],
            output: None,
            input: None,
            defaults: Default::default(),
        };
        assert_eq!(
            serde_json::to_string(&profile).unwrap(),
            r#"{"id":"p","label":"P","levels":[],"actions":[],"output":null,"input":null,"defaults":{}}"#
        );
    }
}
