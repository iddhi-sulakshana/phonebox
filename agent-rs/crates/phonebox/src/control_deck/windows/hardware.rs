//! The deck's hardware and platform actions on Windows (`ControlDeck.Windows.cs`): monitors with their DDC/CI
//! brightness and display modes, power plans, services, bringing an app's window to the front or arranging it, the
//! presentation keys, restart, shut down and sleep.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, PHYSICAL_MONITOR, SetMonitorBrightness,
};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, POINTL, RECT};
use windows::Win32::Graphics::Gdi::{
    CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS,
    EnumDisplayMonitors, EnumDisplaySettingsExW, GetMonitorInfoW, HDC, HMONITOR,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::System::Power::SetSuspendState;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT,
    SERVICE_QUERY_STATUS, SERVICE_STATUS,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetForegroundWindow, GetWindow, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, MONITORINFOF_PRIMARY, SW_MAXIMIZE, SW_RESTORE, SW_SHOW, SWP_NOACTIVATE,
    SWP_NOZORDER, SetForegroundWindow, SetWindowPos, ShowWindow,
};
use windows::core::{BOOL, HSTRING, PCWSTR};

use super::super::models::{DeckItem, DeckMonitor};
use super::{
    brightness_level, brightness_percent, layout_rect, monitor_name, parse_display_profile,
    parse_power_plans, presentation_key, process_stem,
};

/// How long the monitors and the power plans are kept (only when there are some).
const CACHE_FOR: Duration = Duration::from_secs(30);
/// How long powercfg, sc and shutdown may run (C#'s `ToolProcess` budget).
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a service may take to reach the state it was asked for.
const SERVICE_WAIT: Duration = Duration::from_secs(12);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn fresh(at: Option<Instant>) -> bool {
    at.is_some_and(|at| at.elapsed() < CACHE_FOR)
}

#[derive(Default)]
struct Cache {
    monitors: Vec<DeckMonitor>,
    monitors_at: Option<Instant>,
    plans: Vec<DeckItem>,
    plans_at: Option<Instant>,
}

/// The monitors and power plans, cached for 30 s under one lock (C#'s `_hardwareGate`).
#[derive(Default)]
pub(super) struct Hardware {
    cache: Mutex<Cache>,
}

impl Hardware {
    pub(super) fn monitors(&self) -> Vec<DeckMonitor> {
        let mut cache = lock(&self.cache);
        if fresh(cache.monitors_at) && !cache.monitors.is_empty() {
            return cache.monitors.clone();
        }
        cache.monitors_at = Some(Instant::now());
        cache.monitors = screens()
            .into_iter()
            .map(|screen| {
                let mode = mode(&screen.name);
                let brightness = physical(screen.handle, |monitor| {
                    let (mut min, mut current, mut max) = (0, 0, 0);
                    // SAFETY: a physical monitor handle from GetPhysicalMonitorsFromHMONITOR, still open
                    let read =
                        unsafe { GetMonitorBrightness(monitor, &mut min, &mut current, &mut max) }
                            != 0;
                    if read {
                        brightness_percent(min, current, max)
                    } else {
                        None
                    }
                });
                DeckMonitor {
                    id: screen.name.clone(),
                    name: monitor_name(&screen.name, screen.primary),
                    brightness,
                    width: mode.dmPelsWidth as i32,
                    height: mode.dmPelsHeight as i32,
                    hz: mode.dmDisplayFrequency as i32,
                }
            })
            .collect();
        cache.monitors.clone()
    }

    pub(super) fn brightness(&self, id: &str, value: i32) -> bool {
        if !(0..=100).contains(&value) {
            return false;
        }
        let Some(screen) = screens().into_iter().find(|s| s.name == id) else {
            return false;
        };
        let ok = physical(screen.handle, |monitor| {
            let (mut min, mut current, mut max) = (0, 0, 0);
            // SAFETY: a physical monitor handle from GetPhysicalMonitorsFromHMONITOR, still open
            let set = unsafe {
                GetMonitorBrightness(monitor, &mut min, &mut current, &mut max) != 0
                    && SetMonitorBrightness(monitor, brightness_level(min, max, value)) != 0
            };
            set.then_some(1)
        }) == Some(1);
        lock(&self.cache).monitors_at = None;
        ok
    }

    pub(super) fn power_plans(&self) -> Vec<DeckItem> {
        let mut cache = lock(&self.cache);
        if fresh(cache.plans_at) && !cache.plans.is_empty() {
            return cache.plans.clone();
        }
        cache.plans_at = Some(Instant::now());
        cache.plans = match run_blocking("powercfg.exe", &["/list"]) {
            Some(output) if output.code == 0 => parse_power_plans(&output.stdout),
            _ => vec![],
        };
        cache.plans.clone()
    }

