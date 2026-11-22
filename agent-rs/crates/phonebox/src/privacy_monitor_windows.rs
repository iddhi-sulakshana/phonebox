//! Windows: recording sessions on the microphones (Core Audio), the camera streams Media Foundation reports
//! (`CameraActivity.cs`), and the privacy consent store as the fallback where either can't be read.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
};
use windows::Win32::Media::MediaFoundation::{
    IMFSensorActivitiesReport, IMFSensorActivitiesReportCallback,
    IMFSensorActivitiesReportCallback_Impl, IMFSensorActivityMonitor, MF_VERSION, MFSTARTUP_FULL,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, REG_QWORD, REG_VALUE_TYPE, RegCloseKey, RegEnumKeyExW,
    RegOpenKeyExW, RegQueryValueExW,
};
use windows::core::{HRESULT, HSTRING, Interface, PCWSTR, PWSTR, Ref, implement, s, w};

use super::{
    distinct, friendly_exe, friendly_package, lock, ordinal_distinct, process_name, session_name,
};

/// The apps using a microphone and a camera; the consent store stands in for a side that can't be read.
pub(super) fn read(camera: &CameraActivity) -> (Vec<String>, Vec<String>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    match (capture_sessions(), camera.streaming()) {
        (Some(mic), Some(cam)) => (mic, cam),
        (mic, cam) => {
            let running: HashSet<String> = crate::process_monitor::list_processes(None)
                .into_iter()
                .map(|p| p.name.to_uppercase())
                .collect();
            (
                mic.unwrap_or_else(|| consent_store("microphone", &running)),
                cam.unwrap_or_else(|| consent_store("webcam", &running)),
            )
        }
    }
}

/// Apps with a recording session that is running (not just open) on any active microphone; None when Core Audio
/// can't be read.
fn capture_sessions() -> Option<Vec<String>> {
    let mut apps = Vec::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let all = enumerator
            .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            .ok()?;
        let count = all.GetCount().ok()?;
        for i in 0..count {
            let Ok(device) = all.Item(i) else {
                continue;
            };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let Ok(sessions) = manager.GetSessionEnumerator() else {
                continue;
            };
            let Ok(n) = sessions.GetCount() else {
                continue;
            };
            for j in 0..n {
                // a session that ended meanwhile is skipped
                let Ok(session) = sessions.GetSession(j) else {
                    continue;
                };
                if session.GetState().ok() != Some(AudioSessionStateActive) {
                    continue;
                }
                let Ok(pid) = session
                    .cast::<IAudioSessionControl2>()
                    .and_then(|s| s.GetProcessId())
                else {
                    continue;
                };
                let name = session.GetDisplayName().ok().map(|text| {
                    let name = text.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(text.0 as *const c_void));
                    name
                });
                apps.push(session_name(pid, name.as_deref()));
            }
        }
    }
    Some(distinct(apps))
}

