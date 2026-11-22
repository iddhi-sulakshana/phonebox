//! Core Audio (`ControlDeck.Native.cs:27-216`): the active output and input endpoints with their levels, the output
//! endpoints' application sessions with friendly process names, and changing the default endpoint through the
//! undocumented `IPolicyConfig` Windows' own sound settings use.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use indexmap::IndexMap;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, PROPERTYKEY};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, AudioSessionStateExpired, DEVICE_STATE_ACTIVE, EDataFlow, ERole,
    IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator, eCapture, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::core::{
    GUID, HRESULT, HSTRING, IUnknown, IUnknown_Vtbl, Interface, PCWSTR, PWSTR, interface,
};

use super::super::models::{DeckAudio, DeckDevice, DeckSession};
use super::{session_display, useful_name};

/// (raw, friendly) process names by (pid, start time); cleared past 1024 entries.
pub(super) type Names = Mutex<HashMap<(u32, u64), (String, String)>>;

const FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};
const POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// `IPolicyConfig`: only `SetDefaultEndpoint` (the 11th method) is called; the ten before it keep the vtable order.
#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: IUnknown {
    fn get_mix_format(&self) -> HRESULT;
    fn get_device_format(&self) -> HRESULT;
    fn reset_device_format(&self) -> HRESULT;
    fn set_device_format(&self) -> HRESULT;
    fn get_processing_period(&self) -> HRESULT;
    fn set_processing_period(&self) -> HRESULT;
    fn get_share_mode(&self) -> HRESULT;
    fn set_share_mode(&self) -> HRESULT;
    fn get_property_value(&self) -> HRESULT;
    fn set_property_value(&self) -> HRESULT;
    fn set_default_endpoint(&self, id: PCWSTR, role: i32) -> HRESULT;
}

pub(super) fn enumerator() -> Option<IMMDeviceEnumerator> {
    // SAFETY: COM is joined to the multithreaded apartment for this thread (as .NET's thread pool) before the call
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()
    }
}

/// A COM-allocated string, freed.
unsafe fn take(text: PWSTR) -> String {
    // SAFETY: the caller passes a string the COM allocator gave it, freed once here
    unsafe {
        let value = text.to_string().unwrap_or_default();
        CoTaskMemFree(Some(text.0 as *const c_void));
        value
    }
}

/// `(int)Math.Round(level * 100)`: the float product, rounded half to even.
fn percent(level: f32) -> i32 {
    f64::from(level * 100.0).round_ties_even() as i32
}

pub(super) fn device_id(device: &IMMDevice) -> Option<String> {
    // SAFETY: a plain query on a live device; the string is freed by `take`
    unsafe { device.GetId().ok().map(|id| take(id)) }
}

fn friendly_name(device: &IMMDevice) -> String {
    // SAFETY: plain property reads on a live device; the converted string is freed by `take`
    unsafe {
        let Ok(store) = device.OpenPropertyStore(STGM_READ) else {
            return String::new();
        };
        let Ok(value) = store.GetValue(&FRIENDLY_NAME) else {
            return String::new();
        };
        PropVariantToStringAlloc(&value)
            .map(|text| take(text))
            .unwrap_or_default()
    }
}