    /// `PlatformAction`.
    pub(super) async fn act(
        self: Arc<Self>,
        kind: String,
        target: String,
        argument: Option<String>,
        value: i32,
    ) -> bool {
        match kind.as_str() {
            "desktop" => {
                let keys = if value < 0 {
                    "Win+Ctrl+Left"
                } else {
                    "Win+Ctrl+Right"
                };
                blocking(move || crate::key_sender::send_keys(keys).is_ok()).await
            }
            "focus" => blocking(move || focus(&target)).await,
            "window-layout" => {
                blocking(move || layout(&target, argument.as_deref().unwrap_or("left"))).await
            }
            "display-profile" => {
                blocking(move || display_profile(&target, argument.as_deref().unwrap_or("{}")))
                    .await
            }
            "power-plan" => {
                let (me, id) = (self.clone(), target.clone());
                if !blocking(move || me.power_plans().iter().any(|p| p.id == id)).await {
                    return false;
                }
                let done = run("powercfg.exe", &["/setactive", &target]).await;
                lock(&self.cache).plans_at = None;
                done.is_some_and(|o| o.code == 0)
            }
            "service" => service(&target, argument.as_deref()).await,
            "presentation" => {
                if !matches!(target.as_str(), "POWERPNT" | "soffice") {
                    return false;
                }
                blocking(move || {
                    if !focus(&target) {
                        return false;
                    }
                    presentation_key(argument.as_deref())
                        .is_some_and(|key| crate::key_sender::send_keys(key).is_ok())
                })
                .await
            }
            "power" => match target.as_str() {
                "restart" | "shutdown" => {
                    let switch = if target == "restart" { "/r" } else { "/s" };
                    run("shutdown.exe", &[switch, "/t", "5"])
                        .await
                        .is_some_and(|o| o.code == 0)
                }
                "sleep" => {
                    // a moment for the answer to reach the phone first
                    std::thread::spawn(|| {
                        std::thread::sleep(Duration::from_millis(500));
                        // SAFETY: no arguments beyond three flags
                        let _ = unsafe { SetSuspendState(false, false, false) };
                    });
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }
}

async fn blocking(work: impl FnOnce() -> bool + Send + 'static) -> bool {
    tokio::task::spawn_blocking(work).await.unwrap_or(false)
}

/// `%SystemRoot%\System32`.
fn system32() -> PathBuf {
    let mut buffer = [0u16; 260];
    // SAFETY: the buffer and its length
    let n = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if n > 0 && n < buffer.len() {
        PathBuf::from(String::from_utf16_lossy(&buffer[..n]))
    } else {
        PathBuf::from(r"C:\Windows\System32")
    }
}

/// A System32 tool, 5 s at most (its tree killed past that); None when it could not run or timed out.
async fn run(exe: &str, args: &[&str]) -> Option<crate::tool_process::Output> {
    let command =
        crate::tool_process::ToolCommand::new(system32().join(exe)).args(args.iter().copied());
    crate::tool_process::run(&command, TOOL_TIMEOUT, std::future::pending())
        .await
        .ok()
}

/// `run` from a blocking caller, on a thread of its own so the wait never sits inside an async task.
fn run_blocking(exe: &str, args: &[&str]) -> Option<crate::tool_process::Output> {
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    std::thread::scope(|s| {
        s.spawn(|| runtime.block_on(run(exe, args)))
            .join()
            .ok()
            .flatten()
    })
}

/// A service's `dwCurrentState`, 0 when it cannot be read.
pub(super) fn service_state(name: &str) -> u32 {
    // SAFETY: handles opened here are closed before returning; the status is written by the call
    unsafe {
        let Ok(manager) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) else {
            return 0;
        };
        let state = match OpenServiceW(manager, &HSTRING::from(name), SERVICE_QUERY_STATUS) {
            Ok(service) => {
                let mut status = SERVICE_STATUS::default();
                let state = if QueryServiceStatus(service, &mut status).is_ok() {
                    status.dwCurrentState.0
                } else {
                    0
                };
                let _ = CloseServiceHandle(service);
                state
            }
            Err(_) => 0,
        };
        let _ = CloseServiceHandle(manager);
        state
    }
}

/// Start, stop or restart a service through sc.exe, waiting up to 12 s for each state.
async fn service(target: &str, argument: Option<&str>) -> bool {
    let length = target.encode_utf16().count();
    let Some(argument) = argument.filter(|a| matches!(*a, "start" | "stop" | "restart")) else {
        return false;
    };
    if length == 0 || length > 256 {
        return false;
    }
    if matches!(argument, "stop" | "restart") {
        if service_state(target) != 1
            && !run("sc.exe", &["stop", target])
                .await
                .is_some_and(|o| o.code == 0)
        {
            return false;
        }
        if !wait_service(target, 1).await {
            return false;
        }
        if argument == "stop" {
            return true;
        }
    }
    if service_state(target) != 4
        && !run("sc.exe", &["start", target])
            .await
            .is_some_and(|o| o.code == 0)
    {
        return false;
    }
    wait_service(target, 4).await
}

async fn wait_service(name: &str, expected: u32) -> bool {
    let deadline = Instant::now() + SERVICE_WAIT;
    loop {
        if service_state(name) == expected {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        if Instant::now() >= deadline {
            return false;
        }
    }
}

/// A screen (`Screen.AllScreens`): its monitor, device name, whether it is the primary one, and its working area.
struct Screen {
    handle: HMONITOR,
    name: String,
    primary: bool,
}

fn wide(text: &[u16]) -> String {
    let end = text.iter().position(|c| *c == 0).unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

/// A monitor's device name, whether it is the primary one, and its working area.
fn monitor_info(handle: HMONITOR) -> Option<(String, bool, RECT)> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: a MONITORINFOEXW with its size set, passed as the MONITORINFO it starts with
    let read =
        unsafe { GetMonitorInfoW(handle, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) };
    read.as_bool().then(|| {
        (
            wide(&info.szDevice),
            info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            info.monitorInfo.rcWork,
        )
    })
}

fn screens() -> Vec<Screen> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        list: LPARAM,
    ) -> BOOL {
        // SAFETY: `list` is the Vec passed below, alive for the whole enumeration
        let list = unsafe { &mut *(list.0 as *mut Vec<HMONITOR>) };
        list.push(monitor);
        BOOL(1)
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only touches `handles`, which outlives the call
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut handles as *mut Vec<HMONITOR> as isize),
        )
    };
    handles
        .into_iter()
        .filter_map(|handle| {
            monitor_info(handle).map(|(name, primary, _)| Screen {
                handle,
                name,
                primary,
            })
        })
        .collect()
}

