//! Port of `AppConsole.cs`: follows the focused app and sends a profile's keys to it. Off unless an enabled page of the
//! phone shows an app-console widget with "control" on, and only for that widget; the phone sends a profile id and a
//! slot, never keys; the focused app is checked again right before every combo, so keys never land in an app the
//! profile isn't for. Only Windows can say which app is focused without extra permissions or tools.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::store::{ConfigStore, MAX_CONSOLE_COMBOS};
use crate::config::{AgentConfig, WidgetConfig};
use crate::dictation::FocusedApp;
use crate::server::Stop;

pub const WIDGET_TYPE: &str = "app-console";

/// The caller's stop signal ended the call: nothing more was sent, and the route answers nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// Presses one parsed combo; false when the system refused it.
pub type SendCombo = Arc<dyn Fn(&[String]) -> bool + Send + Sync>;

/// The status and, for anything but 200, the error the phone shows.
pub type ActResult = Result<(u16, Option<String>), Cancelled>;

pub struct AppConsole {
    cfg: Arc<ConfigStore>,
    pub(crate) supported: bool,
    pub(crate) focused: Arc<dyn Fn() -> Option<FocusedApp> + Send + Sync>,
    pub(crate) foreground: Arc<dyn Fn() -> isize + Send + Sync>,
    pub(crate) send_combo: SendCombo,
    pub(crate) agent_elevated: bool,
    sending: tokio::sync::Semaphore,
}

/// The app console widget the phone names, as "pageId/widgetId" ("pageId/stackId:cardId" in a Smart Stack), if it is
/// on an enabled page this phone shows. Each widget's own "control" option counts, not another one's.
pub fn widget_for(
    c: &AgentConfig,
    device_id: Option<&str>,
    key: Option<&str>,
) -> Option<WidgetConfig> {
    crate::panel_widgets::find(c, device_id, key, WIDGET_TYPE)
}

impl AppConsole {
    pub fn new(cfg: Arc<ConfigStore>) -> AppConsole {
        AppConsole {
            cfg,
            supported: cfg!(windows),
            focused: Arc::new(focused_now),
            foreground: Arc::new(foreground_now),
            send_combo: Arc::new(|combo| crate::key_sender::send_combo(combo).unwrap_or(false)),
            agent_elevated: crate::dictation::is_privileged_process(),
            sending: tokio::sync::Semaphore::new(1),
        }
    }

    /// What the phone may know: the matching profile's id, label and key labels. Never the process name or the keys.
    pub fn state(&self, device_id: Option<&str>, widget: Option<&str>) -> (u16, Value) {
        let c = self.cfg.get();
        let Some(w) = widget_for(&c, device_id, widget) else {
            return (
                404,
                json!({ "error": "this app console widget isn't on an enabled page of this phone" }),
            );
        };
        let control = w.bool("control") == Some(true);
        if !self.supported {
            return (
                200,
                json!({ "supported": false, "control": control, "profile": null }),
            );
        }
        let app = (self.focused)();
        let profile =
            app.and_then(|app| c.console.profiles.iter().find(|p| p.matches(&app.process)));
        let profile = profile.map(|p| {
            json!({
                "id": p.id,
                "label": p.label,
                "keys": p.keys.iter().map(|k| k.label.as_str()).collect::<Vec<_>>(),
                "dial": {
                    "left": p.dial_left.is_some(),
                    "right": p.dial_right.is_some(),
                    "press": p.dial_press.is_some(),
                },
            })
        });
        (
            200,
            json!({ "supported": true, "control": control, "profile": profile }),
        )
    }

