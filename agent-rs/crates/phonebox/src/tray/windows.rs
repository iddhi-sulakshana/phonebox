//! The notification-area icon of `Tray.cs`, in raw Win32: its own STA thread runs a hidden top-level window, which also
//! hears `WM_QUERYENDSESSION` and the `TaskbarCreated` broadcast (a message-only window hears neither).

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ::windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use ::windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, IPersistFile,
};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::UI::Shell::{
    FOLDERID_Startup, IShellLinkW, KF_FLAG_DEFAULT, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFY_ICON_MESSAGE, NOTIFYICONDATAW, SHGetKnownFolderPath,
    Shell_NotifyIconW, ShellExecuteW, ShellLink,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CREATESTRUCTW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW,
    DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
    GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, HICON, IDI_APPLICATION,
    KillTimer, LR_DEFAULTCOLOR, LoadIconW, MB_ICONWARNING, MB_OK, MF_CHECKED, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, MSG, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SM_CXSMICON, SW_SHOWNORMAL, SetForegroundWindow, SetMenuDefaultItem,
    SetTimer, SetWindowLongPtrW, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
    TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_DESTROY, WM_LBUTTONDBLCLK, WM_NCCREATE, WM_NULL,
    WM_QUERYENDSESSION, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};
use ::windows::core::{HSTRING, Interface, PCWSTR, w};

use super::{Action, MenuItem, Tray, menu, startup_shortcut, tooltip};
use crate::adb::AdbReverse;
use crate::agent_endpoint::AgentEndpoint;
use crate::app_paths;
use crate::log;
use crate::server::Stop;

const CATEGORY: &str = "Tray";
/// The icon's mouse messages.
const WM_TRAY: u32 = WM_APP + 1;
/// The agent is stopping: remove the icon and end the thread.
const WM_STOP: u32 = WM_APP + 2;
/// Every second: the tooltip follows the phones (C#'s `AdbReverse.StateChanged`).
const STATE_TIMER: usize = 1;
const ICON_ID: u32 = 1;
/// Menu ids are 1 + the item's index (`TrackPopupMenu` returns 0 for no choice).
const FIRST_COMMAND: usize = 1;

/// What the window procedure works with; it lives on the tray thread, behind `GWLP_USERDATA`.
struct Context {
    url: String,
    adb: Arc<AdbReverse>,
    stop: Stop,
    runtime: tokio::runtime::Handle,
    icon: HICON,
    /// `TaskbarCreated`: Explorer restarted, so the icon must be added again
    taskbar_created: u32,
    /// the phone list the tooltip last followed
    devices: RefCell<String>,
    tip: RefCell<String>,
}

pub(super) fn start(endpoint: AgentEndpoint, adb: Arc<AdbReverse>, stop: Stop) -> Tray {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        log::warn(CATEGORY, "the tray icon could not start: no runtime");
        return Tray { done: None };
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let (window_tx, window_rx) = tokio::sync::oneshot::channel::<isize>();
    // the icon goes at the start of shutdown (C#'s StopAsync), on the tray thread
    let watch = stop.clone();
    runtime.spawn(async move {
        let Ok(window) = window_rx.await else {
            return;
        };
        watch.stopped().await;
        // SAFETY: posting to a window that is already gone only fails
        unsafe {
            let _ = PostMessageW(
                Some(HWND(window as *mut c_void)),
                WM_STOP,
                WPARAM(0),
                LPARAM(0),
            );
        }
    });
    let url = endpoint.settings_url();
    let spawned = std::thread::Builder::new()
        .name("tray".to_string())
        .spawn(move || {
            let _done = done_tx;
            run(url, adb, stop, runtime, window_tx);
        });
    match spawned {
        Ok(_) => Tray {
            done: Some(done_rx),
        },
        Err(e) => {
            log::warn(CATEGORY, &format!("the tray icon could not start: {e}"));
            Tray { done: None }
        }
    }
}

fn run(
    url: String,
    adb: Arc<AdbReverse>,
    stop: Stop,
    runtime: tokio::runtime::Handle,
    window_tx: tokio::sync::oneshot::Sender<isize>,
) {
    // SAFETY: the context outlives the window: it is freed only after the message loop, once the window is destroyed
    unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let (icon, owned) = load_icon();
        let context = Box::into_raw(Box::new(Context {
            url,
            adb,
            stop,
            runtime,
            icon,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            devices: RefCell::new(String::new()),
            tip: RefCell::new("PhoneBox".to_string()),
        }));
        match create_window(context) {
            Ok(window) => {
                let _ = window_tx.send(window.0 as isize);
                // no notification area yet (Explorer is not running): the window stays for session ending, and
                // TaskbarCreated adds the icon later
                if !notify(window, &*context, NIM_ADD) {
                    log::warn(CATEGORY, "the tray icon could not be added");
                }
                SetTimer(Some(window), STATE_TIMER, 1000, None);
                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            Err(e) => log::warn(
                CATEGORY,
                &format!("the tray icon could not start: {}", e.message()),
            ),
        }
        let context = Box::from_raw(context);
        if owned {
            let _ = DestroyIcon(context.icon);
        }
        drop(context);
        if com.is_ok() {
            CoUninitialize();
        }
    }
}