/// A display's current mode (zeroed but for its size when it cannot be read, as C# used it).
fn mode(id: &str) -> DEVMODEW {
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: a DEVMODEW with its size set
    let _ = unsafe {
        EnumDisplaySettingsExW(
            &HSTRING::from(id),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
            ENUM_DISPLAY_SETTINGS_FLAGS(0),
        )
    };
    mode
}

/// The first answer `used` gives for one of the monitor's physical monitors (at most 16), which are released after.
fn physical(monitor: HMONITOR, mut used: impl FnMut(HANDLE) -> Option<i32>) -> Option<i32> {
    let mut count = 0u32;
    // SAFETY: the count is written by the call; the array is as long as it says and released before returning
    unsafe {
        if GetNumberOfPhysicalMonitorsFromHMONITOR(monitor, &mut count).is_err()
            || count == 0
            || count > 16
        {
            return None;
        }
        let mut items = vec![PHYSICAL_MONITOR::default(); count as usize];
        if GetPhysicalMonitorsFromHMONITOR(monitor, &mut items).is_err() {
            return None;
        }
        let found = items.iter().find_map(|item| used(item.hPhysicalMonitor));
        let _ = DestroyPhysicalMonitors(&items);
        found
    }
}

/// A display profile applied to a screen: its size, and its refresh rate and position when given.
fn display_profile(id: &str, profile: &str) -> bool {
    if !screens().iter().any(|s| s.name == id) {
        return false;
    }
    let Some(profile) = parse_display_profile(profile) else {
        return false;
    };
    let mut mode = mode(id);
    mode.dmPelsWidth = profile.width;
    mode.dmPelsHeight = profile.height;
    mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
    if profile.hz > 0 {
        mode.dmDisplayFrequency = profile.hz;
        mode.dmFields |= DM_DISPLAYFREQUENCY;
    }
    if let Some((x, y)) = profile.position {
        // SAFETY: a display's mode, so the union holds the display half
        let mut display = unsafe { mode.Anonymous1.Anonymous2 };
        display.dmPosition = POINTL { x, y };
        mode.Anonymous1.Anonymous2 = display;
        mode.dmFields |= DM_POSITION;
    }
    // SAFETY: a DEVMODEW read for this display, the changed fields flagged
    unsafe {
        ChangeDisplaySettingsExW(&HSTRING::from(id), Some(&mode), None, CDS_TYPE(0), None)
            == DISP_CHANGE_SUCCESSFUL
    }
}

