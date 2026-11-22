//! The media sessions Windows lists (GSMTC, `ControlDeck.Native.cs:218-281`): every session, not just the current
//! one, for the controls page's players, and their commands.

use std::sync::Mutex;

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Media::MediaPlaybackAutoRepeatMode;

use super::super::models::DeckMedia;
use super::{media_name, media_timeline, repeat_name};

/// .NET ticks (100 ns) from 1601-01-01 to 1970-01-01.
const UNIX_EPOCH_FILETIME: i64 = 116_444_736_000_000_000;

#[derive(Default)]
pub(super) struct Sessions {
    manager: Mutex<Option<Manager>>,
}

fn now_ticks() -> i64 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    ms * 10_000 + UNIX_EPOCH_FILETIME
}

/// COM identity (C#'s `ReferenceEquals` on the projected objects).
fn same(a: &Session, b: &Session) -> bool {
    use windows::core::{IUnknown, Interface};
    matches!((a.cast::<IUnknown>(), b.cast::<IUnknown>()), (Ok(a), Ok(b)) if a == b)
}

fn playing(session: &Session) -> bool {
    session
        .GetPlaybackInfo()
        .and_then(|p| p.PlaybackStatus())
        .is_ok_and(|s| s == Status::Playing)
}

impl Sessions {
    fn manager(&self) -> windows::core::Result<Manager> {
        let mut manager = self.manager.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = manager.as_ref() {
            return Ok(m.clone());
        }
        let m = Manager::RequestAsync()?.join()?;
        *manager = Some(m.clone());
        Ok(m)
    }

    /// Every session, the current one first. Blocks.
    pub(super) fn list(&self) -> Result<Vec<DeckMedia>, String> {
        let manager = self.manager().map_err(|e| e.to_string())?;
        let current = manager.GetCurrentSession().ok();
        let sessions = manager.GetSessions().map_err(|e| e.to_string())?;
        let mut items: Vec<(DeckMedia, bool)> = Vec::new();
        for session in sessions {
            let read = || -> windows::core::Result<DeckMedia> {
                let properties = session.TryGetMediaPropertiesAsync()?.join()?;
                let info = session.GetPlaybackInfo()?;
                let timeline = session.GetTimelineProperties()?;
                let is_playing = info.PlaybackStatus()? == Status::Playing;
                let (position, duration) = media_timeline(
                    timeline.StartTime()?.Duration,
                    timeline.EndTime()?.Duration,
                    timeline.Position()?.Duration,
                    timeline.LastUpdatedTime()?.UniversalTime,
                    now_ticks(),
                    is_playing,
                );
                let controls = info.Controls()?;
                let aumid = session.SourceAppUserModelId()?.to_string();
                Ok(DeckMedia {
                    name: media_name(&aumid),
                    id: aumid,
                    title: properties.Title()?.to_string(),
                    playing: is_playing,
                    position,
                    duration,
                    can_seek: controls.IsPlaybackPositionEnabled()?,
                    can_shuffle: controls.IsShuffleEnabled()?,
                    can_repeat: controls.IsRepeatEnabled()?,
                    shuffle: info
                        .IsShuffleActive()
                        .and_then(|v| v.Value())
                        .unwrap_or(false),
                    repeat: repeat_name(
                        info.AutoRepeatMode()
                            .and_then(|v| v.Value())
                            .ok()
                            .map(|m| m.0),
                    ),
                })
            };
            let item = read().map_err(|e| e.to_string())?;
            items.push((item, current.as_ref().is_some_and(|c| same(c, &session))));
        }
        // the current session first, the others in Windows' order
        items.sort_by_key(|(_, is_current)| !*is_current);
        Ok(items.into_iter().map(|(item, _)| item).collect())
    }

    /// A command to the session with this app id (the current one if it is that app): (done, was playing — for
    /// playpause). Blocks.
    pub(super) fn act(&self, id: &str, action: &str, value: i64) -> (bool, bool) {
        let Ok(manager) = self.manager() else {
            return (false, false);
        };
        let current = manager
            .GetCurrentSession()
            .ok()
            .filter(|s| s.SourceAppUserModelId().is_ok_and(|a| a == id));
        let session = current.or_else(|| {
            manager
                .GetSessions()
                .ok()?
                .into_iter()
                .find(|s| s.SourceAppUserModelId().is_ok_and(|a| a == id))
        });
        let Some(s) = session else {
            return (false, false);
        };
        let was_playing = action == "playpause" && playing(&s);
        let controls = s.GetPlaybackInfo().and_then(|i| i.Controls());
        let enabled = |check: fn(
            &windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackControls,
        ) -> windows::core::Result<bool>| {
            controls.as_ref().is_ok_and(|c| check(c).unwrap_or(false))
        };
        let run = || -> windows::core::Result<bool> {
            match action {
                "playpause" => s.TryTogglePlayPauseAsync()?.join(),
                "next" => s.TrySkipNextAsync()?.join(),
                "prev" => s.TrySkipPreviousAsync()?.join(),
                "pause" => s.TryPauseAsync()?.join(),
                "seek" if enabled(|c| c.IsPlaybackPositionEnabled()) => {
                    let timeline = s.GetTimelineProperties()?;
                    let start = timeline.StartTime()?.Duration;
                    let length_ms = (timeline.EndTime()?.Duration - start) as f64 / 10_000.0;
                    if value < 0 || value as f64 > length_ms {
                        return Ok(false);
                    }
                    s.TryChangePlaybackPositionAsync(start + value.saturating_mul(10_000))?
                        .join()
                }
                "shuffle" if enabled(|c| c.IsShuffleEnabled()) => {
                    s.TryChangeShuffleActiveAsync(value != 0)?.join()
                }
                "repeat" if enabled(|c| c.IsRepeatEnabled()) && (0..=2).contains(&value) => s
                    .TryChangeAutoRepeatModeAsync(MediaPlaybackAutoRepeatMode(value as i32))?
                    .join(),
                _ => Ok(false),
            }
        };
        (run().unwrap_or(false), was_playing)
    }

    /// Pauses the current session if it plays. Blocks.
    pub(super) fn pause_current(&self) -> bool {
        let Ok(manager) = self.manager() else {
            return false;
        };
        let Ok(session) = manager.GetCurrentSession() else {
            return false;
        };
        playing(&session)
            && session
                .TryPauseAsync()
                .and_then(|op| op.join())
                .unwrap_or(false)
    }
}