/// The active devices and the output sessions; on a failure part-way, what was read so far as unavailable.
pub(super) fn read(names: &Names) -> DeckAudio {
    let mut devices = Vec::new();
    let mut sessions = Vec::new();
    let mut defaults = IndexMap::new();
    let read_all = (|| -> Option<()> {
        let enumerator = enumerator()?;
        for (flow, direction) in [(eRender, "output"), (eCapture, "input")] {
            let mut default_id = None;
            for role in 0..3 {
                // SAFETY: plain queries on the enumerator
                let current = unsafe { enumerator.GetDefaultAudioEndpoint(flow, ERole(role)) };
                if let Some(id) = current.ok().as_ref().and_then(device_id) {
                    defaults.insert(format!("{direction}:{role}"), id.clone());
                    if role == 1 {
                        default_id = Some(id);
                    }
                }
            }
            // SAFETY: plain queries on the enumerator and its collection
            let all = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }.ok()?;
            let count = unsafe { all.GetCount() }.ok()?;
            for i in 0..count {
                // a device that goes away meanwhile is skipped
                let Ok(device) = (unsafe { all.Item(i) }) else {
                    continue;
                };
                let _ = read_device(
                    &device,
                    flow,
                    direction,
                    default_id.as_deref(),
                    names,
                    &mut devices,
                    &mut sessions,
                );
            }
        }
        Some(())
    })();
    match read_all {
        Some(()) => DeckAudio {
            available: true,
            error: None,
            devices,
            sessions,
            defaults: Some(defaults),
        },
        None => DeckAudio {
            available: false,
            error: Some("Windows audio is unavailable".into()),
            devices,
            sessions,
            defaults: None,
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn read_device(
    device: &IMMDevice,
    flow: EDataFlow,
    direction: &str,
    default_id: Option<&str>,
    names: &Names,
    devices: &mut Vec<DeckDevice>,
    sessions: &mut Vec<DeckSession>,
) -> Option<()> {
    let id = device_id(device)?;
    // SAFETY: plain reads on a live device's interfaces
    unsafe {
        let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
        let meter: IAudioMeterInformation = device.Activate(CLSCTX_ALL, None).ok()?;
        let channels = volume.GetChannelCount().ok()?;
        let channel_volumes: Vec<i32> = (0..channels)
            .map(|c| {
                volume
                    .GetChannelVolumeLevelScalar(c)
                    .map(percent)
                    .unwrap_or(0)
            })
            .collect();
        devices.push(DeckDevice {
            id: id.clone(),
            name: friendly_name(device),
            direction: direction.into(),
            default: Some(id.as_str()) == default_id,
            volume: percent(volume.GetMasterVolumeLevelScalar().ok()?),
            muted: volume.GetMute().ok()?.as_bool(),
            peak: f64::from(meter.GetPeakValue().ok()?),
            channels: channels as i32,
            can_control: true,
            channel_volumes: Some(channel_volumes),
        });
        if flow != eRender {
            return Some(());
        }
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None).ok()?;
        let list = manager.GetSessionEnumerator().ok()?;
        for i in 0..list.GetCount().ok()? {
            // a session can end during the enumeration: the others are kept
            let Ok(control) = list.GetSession(i) else {
                continue;
            };
            let Ok(control) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            let Ok(pid) = control.GetProcessId() else {
                continue;
            };
            if control.GetState().ok() == Some(AudioSessionStateExpired) {
                continue;
            }
            let (raw, friendly) = process_names(pid, names);
            let display = control.GetDisplayName().ok().map(|text| take(text));
            let (Ok(instance), Ok(simple), Ok(meter)) = (
                control.GetSessionInstanceIdentifier(),
                control.cast::<ISimpleAudioVolume>(),
                control.cast::<IAudioMeterInformation>(),
            ) else {
                continue;
            };
            let (Ok(level), Ok(muted), Ok(peak), Ok(state)) = (
                simple.GetMasterVolume(),
                simple.GetMute(),
                meter.GetPeakValue(),
                control.GetState(),
            ) else {
                take(instance);
                continue;
            };
            sessions.push(DeckSession {
                id: take(instance),
                name: session_display(pid, display.as_deref(), &friendly),
                pid: pid as i32,
                device_id: id.clone(),
                volume: percent(level),
                muted: muted.as_bool(),
                peak: f64::from(peak),
                process_name: Some(raw),
                active: state == AudioSessionStateActive,
            });
        }
    }
    Some(())
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcess and is closed once
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The process's (raw, friendly) names: the system's sounds for pid 0; a process whose start time can't be read (a
/// protected service) shows its name, uncached; otherwise the package's display name, the file description, the
/// product name or the process name, whichever is first worth showing, cached per (pid, start time).
fn process_names(pid: u32, names: &Names) -> (String, String) {
    if pid == 0 {
        return ("System sounds".into(), "System sounds".into());
    }
    // SAFETY: plain process queries; the handle is closed by its guard, the buffers hold what the calls are told
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return (String::new(), "Application".into());
        };
        let handle = Handle(handle);
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        if QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
        .is_err()
        {
            return (String::new(), "Application".into());
        }
        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        let file = path.rsplit(['\\', '/']).next().unwrap_or("");
        let raw = file.rfind('.').map_or(file, |dot| &file[..dot]).to_string();
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        if GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user).is_err() {
            return (raw.clone(), raw);
        }
        let started = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        let key = (pid, started);
        if let Some(cached) = names.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return cached.clone();
        }
        let friendly = [
            package_display_name(handle.0),
            version_string(&path, "FileDescription"),
            version_string(&path, "ProductName"),
        ]
        .into_iter()
        .find(|name| useful_name(name.as_deref(), &raw))
        .flatten()
        .unwrap_or_else(|| raw.clone());
        let mut cache = names.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() > 1024 {
            cache.clear();
        }
        cache.entry(key).or_insert((raw, friendly)).clone()
    }
}