/// The top-level windows that are visible and owned by no other window (.NET's main window test), front to back, with
/// their process ids.
fn main_windows() -> Vec<(HWND, u32)> {
    unsafe extern "system" fn collect(window: HWND, list: LPARAM) -> BOOL {
        // SAFETY: `list` is the Vec passed below, alive for the whole enumeration
        let list = unsafe { &mut *(list.0 as *mut Vec<(HWND, u32)>) };
        // SAFETY: plain window queries
        unsafe {
            let unowned = GetWindow(window, GW_OWNER).map_or(true, |owner| owner.is_invalid());
            if IsWindowVisible(window).as_bool() && unowned {
                let mut pid = 0u32;
                GetWindowThreadProcessId(window, Some(&mut pid as *mut u32));
                if pid != 0 {
                    list.push((window, pid));
                }
            }
        }
        BOOL(1)
    }
    let mut list: Vec<(HWND, u32)> = Vec::new();
    // SAFETY: the callback only touches `list`, which outlives the call
    let _ = unsafe {
        EnumWindows(
            Some(collect),
            LPARAM(&mut list as *mut Vec<(HWND, u32)> as isize),
        )
    };
    list
}

fn session_of(pid: u32) -> Option<u32> {
    let mut session = 0u32;
    // SAFETY: the session id is written by the call
    let read = unsafe { ProcessIdToSessionId(pid, &mut session) };
    read.ok().map(|_| session)
}

/// The main window of the app's process in this session (by its file name without the extension, in any case); a
/// folder or an empty name matches nothing.
fn window(process: &str) -> Option<HWND> {
    let name = process_stem(process);
    if name.is_empty() {
        return None;
    }
    let session = session_of(std::process::id())?;
    let windows = main_windows();
    let pids: Vec<u32> = windows.iter().map(|(_, pid)| *pid).collect();
    let named: Vec<u32> = crate::process_monitor::list_processes(Some(&pids))
        .into_iter()
        .filter(|p| p.name.eq_ignore_ascii_case(name))
        .map(|p| p.pid)
        .collect();
    windows
        .into_iter()
        .find(|(_, pid)| named.contains(pid) && session_of(*pid) == Some(session))
        .map(|(window, _)| window)
}

/// Whether the window became the foreground one, waiting up to half a second for a window on another thread.
fn activated(handle: HWND) -> bool {
    for _ in 0..10 {
        // SAFETY: no arguments
        let front = unsafe { GetForegroundWindow() };
        if front == handle {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: no arguments
    let front = unsafe { GetForegroundWindow() };
    front == handle
}

/// Brings the app's window to the front as it was: a minimized one is restored, a maximized one stays maximized
/// (#213). Windows only lets a process that had the last input take the foreground, so a second try follows a
/// zero-pixel mouse move.
fn focus(process: &str) -> bool {
    let Some(handle) = window(process) else {
        return false;
    };
    // SAFETY: plain window calls; a window closed meanwhile makes them fail
    unsafe {
        let _ = ShowWindow(
            handle,
            if IsIconic(handle).as_bool() {
                SW_RESTORE
            } else {
                SW_SHOW
            },
        );
        if SetForegroundWindow(handle).as_bool() && activated(handle) {
            return true;
        }
    }
    crate::key_sender::nudge_input();
    // SAFETY: as above
    let set = unsafe { SetForegroundWindow(handle) };
    set.as_bool() && activated(handle)
}

/// Maximizes the app's window or puts it in the left or right half or the middle of its screen's working area.
fn layout(process: &str, layout: &str) -> bool {
    let Some(handle) = window(process) else {
        return false;
    };
    // SAFETY: plain window and monitor calls; a window closed meanwhile makes them fail
    unsafe {
        let Some((_, _, work)) = monitor_info(MonitorFromWindow(handle, MONITOR_DEFAULTTONEAREST))
        else {
            return false;
        };
        if layout == "max" {
            let _ = ShowWindow(handle, SW_MAXIMIZE);
            return true;
        }
        let Some((x, y, width, height)) = layout_rect(
            work.left,
            work.top,
            work.right - work.left,
            work.bottom - work.top,
            layout,
        ) else {
            return false;
        };
        let _ = ShowWindow(handle, SW_RESTORE);
        SetWindowPos(
            handle,
            None,
            x,
            y,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
        .is_ok()
    }
}
