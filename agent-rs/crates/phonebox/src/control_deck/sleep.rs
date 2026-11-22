//! `ControlDeck.Sleep.cs` and the ducking in `ControlDeck.cs`: a sleep timer fades its player's own audio sessions over
//! the last 30 s, pauses it and puts the volume back; ducking lowers the other sessions while a microphone hears
//! something. Both keep what they changed in [`DeckState`] and give a session back once the user moves its volume.

use super::*;

/// The fade starts this long before the timer ends.
const FADE_MS: i64 = 30_000;
/// A microphone louder than this hears something (and ducking acts).
const HEARING_PEAK: f64 = 0.035;

/// Whether an audio session (by its name or process name) is the player's own: the player id's name, or for a packaged
/// app the part after `!`, without `.exe`, in any case.
pub(crate) fn player_session_matches(player: &str, application: &str) -> bool {
    fn clean(text: &str) -> String {
        let n = text.len();
        let stem =
            if n >= 4 && text.is_char_boundary(n - 4) && text[n - 4..].eq_ignore_ascii_case(".exe")
            {
                &text[..n - 4]
            } else {
                text
            };
        lower_invariant(stem)
    }
    let name = clean(application);
    utf16_len(&name) > 2
        && (clean(player) == name
            || player
                .find('!')
                .is_some_and(|bang| clean(&player[bang + 1..]) == name))
}

impl ControlDeck {
    /// Runs the sleep timers: fades the players whose timer ends within [`FADE_MS`], pauses the ones that ended and puts
    /// their volume back.
    pub(crate) async fn tick_sleep(&self, stop: &Stop) {
        let timers: Vec<(String, i64)> = self
            .blocking(|d| {
                lock(&d.state)
                    .sleep
                    .iter()
                    .map(|(player, at)| (player.clone(), *at))
                    .collect()
            })
            .await
            .unwrap_or_default();
        for (player, deadline) in timers {
            let remaining = deadline - (self.now)();
            if remaining <= FADE_MS {
                let faded = player.clone();
                self.blocking(move |d| d.fade(&faded, deadline, remaining))
                    .await;
            }
            if remaining <= 0 {
                tokio::select! {
                    _ = self.native.media_action(&player, "pause", 0) => {}
                    _ = stop.stopped() => {}
                }
                self.blocking(move |d| {
                    let mut state = lock(&d.state);
                    if state.sleep.get(&player) == Some(&deadline) {
                        state.sleep.shift_remove(&player);
                        d.restore_fade_locked(&mut state, &player);
                    }
                })
                .await;
            }
        }
    }

    /// One fade step for a player whose timer ends in `remaining` ms (unless the timer changed meanwhile). Blocks.
    fn fade(&self, player: &str, deadline: i64, remaining: i64) {
        let mut state = lock(&self.state);
        if state.sleep.get(player) != Some(&deadline) {
            return;
        }
        let mut saved = state.fades.shift_remove(player).unwrap_or_default();
        let sessions = self.native.audio().sessions;
        let own = |s: &&DeckSession| {
            player_session_matches(player, &s.name)
                || s.process_name
                    .as_deref()
                    .is_some_and(|p| !p.is_empty() && player_session_matches(player, p))
        };
        for session in sessions.iter().filter(own) {
            let ducked = state.duck.shift_remove(&session.id);
            if let Some((original, applied)) = ducked
                && session.volume == applied
            {
                if self.native.session(&session.id, Some(original), None)
                    && !saved.contains_key(&session.id)
                {
                    saved.insert(session.id.clone(), (original, original));
                }
                // read back on the next tick before the fade applies
                continue;
            }
            let level = saved.get(&session.id).copied();
            if let Some((_, applied)) = level
                && session.volume != applied
            {
                // a volume the user set takes the session back from the timer
                saved.insert(session.id.clone(), (-1, session.volume));
                continue;
            }
            let (original, _) = level.unwrap_or((session.volume, session.volume));
            if original < 0 {
                continue;
            }
            let share = (remaining as f64 / FADE_MS as f64).clamp(0.0, 1.0);
            let volume = (f64::from(original) * share).round_ties_even() as i32;
            if self.native.session(&session.id, Some(volume), None) {
                saved.insert(session.id.clone(), (original, volume));
            }
        }
        state.fades.insert(player.to_string(), saved);
        state.audio = None;
    }

    /// Puts back the volumes a player's fade set, where the user has not changed them since.
    pub(crate) fn restore_fade_locked(&self, state: &mut DeckState, player: &str) {
        let Some(saved) = state.fades.shift_remove(player) else {
            return;
        };
        let sessions = self.native.audio().sessions;
        for (id, (original, applied)) in saved {
            if original >= 0
                && sessions
                    .iter()
                    .find(|s| s.id == id)
                    .is_some_and(|s| s.volume == applied)
            {
                self.native.session(&id, Some(original), None);
            }
        }
        state.audio = None;
    }

    /// Ducking, once per tick: while it is on and an unmuted microphone hears something, every other session (not the
    /// agent's own, not one a sleep fade owns) goes down to the percentage; otherwise the ducked ones come back. Blocks.
    pub(crate) fn tick_ducking(&self) {
        let mut state = lock(&self.state);
        let Some(percent) = state.duck_percent else {
            self.restore_ducking_locked(&mut state);
            return;
        };
        let audio = self.audio_locked(&mut state);
        let hearing = audio
            .devices
            .iter()
            .any(|d| d.direction == "input" && !d.muted && d.peak > HEARING_PEAK);
        if !hearing {
            self.restore_ducking_locked(&mut state);
            return;
        }
        let own = std::process::id() as i32;
        for s in audio.sessions.iter().filter(|s| s.pid != own) {
            if state
                .fades
                .values()
                .any(|levels| levels.contains_key(&s.id))
                || state.duck.contains_key(&s.id)
            {
                continue;
            }
            let applied =
                (f64::from(s.volume) * f64::from(percent) / 100.0).round_ties_even() as i32;
            if self.native.session(&s.id, Some(applied), None) {
                state.duck.insert(s.id.clone(), (s.volume, applied));
            }
        }
    }

    /// Puts the ducked sessions back where the user has not changed them since.
    pub(crate) fn restore_ducking_locked(&self, state: &mut DeckState) {
        if state.duck.is_empty() {
            return;
        }
        let audio = self.native.audio();
        for (id, (original, applied)) in std::mem::take(&mut state.duck) {
            if audio
                .sessions
                .iter()
                .find(|s| s.id == id)
                .is_some_and(|s| s.volume == applied)
            {
                self.native.session(&id, Some(original), None);
            }
        }
        state.audio = None;
    }
}
