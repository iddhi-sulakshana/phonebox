//! `ControlDeck.cs`, the radio: internet radio stations, which one Now Playing shows (playing or paused), the phone's
//! next, previous and play/pause on them, and stations that stop by themselves or leave the settings.
//!
//! Lock order: `radio_gate` (held across a station's blocking open) before `radio`; the media the state shows takes
//! only `radio`, so it never waits for a station to open or for an audio reading.

use std::sync::TryLockError;

use indexmap::IndexSet;

use super::*;
use crate::media_monitor::{MediaDeck, MediaSnapshot};

/// `_radioState`.
#[derive(Default)]
pub(super) struct RadioState {
    /// The stations playing now.
    active: IndexSet<String>,
    /// The station Now Playing shows, playing or paused.
    last: Option<String>,
    /// When it started playing (Unix ms; 0 while it does not play).
    started_at: i64,
    /// How long it had played when it paused.
    elapsed_ms: i64,
}

impl RadioState {
    /// Nothing to show any more.
    fn forget(&mut self) {
        self.last = None;
        self.started_at = 0;
        self.elapsed_ms = 0;
    }

    /// The station stopped playing: keep how long it played.
    fn pause_clock(&mut self, now: i64) {
        if self.started_at > 0 {
            self.elapsed_ms = (now - self.started_at).max(0);
            self.started_at = 0;
        }
    }

    fn idle(&self) -> bool {
        self.active.is_empty() && self.last.is_none()
    }
}

/// A station's name on Now Playing: its label, else its id.
fn station_title(action: &DeckAction) -> &str {
    if action.label.trim().is_empty() {
        &action.id
    } else {
        &action.label
    }
}

impl ControlDeck {
    fn radios(&self) -> Vec<DeckAction> {
        self.cfg
            .get()
            .controls
            .actions
            .iter()
            .filter(|a| a.kind == "radio")
            .cloned()
            .collect()
    }

    fn radio_enabled(&self, id: &str) -> bool {
        self.cfg
            .get()
            .controls
            .actions
            .iter()
            .any(|a| a.kind == "radio" && a.id == id && a.enabled)
    }