    /// Presses a slot of a profile; `times` repeats a dial turn (clamped to 1..5).
    pub async fn act(
        &self,
        device_id: Option<&str>,
        widget: Option<&str>,
        profile_id: &str,
        slot: &str,
        times: i32,
        cancel: &Stop,
    ) -> ActResult {
        let refused = |status: u16, error: &str| Ok((status, Some(error.to_string())));
        let c = self.cfg.get();
        if widget_for(&c, device_id, widget).and_then(|w| w.bool("control")) != Some(true) {
            return refused(
                403,
                "sending keys is off: turn on \"Send keys to the computer\" in the app console widget's settings",
            );
        }
        if !self.supported {
            return refused(409, "the app console needs Windows");
        }
        let Some(profile) = c.console.profiles.iter().find(|p| p.id == profile_id) else {
            return refused(404, "no such profile");
        };
        let Some(spec) = profile.spec(slot).filter(|s| !s.trim().is_empty()) else {
            return refused(404, "nothing is set for this key");
        };
        let combos = match crate::key_sender::parse(spec) {
            Ok(combos) => combos,
            Err(e) => return refused(400, &e),
        };
        // also for a binding saved before the limit, or written into the file by hand
        if combos.len() > MAX_CONSOLE_COMBOS {
            return refused(
                400,
                &format!(
                    "more than {MAX_CONSOLE_COMBOS} key presses; shorten this binding in the app console settings"
                ),
            );
        }
        let times = if matches!(slot, "dial-left" | "dial-right") {
            times.clamp(1, 5)
        } else {
            1
        };

        let _permit = tokio::select! {
            biased;
            _ = cancel.stopped() => return Err(Cancelled),
            permit = tokio::time::timeout(Duration::from_secs(2), self.sending.acquire()) => match permit {
                Ok(Ok(permit)) => permit,
                _ => return refused(429, "still sending the previous keys"),
            },
        };
        for _ in 0..times {
            for combo in &combos {
                // checked again before every combo: a click elsewhere mid-macro must not send the rest there. Looked
                // up first, so a binding saved while this runs is still seen by the checks below.
                let app = (self.focused)();
                // the switch can be turned off (or the widget removed) while this waits or runs
                let current = self.cfg.get();
                if widget_for(&current, device_id, widget).and_then(|w| w.bool("control"))
                    != Some(true)
                {
                    return refused(403, "sending keys was turned off");
                }
                let current_profile = current.console.profiles.iter().find(|p| p.id == profile_id);
                let Some(current_profile) = current_profile.filter(|p| p.spec(slot) == Some(spec))
                else {
                    return refused(409, "the app console binding changed");
                };
                // the app the macro started for, as the profile named it then and names it now: a profile moved to
                // another app mid-macro doesn't take the rest of the keys with it
                let Some(app) = app
                    .filter(|a| profile.matches(&a.process) && current_profile.matches(&a.process))
                else {
                    return refused(409, "the focused app changed");
                };
                if app.elevated && !self.agent_elevated {
                    return refused(
                        409,
                        "the focused app runs as administrator, so Windows doesn't let the agent send keys to it",
                    );
                }
                // the checks above take a moment: the same window must still be in front as the keys go out
                if app.window != 0 && (self.foreground)() != app.window {
                    return refused(409, "the focused app changed");
                }
                if cancel.is_stopped() {
                    return Err(Cancelled);
                }
                if !(self.send_combo)(combo) {
                    return refused(500, "Windows refused the key press");
                }
                if combos.len() > 1 || times > 1 {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(40)) => {}
                        _ = cancel.stopped() => return Err(Cancelled),
                    }
                }
            }
        }
        Ok((200, None))
    }
}

/// The window in front (0 when there is none, and off Windows).
pub fn foreground_now() -> isize {
    #[cfg(windows)]
    {
        // SAFETY: a plain query of the foreground window
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// The app that owns the foreground window and whether it runs elevated; None off Windows, or when it is gone.
pub fn focused_now() -> Option<FocusedApp> {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId,
        };
        let mut pid = 0u32;
        // SAFETY: plain queries of the foreground window and its process id
        let window = unsafe {
            let window = GetForegroundWindow();
            if window.is_invalid() {
                return None;
            }
            GetWindowThreadProcessId(window, Some(&mut pid as *mut u32));
            window
        };
        if pid == 0 {
            return None;
        }
        let process = crate::privacy_monitor::process_name(pid)?;
        Some(FocusedApp {
            process,
            elevated: elevated(pid),
            window: window.0 as isize,
        })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// True when the process runs elevated, or when its token can't even be read (which is how an elevated one looks from
/// here).
#[cfg(windows)]
fn elevated(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: both handles are closed below; GetTokenInformation writes at most size_of::<TOKEN_ELEVATION>() bytes
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return true;
        };
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token).is_ok();
        let result = if opened {
            let mut elevation = TOKEN_ELEVATION::default();
            let mut returned = 0u32;
            let read = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut TOKEN_ELEVATION as *mut _),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            )
            .is_ok();
            let _ = CloseHandle(token);
            !read || elevation.TokenIsElevated != 0
        } else {
            true
        };
        let _ = CloseHandle(process);
        result
    }
}

#[cfg(test)]
#[path = "app_console_tests.rs"]
mod tests;
