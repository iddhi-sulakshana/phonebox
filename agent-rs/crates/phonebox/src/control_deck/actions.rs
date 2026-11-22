//! `ControlDeck.cs`, the actions: a configured control the phone taps (or a profile runs), by kind, and the shortcuts the
//! phone lists beside them.

use super::*;

impl ControlDeck {
    /// A configured control (`shortcut:<id>` for a shortcut): a disabled one may only be stopped.
    pub(super) async fn action(
        &self,
        id: &str,
        op: &str,
        value: Option<i32>,
        stop: &Stop,
    ) -> Result<DeckResult, Cancelled> {
        let c = self.cfg.get();
        if let Some(shortcut) = id.strip_prefix("shortcut:") {
            return self.shortcut_action(&c, shortcut, op, stop).await;
        }
        let Some(action) = c.controls.actions.iter().find(|a| a.id == id).cloned() else {
            return Ok(DeckResult::fail(UNKNOWN_CONTROL));
        };
        if !action.enabled
            && !(op == "stop"
                && matches!(action.kind.as_str(), "task" | "server" | "sound" | "radio"))
        {
            return Ok(DeckResult::fail("Control is disabled"));
        }
        let failed = || DeckResult::fail(CONTROL_FAILED);
        let done = |result: Result<(), String>| {
            if result.is_ok() {
                DeckResult::ok()
            } else {
                failed()
            }
        };
        let value = value.unwrap_or(action.value);
        Ok(match action.kind.as_str() {
            "task" | "server" => {
                let stopping = op == "stop";
                self.blocking(move |d| d.tasks.start(&action, stopping))
                    .await
                    .unwrap_or_else(failed)
            }
            "shortcut" | "macro" | "snippet" | "meeting" | "discord" | "project" | "app"
            | "focus" => {
                return self.shortcut_action(&c, &action.target, op, stop).await;
            }
            "keys" => {
                let keys = action.target.clone();
                done(
                    self.blocking(move |_| crate::key_sender::send_keys(&keys))
                        .await
                        .unwrap_or(Err(String::new())),
                )
            }
            "text" => {
                let text = action.target.clone();
                done(
                    self.blocking(move |_| crate::key_sender::type_text(&text))
                        .await
                        .unwrap_or(Err(String::new())),
                )
            }
            "sound" => {
                let stopping = op == "stop";
                let played = self
                    .blocking(move |d| {
                        if stopping {
                            d.native.stop_sound(Some(&action.id));
                            Ok(())
                        } else {
                            let volume = action.value.clamp(0, 100);
                            d.native.play(
                                &action.id,
                                &action.target,
                                action.device.as_deref(),
                                volume,
                            )
                        }
                    })
                    .await;
                done(played.unwrap_or(Err(String::new())))
            }
            "radio" => return self.radio_action(&action.id, op, stop).await,
            "midi" => {
                let Some(port) = action.device.as_deref().and_then(parse_int) else {
                    return Ok(DeckResult::fail("Choose a MIDI output port"));
                };
                let sent = self
                    .blocking(move |d| d.native.midi(port, &action.target, value))
                    .await;
                done(sent.unwrap_or(Err(String::new())))
            }
            "osc" => done(
                send_osc(
                    action.device.as_deref().unwrap_or(""),
                    &action.target,
                    value,
                )
                .await,
            ),
            "workflow" => return self.workflow(&action, stop).await,
            "playlist" => {
                let Some(spotify) = self.spotify.clone() else {
                    return Ok(DeckResult::fail("Spotify is unavailable"));
                };
                let play = async {
                    let result = spotify.act("playlist", Some(&action.id)).await;
                    if result.ok {
                        self.blocking(|d| d.stop_radios(None, true)).await;
                    }
                    result
                };
                self.run_with_media_focus(play, stop).await?
            }
            "audio-route" => return self.route_audio(&action, stop).await,
            "mixer" | "mixer-mute" => self.mixer_action(&action, value),
            kind => {
                let ok = self
                    .platform(
                        kind,
                        &action.target,
                        action.argument.as_deref(),
                        value,
                        stop,
                    )
                    .await?;
                if ok {
                    DeckResult::ok()
                } else {
                    DeckResult::fail("Integration is unavailable or requires setup on the computer")
                }
            }
        })
    }

    /// Runs a shortcut (its toggle state read from the pushed values); `focus` brings a running app to the front
    /// instead (Windows).
    async fn shortcut_action(
        &self,
        c: &Arc<AgentConfig>,
        id: &str,
        op: &str,
        stop: &Stop,
    ) -> Result<DeckResult, Cancelled> {
        let Some(shortcut) = c.shortcuts.iter().find(|s| s.id == id).cloned() else {
            return Ok(DeckResult::fail("Configured shortcut is missing"));
        };
        let running = {
            let shortcut = shortcut.clone();
            op == "focus"
                && self
                    .blocking(move |_| shortcut_running(&shortcut))
                    .await
                    .unwrap_or(false)
        };
        if running {
            let focused = self.platform("focus", &shortcut.cmd, None, 0, stop).await?;
            return Ok(if focused {
                DeckResult::ok()
            } else {
                DeckResult::fail("Windows could not focus the running application")
            });
        }
        let (c, id) = (c.clone(), id.to_string());
        let error = self
            .blocking(move |d| {
                crate::shortcuts::run(&c, &id, d.push.as_deref(), d.master.as_deref())
            })
            .await
            .unwrap_or_else(|| Some(String::new()));
        Ok(match error {
            None => DeckResult::ok(),
            Some(_) => DeckResult::fail("Configured shortcut failed"),
        })
    }
}