/// A hidden top-level window (never shown) whose procedure finds the context in `GWLP_USERDATA`.
unsafe fn create_window(context: *mut Context) -> ::windows::core::Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: w!("PhoneBoxTray"),
            ..Default::default()
        };
        // 0 when the class exists already (a second start in one process), which CreateWindowExW then uses
        RegisterClassW(&class);
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("PhoneBoxTray"),
            w!("PhoneBox"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            Some(context as *const c_void),
        )
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the context from WM_NCCREATE on; it outlives the window (see `run`)
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
            return DefWindowProcW(window, message, wparam, lparam);
        }
        let context = GetWindowLongPtrW(window, GWLP_USERDATA) as *const Context;
        let Some(context) = context.as_ref() else {
            return DefWindowProcW(window, message, wparam, lparam);
        };
        match message {
            WM_TRAY => match (lparam.0 & 0xffff) as u32 {
                WM_LBUTTONDBLCLK => shell_open(&context.url, ""),
                WM_RBUTTONUP => show_menu(window, context),
                _ => {}
            },
            WM_TIMER if wparam.0 == STATE_TIMER => follow_phones(window, context),
            // Windows is ending the session: no adb for a minute, so a phone is not reconnected while it goes; allow it
            WM_QUERYENDSESSION => {
                context.adb.mark_session_ending();
                return LRESULT(1);
            }
            WM_STOP => {
                notify(window, context, NIM_DELETE);
                let _ = DestroyWindow(window);
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(window), STATE_TIMER);
                PostQuitMessage(0);
            }
            m if m != 0 && m == context.taskbar_created => {
                notify(window, context, NIM_ADD);
            }
            _ => {}
        }
        DefWindowProcW(window, message, wparam, lparam)
    }
}

/// Adds, updates or removes the icon; false when the shell refused.
fn notify(window: HWND, context: &Context, action: NOTIFY_ICON_MESSAGE) -> bool {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: ICON_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: context.icon,
        ..Default::default()
    };
    let tip: Vec<u16> = context.tip.borrow().encode_utf16().collect();
    let length = tip.len().min(data.szTip.len() - 1);
    data.szTip[..length].copy_from_slice(&tip[..length]);
    // SAFETY: the data is complete and lives through the call
    unsafe { Shell_NotifyIconW(action, &data).as_bool() }
}

/// The tooltip follows the phones once their list changes, as C#'s `StateChanged` handler does.
fn follow_phones(window: HWND, context: &Context) {
    let devices = context.adb.devices().join(",");
    if *context.devices.borrow() == devices {
        return;
    }
    *context.devices.borrow_mut() = devices;
    *context.tip.borrow_mut() = tooltip(context.adb.connected()).to_string();
    notify(window, context, NIM_MODIFY);
}

/// The context menu, built fresh each time it opens (C#'s `Opening` handler refreshes the status and the check).
fn show_menu(window: HWND, context: &Context) {
    let folder = startup_folder();
    let autostart = folder
        .as_deref()
        .is_some_and(|f| startup_shortcut(f).exists());
    let items = menu(
        app_paths::version(),
        context.adb.connected(),
        &context.adb.devices().join(", "),
        autostart,
    );
    // SAFETY: the menu is created, shown and destroyed here; every string outlives its call
    let chosen = unsafe {
        let Ok(popup) = CreatePopupMenu() else {
            return;
        };
        for (index, item) in items.iter().enumerate() {
            let id = FIRST_COMMAND + index;
            match item {
                MenuItem::Separator => {
                    let _ = AppendMenuW(popup, MF_SEPARATOR, 0, PCWSTR::null());
                }
                MenuItem::Item {
                    text,
                    enabled,
                    bold,
                    checked,
                    ..
                } => {
                    let mut flags = MF_STRING;
                    if !enabled {
                        flags |= MF_GRAYED;
                    }
                    if *checked == Some(true) {
                        flags |= MF_CHECKED;
                    }
                    let _ = AppendMenuW(popup, flags, id, &HSTRING::from(text.as_str()));
                    if *bold {
                        let _ = SetMenuDefaultItem(popup, id as u32, 0);
                    }
                }
            }
        }
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        // the menu closes on a click elsewhere only while its window is in front
        let _ = SetForegroundWindow(window);
        let chosen = TrackPopupMenu(
            popup,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            point.x,
            point.y,
            None,
            window,
            None,
        );
        let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(popup);
        usize::try_from(chosen.0).unwrap_or(0)
    };
    if chosen < FIRST_COMMAND || context.stop.is_stopped() {
        return;
    }
    if let Some(MenuItem::Item { action, .. }) = items.get(chosen - FIRST_COMMAND) {
        act(window, context, *action, folder.as_deref());
    }
}