/// The Media Foundation monitor, kept for the life of the agent (Windows stops it when the process ends): it is only
/// held, so that its callback keeps reporting.
struct Monitor(#[allow(dead_code)] IMFSensorActivityMonitor);

// SAFETY: the monitor is created in the multithreaded apartment, where its interface may be used from any thread; it
// is only held, never called after Start.
unsafe impl Send for Monitor {}
unsafe impl Sync for Monitor {}

#[derive(Default)]
enum State {
    #[default]
    NotStarted,
    Running(#[allow(dead_code)] Monitor),
    /// An older Windows (before 10 2004) or a Windows without Media Foundation: the consent store is the only source.
    Failed,
}

/// Which apps are streaming from a camera, as Media Foundation reports it. Windows calls back when a camera starts or
/// stops; the last report is kept. The monitor starts on the first read.
#[derive(Default)]
pub(super) struct CameraActivity {
    state: Mutex<State>,
    streaming: Arc<Mutex<Vec<u32>>>,
}

impl CameraActivity {
    /// Names of the apps streaming now; None when the monitor is unavailable.
    pub(super) fn streaming(&self) -> Option<Vec<String>> {
        if !self.start() {
            return None;
        }
        let pids = lock(&self.streaming).clone();
        // an app that exited no longer counts
        Some(distinct(pids.into_iter().filter_map(process_name)))
    }

    fn start(&self) -> bool {
        let mut state = lock(&self.state);
        match &*state {
            State::Running(_) => return true,
            State::Failed => return false,
            State::NotStarted => {}
        }
        // SAFETY: the two entry points are looked up by name and called with their documented signatures
        *state = match unsafe { self.create() } {
            Some(monitor) => State::Running(Monitor(monitor)),
            None => State::Failed,
        };
        matches!(*state, State::Running(_))
    }

    /// mfplat.dll and mfsensorgroup.dll are loaded when needed (as C#'s DllImport did): a Windows without them (N
    /// editions without the Media Feature Pack, before 10 2004) still starts the agent.
    unsafe fn create(&self) -> Option<IMFSensorActivityMonitor> {
        type Startup = unsafe extern "system" fn(u32, u32) -> HRESULT;
        type Create = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
        unsafe {
            let mfplat = LoadLibraryW(w!("mfplat.dll")).ok()?;
            let startup: Startup = std::mem::transmute(GetProcAddress(mfplat, s!("MFStartup"))?);
            startup(MF_VERSION, MFSTARTUP_FULL).ok().ok()?;
            let sensors = LoadLibraryW(w!("mfsensorgroup.dll")).ok()?;
            let create: Create = std::mem::transmute(GetProcAddress(
                sensors,
                s!("MFCreateSensorActivityMonitor"),
            )?);
            let callback: IMFSensorActivitiesReportCallback = Callback {
                streaming: self.streaming.clone(),
            }
            .into();
            let mut raw = std::ptr::null_mut();
            create(callback.as_raw(), &mut raw).ok().ok()?;
            if raw.is_null() {
                return None;
            }
            let monitor = IMFSensorActivityMonitor::from_raw(raw);
            monitor.Start().ok()?;
            Some(monitor)
        }
    }
}

#[implement(IMFSensorActivitiesReportCallback)]
struct Callback {
    streaming: Arc<Mutex<Vec<u32>>>,
}

impl IMFSensorActivitiesReportCallback_Impl for Callback_Impl {
    fn OnActivitiesReport(
        &self,
        report: Ref<IMFSensorActivitiesReport>,
    ) -> windows::core::Result<()> {
        if let Some(report) = report.as_ref() {
            // SAFETY: the report is valid for the duration of the callback
            *lock(&self.streaming) = unsafe { streaming_pids(report) };
        }
        Ok(())
    }
}

/// The process ids that are streaming in a report. Each report covers every camera.
unsafe fn streaming_pids(report: &IMFSensorActivitiesReport) -> Vec<u32> {
    let mut pids = HashSet::new();
    unsafe {
        let Ok(devices) = report.GetCount() else {
            return vec![];
        };
        for d in 0..devices {
            let Ok(device) = report.GetActivityReport(d) else {
                continue;
            };
            let Ok(count) = device.GetProcessCount() else {
                continue;
            };
            for p in 0..count {
                let Ok(process) = device.GetProcessActivity(p) else {
                    continue;
                };
                if process.GetStreamingState().is_ok_and(|s| s.as_bool())
                    && let Ok(pid) = process.GetProcessId()
                {
                    pids.insert(pid);
                }
            }
        }
    }
    pids.into_iter().collect()
}

/// A registry key, closed when dropped.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

impl Key {
    fn open(parent: HKEY, path: &str) -> Option<Key> {
        let mut key = HKEY::default();
        let path = HSTRING::from(path);
        let opened =
            unsafe { RegOpenKeyExW(parent, PCWSTR(path.as_ptr()), None, KEY_READ, &mut key) };
        (opened == ERROR_SUCCESS).then_some(Key(key))
    }

    fn child(&self, name: &str) -> Option<Key> {
        Key::open(self.0, name)
    }

    fn subkeys(&self) -> Vec<String> {
        let mut names = Vec::new();
        for index in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            let found = unsafe {
                RegEnumKeyExW(
                    self.0,
                    index,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if found != ERROR_SUCCESS {
                break;
            }
            names.push(String::from_utf16_lossy(&name[..len as usize]));
        }
        names
    }

    /// A QWORD value; None when it is missing or of another type.
    fn qword(&self, name: &str) -> Option<i64> {
        let mut kind = REG_VALUE_TYPE::default();
        let mut data = [0u8; 8];
        let mut size = data.len() as u32;
        let name = HSTRING::from(name);
        let read = unsafe {
            RegQueryValueExW(
                self.0,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind as *mut REG_VALUE_TYPE),
                Some(data.as_mut_ptr()),
                Some(&mut size as *mut u32),
            )
        };
        (read == ERROR_SUCCESS && kind == REG_QWORD && size == 8).then(|| i64::from_le_bytes(data))
    }
}

/// Apps whose last use of the capability has started but not stopped. An app that crashed mid-call leaves such an
/// entry behind, so desktop apps must still be running, and store apps must have started within 12 hours.
fn consent_store(capability: &str, running: &HashSet<String>) -> Vec<String> {
    // DateTime.Now.AddHours(-12).ToFileTime(): 100 ns units since 1601 (UTC)
    let stale_before =
        (crate::server::now_ms() - 12 * 3_600_000) * 10_000 + 116_444_736_000_000_000;
    let mut apps = Vec::new();
    let path = format!(
        r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\{capability}"
    );
    if let Some(root) = Key::open(HKEY_CURRENT_USER, &path) {
        scan(&root, false, running, stale_before, &mut apps);
    }
    ordinal_distinct(apps)
}

fn scan(
    key: &Key,
    desktop_apps: bool,
    running: &HashSet<String>,
    stale_before: i64,
    apps: &mut Vec<String>,
) {
    for name in key.subkeys() {
        let Some(sub) = key.child(&name) else {
            continue;
        };
        if name == "NonPackaged" {
            scan(&sub, true, running, stale_before, apps);
            continue;
        }
        let (Some(stop), Some(start)) = (
            sub.qword("LastUsedTimeStop"),
            sub.qword("LastUsedTimeStart"),
        ) else {
            continue;
        };
        if stop != 0 || start <= 0 {
            continue;
        }
        if desktop_apps {
            let exe = friendly_exe(&name);
            if running.contains(&exe.to_uppercase()) {
                apps.push(exe);
            }
        } else if start >= stale_before {
            apps.push(friendly_package(&name));
        }
    }
}