/// A packaged app's display name (Windows' own mixer shows it for helper executables such as DCv2.exe).
fn package_display_name(process: HANDLE) -> Option<String> {
    use windows::Win32::Storage::Packaging::Appx::GetPackageFullName;
    let mut length = 0u32;
    // SAFETY: the first call only reports the length; the second gets a buffer of that length
    let full_name = unsafe {
        let _ = GetPackageFullName(process, &mut length, None);
        if length == 0 {
            return None;
        }
        let mut buffer = vec![0u16; length as usize];
        if GetPackageFullName(process, &mut length, Some(PWSTR(buffer.as_mut_ptr()))).0 != 0 {
            return None;
        }
        String::from_utf16_lossy(&buffer[..(length as usize).saturating_sub(1)])
    };
    let manager = windows::Management::Deployment::PackageManager::new().ok()?;
    let package = manager
        .FindPackageByUserSecurityIdPackageFullName(&HSTRING::new(), &HSTRING::from(full_name))
        .ok()?;
    package.DisplayName().ok().map(|name| name.to_string())
}

/// A string of a file's version resource (`FileVersionInfo`): the first language it lists, else the usual English
/// code pages.
fn version_string(path: &str, field: &str) -> Option<String> {
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
    // SAFETY: `wide` is NUL-terminated; the data buffer has the size the first call reported; VerQueryValueW returns
    // pointers into that buffer, which outlives their use
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, data.as_mut_ptr().cast()).ok()?;
        let query = |sub: &str| -> Option<(*const u8, u32)> {
            let sub: Vec<u16> = sub.encode_utf16().chain([0]).collect();
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            VerQueryValueW(
                data.as_ptr().cast(),
                PCWSTR(sub.as_ptr()),
                &mut ptr,
                &mut len,
            )
            .as_bool()
            .then_some((ptr as *const u8, len))
        };
        let mut codes = vec![];
        if let Some((ptr, len)) = query("\\VarFileInfo\\Translation")
            && len >= 4
        {
            let lang = u16::from_le_bytes([*ptr, *ptr.add(1)]);
            let page = u16::from_le_bytes([*ptr.add(2), *ptr.add(3)]);
            codes.push(format!("{lang:04X}{page:04X}"));
        }
        codes.extend(["040904B0", "040904E4", "04090000"].map(String::from));
        for code in codes {
            if let Some((ptr, len)) = query(&format!("\\StringFileInfo\\{code}\\{field}"))
                && len > 0
            {
                let units = std::slice::from_raw_parts(ptr as *const u16, len as usize);
                let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
                return Some(String::from_utf16_lossy(&units[..end]));
            }
        }
        None
    }
}

/// Whether this output exists and is active.
pub(super) fn device_active(id: &str) -> bool {
    let Some(enumerator) = enumerator() else {
        return false;
    };
    // SAFETY: plain calls on the enumerator and the device it returns
    unsafe {
        enumerator
            .GetDevice(&HSTRING::from(id))
            .and_then(|device| device.GetState())
            .ok()
            == Some(DEVICE_STATE_ACTIVE)
    }
}