fn act(window: HWND, context: &Context, action: Action, folder: Option<&Path>) {
    match action {
        Action::None => {}
        Action::Settings => shell_open(&context.url, ""),
        Action::OpenPanel => {
            // C# catches only cancellation here, so a failing adb shows WinForms' crash dialog; it is ignored instead
            let adb = context.adb.clone();
            context.runtime.spawn(async move {
                let _ = adb.open_panel().await;
            });
        }
        // the default config, even under --config, as in C#
        Action::EditConfig => shell_open("notepad.exe", &quoted(&app_paths::config_path())),
        Action::OpenLog => shell_open("notepad.exe", &quoted(&app_paths::log_path())),
        Action::Autostart => {
            let result = match (folder, std::env::current_exe()) {
                (Some(folder), Ok(exe)) => toggle_autostart(folder, &exe).map(|_| ()),
                (None, _) => Err("the Startup folder is unknown".to_string()),
                (_, Err(e)) => Err(e.to_string()),
            };
            if let Err(e) = result {
                let text = HSTRING::from(format!("Could not change the startup shortcut: {e}"));
                // SAFETY: both strings live through the call
                unsafe {
                    MessageBoxW(Some(window), &text, w!("PhoneBox"), MB_OK | MB_ICONWARNING);
                }
            }
        }
        Action::Quit => context.stop.stop(),
    }
}

fn quoted(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

/// `Process.Start` with `UseShellExecute = true`: a URL opens in the browser; failures are ignored, as in C#.
fn shell_open(file: &str, parameters: &str) {
    let file = HSTRING::from(file);
    let parameters = HSTRING::from(parameters);
    // SAFETY: the strings live through the call
    unsafe {
        ShellExecuteW(
            None,
            PCWSTR::null(),
            &file,
            &parameters,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// The icon at the notification area's size, from the agent's `.ico`; Windows' application icon when that fails.
/// The flag says whether the icon must be destroyed.
fn load_icon() -> (HICON, bool) {
    let ico = crate::server::static_files::FAVICON;
    // SAFETY: the slice is one whole image of the icon file
    unsafe {
        let size = GetSystemMetrics(SM_CXSMICON).max(16);
        if let Some((offset, length)) = super::icon_entry(ico, size as u32)
            && let Ok(icon) = CreateIconFromResourceEx(
                &ico[offset..offset + length],
                true,
                0x0003_0000,
                size,
                size,
                LR_DEFAULTCOLOR,
            )
        {
            return (icon, true);
        }
        (LoadIconW(None, IDI_APPLICATION).unwrap_or_default(), false)
    }
}

/// The user's Startup folder (`FOLDERID_Startup`), where `PhoneBox.lnk` starts the agent at sign-in.
pub fn startup_folder() -> Option<PathBuf> {
    // SAFETY: the returned string is read, then freed once
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_Startup, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const c_void));
        text.map(PathBuf::from)
    }
}

/// The tray's "Start with Windows": removes `PhoneBox.lnk` from `folder` when it exists, else creates it for `exe`
/// (working folder the exe's, description "PhoneBox agent"). True when the shortcut exists afterwards.
pub fn toggle_autostart(folder: &Path, exe: &Path) -> Result<bool, String> {
    let link = startup_shortcut(folder);
    if link.exists() {
        std::fs::remove_file(&link).map_err(|e| e.to_string())?;
        return Ok(false);
    }
    with_com(|| {
        // SAFETY: COM is initialized on this thread by `with_com`
        unsafe {
            let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            shell.SetPath(&HSTRING::from(exe))?;
            shell.SetWorkingDirectory(&HSTRING::from(exe.parent().unwrap_or(Path::new(""))))?;
            shell.SetDescription(w!("PhoneBox agent"))?;
            shell
                .cast::<IPersistFile>()?
                .Save(&HSTRING::from(link.as_path()), true)
        }
    })
    .map(|()| true)
    .map_err(|e| e.message())
}

/// Runs `f` with COM initialized on this thread (the tray thread has it already; then this only counts up and down).
fn with_com<T>(f: impl FnOnce() -> ::windows::core::Result<T>) -> ::windows::core::Result<T> {
    // SAFETY: each successful initialization is balanced
    unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = f();
        if com.is_ok() {
            CoUninitialize();
        }
        result
    }
}

/// A shortcut's target, working folder and description, read back through `IShellLinkW`.
#[cfg(test)]
pub(crate) fn read_shortcut(link: &Path) -> ::windows::core::Result<(String, String, String)> {
    use ::windows::Win32::System::Com::STGM_READ;
    use ::windows::Win32::UI::Shell::SLGP_RAWPATH;
    let text = |buffer: &[u16]| {
        let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    };
    with_com(|| {
        // SAFETY: COM is initialized on this thread by `with_com`; the buffers outlive the calls
        unsafe {
            let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            shell
                .cast::<IPersistFile>()?
                .Load(&HSTRING::from(link), STGM_READ)?;
            let mut target = [0u16; 1024];
            shell.GetPath(&mut target, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)?;
            let mut folder = [0u16; 1024];
            shell.GetWorkingDirectory(&mut folder)?;
            let mut description = [0u16; 1024];
            shell.GetDescription(&mut description)?;
            Ok((text(&target), text(&folder), text(&description)))
        }
    })
}