/// Sends one OSC message (the value as a float) to `host:port`; an error for an endpoint without a usable port or an
/// address OSC refuses.
async fn send_osc(endpoint: &str, address: &str, value: i32) -> Result<(), String> {
    const UNSET: &str = "Configure an OSC host and port";
    let (host, port) = endpoint.rsplit_once(':').ok_or(UNSET)?;
    let port: u16 = port.parse().ok().filter(|p| *p > 0).ok_or(UNSET)?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return Err(UNSET.into());
    }
    let packet = super::super::osc_packet(address, value as f32)?;
    let target = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("the OSC host has no address")?;
    let local = if target.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let socket = tokio::net::UdpSocket::bind(local)
        .await
        .map_err(|e| e.to_string())?;
    socket
        .send_to(&packet, target)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// A shortcut as the phone lists it (it prefixes the id with `shortcut:`).
pub(super) fn describe_shortcut(s: &Shortcut) -> DeckItem {
    let kind = match s.cmd.as_str() {
        "builtin:keys" => "keys",
        "builtin:text" => "text",
        "builtin:macro" => "macro",
        cmd if cmd.starts_with("builtin:") => "shortcut",
        _ => "app",
    };
    item(
        &s.id,
        &s.label,
        kind,
        shortcut_running(s).then(|| "Running".into()),
    )
}

/// The process name a shortcut's program runs under (`Process.GetProcessesByName`): None for a built-in, a URI that is
/// not a file, or a folder (an empty name would match every process).
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(super) fn shortcut_process(cmd: &str) -> Option<String> {
    if cmd.starts_with("builtin:") {
        return None;
    }
    // an absolute URI other than file: (a one-letter scheme is a Windows drive)
    if let Some(colon) = cmd.find(':') {
        let scheme = &cmd[..colon];
        let is_scheme = scheme.len() > 1
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if is_scheme && !scheme.eq_ignore_ascii_case("file") {
            return None;
        }
    }
    // Path.GetFileNameWithoutExtension, with both separators
    let file = cmd.rsplit(['\\', '/']).next().unwrap_or(cmd);
    let name = file.rfind('.').map_or(file, |dot| &file[..dot]);
    (!name.is_empty()).then(|| name.to_string())
}

/// Whether a shortcut's program runs with a window of its own (the phone shows "Running", and `focus` brings it to the
/// front). Only Windows can say: .NET's `MainWindowHandle` is always 0 elsewhere. Blocks.
pub(super) fn shortcut_running(s: &Shortcut) -> bool {
    #[cfg(windows)]
    {
        shortcut_process(&s.cmd).is_some_and(|name| {
            windowed_processes()
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&name))
        })
    }
    #[cfg(not(windows))]
    {
        let _ = s;
        false
    }
}

/// The names (without `.exe`) of the processes with a visible top-level window that no other window owns — .NET's
/// main window test.
#[cfg(windows)]
fn windowed_processes() -> Vec<String> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GetWindow, GetWindowThreadProcessId, IsWindowVisible,
    };
    use windows::core::BOOL;

    unsafe extern "system" fn collect(window: HWND, pids: LPARAM) -> BOOL {
        // SAFETY: `pids` is the Vec passed below, alive for the whole enumeration
        let pids = unsafe { &mut *(pids.0 as *mut Vec<u32>) };
        // SAFETY: plain window queries
        unsafe {
            let unowned = match GetWindow(window, GW_OWNER) {
                Ok(owner) => owner.is_invalid(),
                Err(_) => true,
            };
            if IsWindowVisible(window).as_bool() && unowned {
                let mut pid = 0u32;
                GetWindowThreadProcessId(window, Some(&mut pid as *mut u32));
                if pid != 0 && !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        }
        BOOL(1)
    }

    let mut pids: Vec<u32> = Vec::new();
    // SAFETY: the callback only touches `pids`, which outlives the call
    let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut pids as *mut Vec<u32> as isize)) };
    pids.into_iter()
        .filter_map(crate::privacy_monitor::process_name)
        .map(|name| {
            let n = name.len();
            if n >= 4 && name.is_char_boundary(n - 4) && name[n - 4..].eq_ignore_ascii_case(".exe")
            {
                name[..n - 4].to_string()
            } else {
                name
            }
        })
        .collect()
}