/// An active device's master or channel volume (0–100) and mute.
pub(super) fn set_device(
    id: &str,
    volume: Option<i32>,
    muted: Option<bool>,
    channel: Option<i32>,
) -> bool {
    let Some(enumerator) = enumerator() else {
        return false;
    };
    // SAFETY: plain calls on a live device's endpoint volume; null event contexts
    unsafe {
        let Ok(device) = enumerator.GetDevice(&HSTRING::from(id)) else {
            return false;
        };
        if device.GetState().ok() != Some(DEVICE_STATE_ACTIVE) {
            return false;
        }
        let Ok(endpoint) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) else {
            return false;
        };
        if let Some(v) = volume {
            if !(0..=100).contains(&v) {
                return false;
            }
            let level = v as f32 / 100.0;
            let set = match channel {
                Some(c) => {
                    let count = endpoint.GetChannelCount().unwrap_or(0);
                    if c < 0 || c as u32 >= count {
                        return false;
                    }
                    endpoint.SetChannelVolumeLevelScalar(c as u32, level, std::ptr::null())
                }
                None => endpoint.SetMasterVolumeLevelScalar(level, std::ptr::null()),
            };
            if set.is_err() {
                return false;
            }
        }
        if let Some(m) = muted
            && endpoint.SetMute(m, std::ptr::null()).is_err()
        {
            return false;
        }
        true
    }
}

/// An output session's volume (0–100) and mute, found by its instance id on any active output.
pub(super) fn set_session(id: &str, volume: Option<i32>, muted: Option<bool>) -> bool {
    if volume.is_some_and(|v| !(0..=100).contains(&v)) {
        return false;
    }
    let Some(enumerator) = enumerator() else {
        return false;
    };
    // SAFETY: plain calls on live session interfaces; null event contexts
    unsafe {
        let Ok(all) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return false;
        };
        for i in 0..all.GetCount().unwrap_or(0) {
            let Ok(device) = all.Item(i) else { continue };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let Ok(list) = manager.GetSessionEnumerator() else {
                continue;
            };
            for j in 0..list.GetCount().unwrap_or(0) {
                let Ok(control) = list
                    .GetSession(j)
                    .and_then(|c| c.cast::<IAudioSessionControl2>())
                else {
                    continue;
                };
                let Ok(instance) = control.GetSessionInstanceIdentifier() else {
                    continue;
                };
                if take(instance) != id {
                    continue;
                }
                let Ok(simple) = control.cast::<ISimpleAudioVolume>() else {
                    return false;
                };
                if let Some(v) = volume {
                    let _ = simple.SetMasterVolume(v as f32 / 100.0, std::ptr::null());
                }
                if let Some(m) = muted {
                    let _ = simple.SetMute(m, std::ptr::null());
                }
                return true;
            }
        }
        false
    }
}

/// Makes a listed device the default for one role (0 console, 1 multimedia, 2 communications) or all three, then
/// checks Windows reports it so.
pub(super) fn set_default(names: &Names, id: &str, role: Option<i32>) -> bool {
    let before = read(names);
    let Some(direction) = before
        .devices
        .iter()
        .find(|d| d.id == id)
        .map(|d| d.direction.clone())
    else {
        return false;
    };
    if role.is_some_and(|r| !(0..=2).contains(&r)) {
        return false;
    }
    let roles: Vec<i32> = role.map_or_else(|| vec![0, 1, 2], |r| vec![r]);
    // SAFETY: the policy object is created in this thread's apartment; the id string outlives the calls
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(policy) = CoCreateInstance::<_, IPolicyConfig>(&POLICY_CONFIG, None, CLSCTX_ALL)
        else {
            return false;
        };
        let wide = HSTRING::from(id);
        for r in &roles {
            if policy
                .set_default_endpoint(PCWSTR(wide.as_ptr()), *r)
                .is_err()
            {
                return false;
            }
        }
    }
    let after = read(names);
    roles.iter().all(|r| {
        after
            .defaults
            .as_ref()
            .and_then(|d| d.get(&format!("{direction}:{r}")))
            .is_some_and(|d| d == id)
    })
}