    /// The radio gate without waiting (a poisoned gate is still a gate).
    fn try_radio_gate(&self) -> Option<MutexGuard<'_, ()>> {
        match self.radio_gate.try_lock() {
            Ok(gate) => Some(gate),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// A station card tapped on the phone (or run by a profile), inside the media focus: `stop` stops it, `toggle`
    /// stops a playing one, anything else starts it.
    pub(super) async fn radio_action(
        &self,
        id: &str,
        op: &str,
        stop: &Stop,
    ) -> Result<DeckResult, Cancelled> {
        let work = async {
            // a tap on a playing station card stops it ("toggle"); a profile ("") always starts it
            if op == "stop" || (op == "toggle" && self.is_radio_playing(id)) {
                let target = id.to_string();
                return Ok(
                    match self.blocking(move |d| d.stop_radio(&target, true)).await {
                        Some(()) => DeckResult::ok(),
                        None => DeckResult::fail(CONTROL_FAILED),
                    },
                );
            }
            let current = self
                .cfg
                .get()
                .controls
                .actions
                .iter()
                .find(|a| a.kind == "radio" && a.id == id)
                .cloned();
            let Some(current) = current else {
                return Ok(DeckResult::fail(UNKNOWN_CONTROL));
            };
            if !current.enabled {
                return Ok(DeckResult::fail("Control is disabled"));
            }
            let started = self.start_radio_and_pause(&current, stop).await?;
            Ok(if started {
                DeckResult::ok()
            } else {
                DeckResult::fail("Radio stream is unavailable")
            })
        };
        self.run_with_media_focus(work, stop).await?
    }

    /// Starts a station, then pauses the current media. The station plays once it opened, and that is what was asked
    /// for: the pause is best effort on its own budget (a slow stream open can outlast the panel's request, and the
    /// request's end must not stop the station that just started).
    async fn start_radio_and_pause(
        &self,
        action: &DeckAction,
        stop: &Stop,
    ) -> Result<bool, Cancelled> {
        if stop.is_stopped() {
            return Err(Cancelled);
        }
        let station = action.clone();
        if !self
            .blocking(move |d| d.start_radio(&station))
            .await
            .unwrap_or(false)
        {
            return Ok(false);
        }
        let _ = tokio::time::timeout(self.media_pause, self.native.pause_current_media()).await;
        Ok(true)
    }

    /// Opens a station (the others stop); false when it is disabled (before or while it opened), failed to open, or
    /// does not play. Blocks.
    fn start_radio(&self, action: &DeckAction) -> bool {
        let _gate = lock(&self.radio_gate);
        if !self.radio_enabled(&action.id) {
            return false;
        }
        self.stop_radios_locked(Some(&action.id), false);
        let volume = action.value.clamp(0, 100);
        if self
            .native
            .play(&action.id, &action.target, action.device.as_deref(), volume)
            .is_err()
        {
            lock(&self.radio).active.shift_remove(&action.id);
            self.invalidate();
            return false;
        }
        // disabled or removed while it opened
        if !self.radio_enabled(&action.id) {
            self.native.stop_sound(Some(&action.id));
            lock(&self.radio).active.shift_remove(&action.id);
            self.invalidate();
            return false;
        }
        let playing = self
            .native
            .action_state(action)
            .is_none_or(|state| state == "Playing");
        let now = (self.now)();
        {
            let mut radio = lock(&self.radio);
            if playing {
                radio.active.insert(action.id.clone());
                radio.last = Some(action.id.clone());
                radio.started_at = now;
                radio.elapsed_ms = 0;
            } else {
                radio.active.shift_remove(&action.id);
            }
        }
        self.invalidate();
        playing
    }

    /// `StopAllRadios`: every station but `except` stops; `clear_last` forgets the one Now Playing showed. Blocks.
    pub fn stop_radios(&self, except: Option<&str>, clear_last: bool) {
        let _gate = lock(&self.radio_gate);
        self.stop_radios_locked(except, clear_last);
    }

    fn stop_radios_locked(&self, except: Option<&str>, clear_last: bool) {
        let now = (self.now)();
        let tracked: Vec<String> = {
            let mut radio = lock(&self.radio);
            let tracked = radio
                .active
                .iter()
                .filter(|id| Some(id.as_str()) != except)
                .cloned()
                .collect();
            match except {
                None => radio.active.clear(),
                Some(except) => radio.active.retain(|id| id == except),
            }
            if clear_last && except.is_none() {
                radio.forget();
            } else if radio.last.as_deref() != except {
                radio.pause_clock(now);
            }
            tracked
        };
        let mut ids: IndexSet<String> = self
            .radios()
            .into_iter()
            .map(|a| a.id)
            .filter(|id| Some(id.as_str()) != except)
            .collect();
        ids.extend(tracked);
        for id in &ids {
            self.native.stop_sound(Some(id));
        }
        self.invalidate();
    }

    /// Stops one station; `clear_last` forgets it, else Now Playing shows it paused. Also stops a plain sound with this
    /// id. Blocks.
    pub(super) fn stop_radio(&self, id: &str, clear_last: bool) {
        let _gate = lock(&self.radio_gate);
        let now = (self.now)();
        {
            let mut radio = lock(&self.radio);
            let was_active = radio.active.shift_remove(id);
            if radio.last.as_deref() == Some(id) || was_active {
                if clear_last {
                    radio.forget();
                } else {
                    radio.last = Some(id.to_string());
                    radio.pause_clock(now);
                }
            }
        }
        self.native.stop_sound(Some(id));
        self.invalidate();
    }

    pub(crate) fn is_radio_playing(&self, id: &str) -> bool {
        lock(&self.radio).active.contains(id)
    }

    /// A station's state on the phone's card. With the gate busy (a station is opening) it answers from what the deck
    /// knows instead of waiting. Blocks.
    pub(super) fn radio_state(&self, action: &DeckAction) -> Option<String> {
        match self.try_radio_gate() {
            Some(_gate) => {
                let state = self.native.action_state(action);
                if state.as_deref().is_some_and(|s| s != "Playing") {
                    self.on_native_sound_stopped_core(&action.id);
                }
                state
            }
            None => Some(
                if self.is_radio_playing(&action.id) {
                    "Playing"
                } else {
                    "Stopped"
                }
                .into(),
            ),
        }
    }

    /// The station playing now (the one Now Playing showed first, if it still plays).
    fn playing_radio_action(&self) -> Option<DeckAction> {
        let radios = self.radios();
        if radios.is_empty() {
            return None;
        }
        let now = (self.now)();
        let mut radio = lock(&self.radio);
        if let Some(last) = &radio.last
            && radio.active.contains(last)
            && let Some(preferred) = radios.iter().find(|a| a.id == *last)
        {
            return Some(preferred.clone());
        }
        let found = radios.into_iter().find(|a| radio.active.contains(&a.id))?;
        radio.last = Some(found.id.clone());
        if radio.started_at <= 0 {
            radio.started_at = now;
        }
        Some(found)
    }

    /// The media the phone sees: a playing station, else a playing system player (which makes the deck forget a paused
    /// station), else a paused station, else the system's media as it is. Never waits for a station or for audio.
    pub fn effective_media(&self, system: Option<MediaSnapshot>) -> Option<MediaSnapshot> {
        if let Some(playing) = self.playing_radio_action() {
            let now = (self.now)();
            let started = {
                let mut radio = lock(&self.radio);
                if radio.started_at <= 0 {
                    radio.started_at = now;
                }
                radio.started_at
            };
            return Some(MediaSnapshot::new(
                "Internet Radio",
                station_title(&playing),
                "Live stream",
                "",
                true,
                (now - started).max(0),
                0,
                0,
                false,
            ));
        }
        if system.as_ref().is_some_and(|m| m.playing) {
            lock(&self.radio).forget();
            return system;
        }
        let (paused, elapsed) = {
            let radio = lock(&self.radio);
            (radio.last.clone(), radio.elapsed_ms)
        };
        if let Some(paused) = paused
            && let Some(station) = self.radios().iter().find(|a| a.id == paused)
        {
            return Some(MediaSnapshot::new(
                "Internet Radio",
                station_title(station),
                "Paused",
                "",
                false,
                elapsed,
                0,
                0,
                false,
            ));
        }
        system
    }

    /// `TryControlRadioMediaAsync`: the phone's next, previous or play/pause, taking the media focus first (the
    /// routes hold the focus themselves and use the [`MediaDeck`] side).
    #[cfg(test)]
    pub(crate) async fn try_control_radio_media(
        &self,
        action: &str,
        system: Option<&MediaSnapshot>,
        stop: &Stop,
    ) -> Result<Option<bool>, Cancelled> {
        self.run_with_media_focus(self.try_control_radio_core(action, system, stop), stop)
            .await?
    }

    /// Inside the media focus: Some(handled) when a station is the current media (playing, or paused while no system
    /// player plays); next and previous skip disabled stations and ones that fail to open.
    async fn try_control_radio_core(
        &self,
        action: &str,
        system: Option<&MediaSnapshot>,
        stop: &Stop,
    ) -> Result<Option<bool>, Cancelled> {
        if stop.is_stopped() {
            return Err(Cancelled);
        }
        let radios = self.radios();
        if radios.is_empty() {
            return Ok(None);
        }
        let playing = self.playing_radio_action();
        let last = lock(&self.radio).last.clone();
        let current = playing.clone().or_else(|| {
            last.filter(|_| system.map(|m| m.playing) != Some(true))
                .and_then(|id| radios.iter().find(|a| a.id == id).cloned())
        });
        let Some(current) = current else {
            return Ok(None);
        };
        let index = radios.iter().position(|a| a.id == current.id).unwrap_or(0);
        let count = radios.len();
        match action {
            "next" | "prev" => {
                for step in 1..=count {
                    if stop.is_stopped() {
                        return Err(Cancelled);
                    }
                    let i = if action == "next" {
                        (index + step) % count
                    } else {
                        (index + count * step - step) % count
                    };
                    let station = &radios[i];
                    if station.enabled && self.start_radio_and_pause(station, stop).await? {
                        return Ok(Some(true));
                    }
                }
                Ok(Some(false))
            }
            "playpause" => {
                if let Some(playing) = playing {
                    self.blocking(move |d| d.stop_radio(&playing.id, false))
                        .await;
                    return Ok(Some(true));
                }
                Ok(Some(
                    current.enabled && self.start_radio_and_pause(&current, stop).await?,
                ))
            }
            _ => Ok(Some(false)),
        }
    }

    /// The native side says a sound stopped by itself; a late notice for a station that plays again is ignored.
    pub(super) fn on_native_sound_stopped(&self, id: &str) {
        let _gate = lock(&self.radio_gate);
        let probe = DeckAction {
            id: id.to_string(),
            kind: "radio".into(),
            ..DeckAction::default()
        };
        if self.native.action_state(&probe).as_deref() == Some("Playing") {
            return;
        }
        self.on_native_sound_stopped_core(id);
    }

    /// A station no longer plays: Now Playing shows it paused.
    fn on_native_sound_stopped_core(&self, id: &str) {
        let now = (self.now)();
        let removed = {
            let mut radio = lock(&self.radio);
            let removed = radio.active.shift_remove(id);
            if removed && radio.last.as_deref() == Some(id) {
                radio.pause_clock(now);
            }
            removed
        };
        if removed {
            self.invalidate();
        }
    }

    /// Stops stations that left the settings or were disabled, and drops ones that stopped playing (shown as paused).
    /// The loop skips it while a station opens; a settings save waits for the gate. Blocks.
    pub fn reconcile_radios(&self, wait_for_gate: bool) {
        let _gate = if wait_for_gate {
            lock(&self.radio_gate)
        } else {
            if lock(&self.radio).idle() {
                return;
            }
            match self.try_radio_gate() {
                Some(gate) => gate,
                None => return,
            }
        };
        let active: Vec<String> = {
            let radio = lock(&self.radio);
            if radio.idle() {
                return;
            }
            radio.active.iter().cloned().collect()
        };
        let c = self.cfg.get();
        let enabled = |id: &str| {
            c.controls
                .actions
                .iter()
                .find(|a| a.kind == "radio" && a.id == id && a.enabled)
        };
        let mut removed_any = false;
        for id in active {
            let Some(configured) = enabled(&id) else {
                self.native.stop_sound(Some(&id));
                let mut radio = lock(&self.radio);
                removed_any |= radio.active.shift_remove(&id);
                if radio.last.as_deref() == Some(id.as_str()) {
                    radio.forget();
                    removed_any = true;
                }
                continue;
            };
            let state = self.native.action_state(configured);
            if state.as_deref().is_none_or(|s| s == "Playing") {
                continue;
            }
            let now = (self.now)();
            let mut radio = lock(&self.radio);
            if radio.active.shift_remove(&id) {
                removed_any = true;
                if radio.last.as_deref() == Some(id.as_str()) {
                    radio.pause_clock(now);
                }
            }
        }
        {
            let mut radio = lock(&self.radio);
            if radio
                .last
                .as_deref()
                .is_some_and(|last| enabled(last).is_none())
            {
                radio.forget();
                removed_any = true;
            }
        }
        if removed_any {
            self.invalidate();
        }
    }

    /// Stops the stations, sounds, tasks and servers the settings just disabled. Blocks.
    pub fn stop_disabled_controls(&self) {
        let c = self.cfg.get();
        for a in c.controls.actions.iter().filter(|a| !a.enabled) {
            match a.kind.as_str() {
                "radio" => self.stop_radio(&a.id, true),
                "sound" => self.native.stop_sound(Some(&a.id)),
                "task" | "server" => self.tasks.stop_if_running(&a.id),
                _ => {}
            }
        }
        self.invalidate();
    }
}

impl MediaDeck for ControlDeck {
    fn effective_media(&self, system: Option<MediaSnapshot>) -> Option<MediaSnapshot> {
        ControlDeck::effective_media(self, system)
    }

    fn try_control_radio<'a>(
        &'a self,
        action: &'a str,
        system: Option<&'a MediaSnapshot>,
    ) -> futures_util::future::BoxFuture<'a, Option<bool>> {
        Box::pin(async move {
            self.try_control_radio_core(action, system, &Stop::default())
                .await
                .unwrap_or(None)
        })
    }

    /// Blocks while a station opens: call it off the async workers.
    fn stop_all_radios(&self) {
        self.stop_radios(None, true);
    }

    fn media_focus(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.focus.clone()
    }
}

#[cfg(test)]
#[path = "radio_tests.rs"]
mod tests;
