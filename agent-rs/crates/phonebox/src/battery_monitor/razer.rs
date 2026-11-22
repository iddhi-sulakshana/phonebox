//! Port of `BatteryMonitor.Razer.cs`: Razer USB HID devices (Windows only, as in C#): the 91-byte feature reports of
//! mice and keyboards, and the "PA" frames of the BlackShark V2 Pro and Barracuda Pro headsets' receivers.

use super::DeviceBattery;

pub const VENDOR_ID: u16 = 0x1532;
/// 1-byte HID report id (0x00) + the 90-byte Razer frame.
pub const REPORT_LENGTH: usize = 91;
pub const PA_REPORT_LENGTH: usize = 64;
pub const CLASS_POWER: u8 = 0x07;
pub const CMD_BATTERY: u8 = 0x80;
pub const CMD_CHARGING: u8 = 0x84;
pub const STATUS_NEW: u8 = 0x00;
pub const STATUS_BUSY: u8 = 0x01;
pub const STATUS_OK: u8 = 0x02;
pub const STATUS_TIMEOUT: u8 = 0x04;
pub const STATUS_NOT_SUPPORTED: u8 = 0x05;
pub const PA_REPORT_ID: u8 = 0x02;
pub const BARRACUDA_REPORT_ID: u8 = 0x01;
pub const BARRACUDA_PID: u16 = 0x053A;
pub const PA_CMD_BATTERY: u8 = 0x21;
pub const PA_CMD_CHARGING: u8 = 0x2A;

/// Transaction ids tried in turn; the one that worked is remembered per product.
pub const TX_CANDIDATES: [u8; 3] = [0x1F, 0x3F, 0xFF];
/// Overall budget for a Razer scan.
#[cfg(windows)]
const TOTAL_BUDGET_MS: i64 = 1500;
/// The headset receivers that speak the "PA" protocol (frame offsets verified from real hardware captures).
const PA_PIDS: [u16; 2] = [0x0555, BARRACUDA_PID];

/// Known Razer products: (name, kind). Mice and keyboards from OpenRazer's drivers; the two PA headsets.
const DEVICES: [(u16, &str, &str); 62] = [
    (0x0045, "Razer Mamba Wireless", "mouse"),
    (0x006F, "Razer Lancehead Wireless", "mouse"),
    (0x0072, "Razer Mamba Wireless", "mouse"),
    (0x0077, "Razer Pro Click", "mouse"),
    (0x007A, "Razer Viper Ultimate", "mouse"),
    (0x007B, "Razer Viper Ultimate", "mouse"),
    (0x007C, "Razer DeathAdder V2 Pro", "mouse"),
    (0x007D, "Razer DeathAdder V2 Pro", "mouse"),
    (0x0083, "Razer Basilisk X HyperSpeed", "mouse"),
    (0x0086, "Razer Basilisk Ultimate", "mouse"),
    (0x0088, "Razer Basilisk Ultimate", "mouse"),
    (0x008F, "Razer Naga Pro", "mouse"),
    (0x0090, "Razer Naga Pro", "mouse"),
    (0x0094, "Razer Orochi V2", "mouse"),
    (0x009A, "Razer Pro Click Mini", "mouse"),
    (0x009C, "Razer DeathAdder V2 X HyperSpeed", "mouse"),
    (0x009E, "Razer Viper Mini SE", "mouse"),
    (0x009F, "Razer Viper Mini SE", "mouse"),
    (0x00A5, "Razer Viper V2 Pro", "mouse"),
    (0x00A6, "Razer Viper V2 Pro", "mouse"),
    (0x00A7, "Razer Naga V2 Pro", "mouse"),
    (0x00A8, "Razer Naga V2 Pro", "mouse"),
    (0x00AA, "Razer Basilisk V3 Pro", "mouse"),
    (0x00AB, "Razer Basilisk V3 Pro", "mouse"),
    (0x00AF, "Razer Cobra Pro", "mouse"),
    (0x00B0, "Razer Cobra Pro", "mouse"),
    (0x00B4, "Razer Naga V2 HyperSpeed", "mouse"),
    (0x00B6, "Razer DeathAdder V3 Pro", "mouse"),
    (0x00B7, "Razer DeathAdder V3 Pro", "mouse"),
    (0x00B8, "Razer Viper V3 HyperSpeed", "mouse"),
    (0x00B9, "Razer Basilisk V3 X HyperSpeed", "mouse"),
    (0x00BE, "Razer DeathAdder V4 Pro", "mouse"),
    (0x00BF, "Razer DeathAdder V4 Pro", "mouse"),
    (0x00C0, "Razer Viper V3 Pro", "mouse"),
    (0x00C1, "Razer Viper V3 Pro", "mouse"),
    (0x00C2, "Razer DeathAdder V3 Pro", "mouse"),
    (0x00C3, "Razer DeathAdder V3 Pro", "mouse"),
    (0x00C4, "Razer DeathAdder V3 HyperSpeed", "mouse"),
    (0x00C5, "Razer DeathAdder V3 HyperSpeed", "mouse"),
    (0x00CC, "Razer Basilisk V3 Pro 35K", "mouse"),
    (0x00CD, "Razer Basilisk V3 Pro 35K", "mouse"),
    (0x00D0, "Razer Pro Click V2", "mouse"),
    (0x00D1, "Razer Pro Click V2", "mouse"),
    (0x00DB, "Razer Cobra HyperSpeed", "mouse"),
    (0x00DE, "Razer Viper V3 Pro SE", "mouse"),
    (0x00DF, "Razer Viper V3 Pro SE", "mouse"),
    (0x00E5, "Razer Viper V4 Pro", "mouse"),
    (0x00E6, "Razer Viper V4 Pro", "mouse"),
    (0x0258, "Razer BlackWidow V3 Mini HyperSpeed", "keyboard"),
    (0x025A, "Razer BlackWidow V3 Pro", "keyboard"),
    (0x025C, "Razer BlackWidow V3 Pro", "keyboard"),
    (0x0271, "Razer BlackWidow V3 Mini HyperSpeed", "keyboard"),
    (0x0290, "Razer DeathStalker V2 Pro", "keyboard"),
    (0x0292, "Razer DeathStalker V2 Pro", "keyboard"),
    (0x0296, "Razer DeathStalker V2 Pro TKL", "keyboard"),
    (0x0298, "Razer DeathStalker V2 Pro TKL", "keyboard"),
    (0x02B9, "Razer BlackWidow V4 Mini HyperSpeed", "keyboard"),
    (0x02BA, "Razer BlackWidow V4 Mini HyperSpeed", "keyboard"),
    (0x02D5, "Razer BlackWidow V4 TKL HyperSpeed", "keyboard"),
    (0x02D7, "Razer BlackWidow V4 TKL HyperSpeed", "keyboard"),
    (BARRACUDA_PID, "Razer Barracuda Pro (2.4 GHz)", "headphones"),
    (0x0555, "Razer BlackShark V2 Pro", "headphones"),
];

/// A 91-byte HID feature report querying a Razer power property (0x07 / 0x80 or 0x84).
pub fn build_report(tx: u8, command: u8) -> Vec<u8> {
    let mut buf = vec![0u8; REPORT_LENGTH];
    buf[1] = STATUS_NEW;
    buf[2] = tx;
    buf[6] = 0x02;
    buf[7] = CLASS_POWER;
    buf[8] = command;
    buf[89] = buf[3..=88].iter().fold(0, |crc, b| crc ^ b);
    buf
}

/// The battery percentage (1-100) of a 0x07/0x80 response, or None when asleep or invalid.
pub fn parse_battery(report: &[u8]) -> Option<i32> {
    if report.len() < REPORT_LENGTH
        || report[1] != STATUS_OK
        || report[7] != CLASS_POWER
        || report[8] != CMD_BATTERY
    {
        return None;
    }
    // wireless dongles report 0 when the peripheral is asleep or out of range
    let raw = report[10];
    (raw != 0).then(|| {
        (crate::system_monitor::round_dotnet(f64::from(raw) * 100.0 / 255.0, 0) as i32)
            .clamp(0, 100)
    })
}

/// The charging state of a 0x07/0x84 response, or None when invalid.
pub fn parse_charging(report: &[u8]) -> Option<bool> {
    if report.len() < REPORT_LENGTH
        || report[1] != STATUS_OK
        || report[7] != CLASS_POWER
        || report[8] != CMD_CHARGING
    {
        return None;
    }
    Some(report[10] != 0)
}

/// The 64-byte "PA" remote/telemetry mode frame (command 0xE1: 1 = on, 0 = off). The BlackShark V2 Pro 2023 (0x0555)
/// 2.4 GHz receiver dozes after ~300 ms idle and ignores queries unless woken and put into remote mode first.
pub fn build_pa_remote_frame(on: bool) -> Vec<u8> {
    let mut buf = vec![0u8; PA_REPORT_LENGTH];
    buf[0] = PA_REPORT_ID;
    buf[1] = 0x80;
    buf[2] = 0x07;
    buf[5] = b'P';
    buf[6] = b'A';
    buf[7] = 0x0E;
    buf[9] = 0x02;
    buf[10] = 0xE1;
    buf[11] = u8::from(on);
    buf
}

pub fn build_pa_query_frame(cmd: u8) -> Vec<u8> {
    let mut buf = vec![0u8; PA_REPORT_LENGTH];
    buf[0] = PA_REPORT_ID;
    buf[1] = 0x80;
    buf[2] = 0x08;
    buf[5] = b'P';
    buf[6] = b'A';
    buf[7] = 0x08;
    buf[9] = 0x03;
    buf[10] = cmd;
    buf
}

pub fn parse_pa_reply(report: &[u8], cmd: u8) -> Option<u8> {
    (report.len() >= 16 && report[0] == PA_REPORT_ID && report[12] == cmd && report[13] == 0x01)
        .then(|| report[15])
}

/// Barracuda Pro PA frames: its request and reply fields sit two bytes earlier than the BlackShark's.
pub fn build_barracuda_remote_frame(on: bool) -> Vec<u8> {
    let mut buf = vec![0u8; PA_REPORT_LENGTH];
    buf[..10].copy_from_slice(&[
        BARRACUDA_REPORT_ID,
        0x80,
        0x07,
        b'P',
        b'A',
        0x0E,
        0x08,
        0x02,
        0xE1,
        u8::from(on),
    ]);
    buf
}

pub fn build_barracuda_query_frame(cmd: u8) -> Vec<u8> {
    let mut buf = vec![0u8; PA_REPORT_LENGTH];
    buf[..9].copy_from_slice(&[
        BARRACUDA_REPORT_ID,
        0x80,
        0x08,
        b'P',
        b'A',
        0x08,
        0x08,
        0x03,
        cmd,
    ]);
    buf
}

/// A Barracuda reply, with its report id byte or without it.
pub fn parse_barracuda_reply(report: &[u8], cmd: u8) -> Option<u8> {
    for offset in [0isize, -1] {
        if offset == -1 && report.first() == Some(&BARRACUDA_REPORT_ID) {
            continue;
        }
        let (marker, command) = ((3 + offset) as usize, (13 + offset) as usize);
        if report.len() <= command + 3 {
            continue;
        }
        if report[marker] != b'P' || report[marker + 1] != b'I' {
            continue;
        }
        if report[command] != cmd || !matches!(report[command + 1], 0x01 | 0x02) {
            continue;
        }
        let length = usize::from(report[command + 2]);
        if (1..=32).contains(&length) && report.len() >= command + 3 + length {
            return Some(report[command + 3]);
        }
    }
    None
}

pub fn device_info(pid: u16) -> Option<(&'static str, &'static str)> {
    DEVICES.iter().find(|d| d.0 == pid).map(|d| (d.1, d.2))
}

pub fn kind_from_pid(pid: u16) -> Option<&'static str> {
    device_info(pid).map(|d| d.1)
}

/// The products asked through 91-byte feature reports (every known one but the PA headsets).
pub fn feature_pids() -> Vec<u16> {
    DEVICES
        .iter()
        .map(|d| d.0)
        .filter(|p| !PA_PIDS.contains(p))
        .collect()
}

/// The 91-byte feature query policy: never probes unknown or PA-headset products, tries the remembered transaction id
/// first, stops trying other ids on OK or Timeout, and respects the overall deadline. (level, charging, working tx).
pub fn query_feature_device(
    pid: u16,
    preferred_tx: Option<u8>,
    send: &mut dyn FnMut(&[u8]) -> Option<Vec<u8>>,
    deadline_ms: Option<i64>,
    clock: Option<&dyn Fn() -> i64>,
) -> (Option<i32>, Option<bool>, Option<u8>) {
    if device_info(pid).is_none() || PA_PIDS.contains(&pid) {
        return (None, None, None);
    }
    let fallback = default_clock;
    let now: &dyn Fn() -> i64 = clock.unwrap_or(&fallback);
    let mut order = TX_CANDIDATES.to_vec();
    if let Some(pref) = preferred_tx {
        order.sort_by_key(|t| u8::from(*t != pref)); // stable: the others keep their order
    }
    for tx in order {
        if deadline_ms.is_some_and(|dl| now() >= dl) {
            break;
        }
        let Some(battery) = query_feature_command(tx, CMD_BATTERY, send, deadline_ms, now) else {
            continue;
        };
        match battery[1] {
            STATUS_TIMEOUT => return (None, None, Some(tx)),
            STATUS_OK => {}
            _ => continue,
        }
        let Some(level) = parse_battery(&battery) else {
            return (None, None, Some(tx));
        };
        let charging = query_feature_command(tx, CMD_CHARGING, send, deadline_ms, now)
            .and_then(|r| parse_charging(&r));
        return (Some(level), charging, Some(tx));
    }
    (None, None, None)
}

fn default_clock() -> i64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as i64
}

/// Up to three tries while the device is busy; a timeout, "not supported" or an unexpected status is the answer.
fn query_feature_command(
    tx: u8,
    command: u8,
    send: &mut dyn FnMut(&[u8]) -> Option<Vec<u8>>,
    deadline_ms: Option<i64>,
    now: &dyn Fn() -> i64,
) -> Option<Vec<u8>> {
    let req = build_report(tx, command);
    for _ in 0..3 {
        if deadline_ms.is_some_and(|dl| now() >= dl) {
            return None;
        }
        let resp = send(&req)?;
        if resp.len() < REPORT_LENGTH {
            return None;
        }
        match resp[1] {
            STATUS_TIMEOUT | STATUS_NOT_SUPPORTED => return Some(resp),
            STATUS_OK if resp[7] == CLASS_POWER && resp[8] == command => return Some(resp),
            STATUS_BUSY | STATUS_OK => {}
            _ => return Some(resp),
        }
    }
    None
}

/// Merges Razer readings by product name (preferring the one with a charging state, when a cable and a wireless dongle
/// of the same model are both connected) into the Bluetooth list, without collapsing duplicate Bluetooth entries.
pub fn merge_into_bluetooth(
    bluetooth: &[DeviceBattery],
    razer: &[DeviceBattery],
) -> Vec<DeviceBattery> {
    let mut list = bluetooth.to_vec();
    for r in razer {
        match list
            .iter()
            .position(|b| b.name.to_lowercase() == r.name.to_lowercase())
        {
            None => list.push(r.clone()),
            Some(i) if prefer(r, &list[i]) => list[i] = r.clone(),
            Some(_) => {}
        }
    }
    list
}

fn prefer(candidate: &DeviceBattery, existing: &DeviceBattery) -> bool {
    (candidate.charging.is_some() && existing.charging.is_none())
        || (candidate.charging == Some(true) && existing.charging != Some(true))
        || (candidate.charging == existing.charging && candidate.level > existing.level)
}

#[cfg(windows)]
pub(super) use windows_hid::read_windows;

#[cfg(windows)]
mod windows_hid {
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_Device_IDW, CM_Get_Parent, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
        SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
        SetupDiGetDeviceInterfaceDetailW,
    };
    use windows::Win32::Devices::HumanInterfaceDevice::{
        HIDD_ATTRIBUTES, HIDP_CAPS, HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature,
        HidD_GetPreparsedData, HidD_GetProductString, HidD_SetFeature, HidP_GetCaps,
        PHIDP_PREPARSED_DATA,
    };
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_IO_PENDING, GetLastError, HANDLE, WAIT_OBJECT_0,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, WriteFile,
    };
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::core::{GUID, PCWSTR};

    use super::*;

    const HID_INTERFACE: GUID = GUID::from_u128(0x4D1E55B2_F16F_11CF_88CB_001111000030);
    /// Synapse 4 runs as RazerAppEngine; Synapse 3 as Razer Synapse 3 / Razer Synapse Service.
    const SYNAPSE: [&str; 4] = [
        "RazerAppEngine",
        "Razer Synapse 3",
        "Razer Synapse Service",
        "RazerSynapse",
    ];

    static PREFERRED_TX: Mutex<Option<HashMap<u16, u8>>> = Mutex::new(None);

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: a handle from CreateFileW, closed once
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn now() -> i64 {
        super::default_clock()
    }

    fn open(path: &[u16], access: u32, flags: FILE_FLAGS_AND_ATTRIBUTES) -> Option<Handle> {
        // SAFETY: `path` is NUL-terminated
        unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                flags,
                None,
            )
        }
        .ok()
        .map(Handle)
    }

    /// Every Razer HID device that answers, merged by name; the first error seen, for the debug log.
    pub(in super::super) fn read_windows() -> (Vec<DeviceBattery>, Option<String>) {
        let mut readings = vec![];
        let mut handled: HashSet<String> = HashSet::new();
        let deadline = now() + TOTAL_BUDGET_MS;
        let mut synapse_running: Option<bool> = None;
        // SAFETY: the device set is destroyed at the end; the detail buffer has the size the first call asked for
        // (u32s keep it aligned) and its cbSize is the fixed struct size, as SetupAPI requires
        unsafe {
            let Ok(set) = SetupDiGetClassDevsW(
                Some(&HID_INTERFACE),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            ) else {
                return (vec![], None);
            };
            let mut if_data = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            let mut i = 0;
            while now() < deadline
                && SetupDiEnumDeviceInterfaces(set, None, &HID_INTERFACE, i, &mut if_data).is_ok()
            {
                i += 1;
                let mut required = 0u32;
                let _ = SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &if_data,
                    None,
                    0,
                    Some(&mut required),
                    None,
                );
                if required == 0 {
                    continue;
                }
                let mut mem = vec![0u32; (required as usize).div_ceil(4)];
                let detail = mem.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
                (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
                let mut dev = SP_DEVINFO_DATA {
                    cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                    ..Default::default()
                };
                if SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &if_data,
                    Some(detail),
                    required,
                    None,
                    Some(&mut dev),
                )
                .is_err()
                {
                    continue;
                }
                let path_ptr = std::ptr::addr_of!((*detail).DevicePath) as *const u16;
                let max = (required as usize).saturating_sub(4) / 2;
                let units = std::slice::from_raw_parts(path_ptr, max);
                let len = units.iter().position(|c| *c == 0).unwrap_or(max);
                let path = String::from_utf16_lossy(&units[..len]);
                if path.is_empty() || !path.to_lowercase().contains("vid_1532") {
                    continue;
                }
                let physical = physical_device_id(&path, dev.DevInst);
                if handled.contains(&physical) {
                    continue;
                }
                let wide: Vec<u16> = units[..len].iter().copied().chain([0]).collect();
                let Some(info) = open(&wide, 0, FILE_FLAGS_AND_ATTRIBUTES(0)) else {
                    continue;
                };
                let mut attrs = HIDD_ATTRIBUTES {
                    Size: size_of::<HIDD_ATTRIBUTES>() as u32,
                    ..Default::default()
                };
                if !HidD_GetAttributes(info.0, &mut attrs) || attrs.VendorID != VENDOR_ID {
                    continue;
                }
                let pid = attrs.ProductID;
                if device_info(pid).is_none() {
                    continue;
                }
                let Some(caps) = hid_caps(info.0) else {
                    continue;
                };
                let device = if PA_PIDS.contains(&pid) {
                    if matches!(caps.UsagePage, 0xFF00 | 0xFF14)
                        && usize::from(caps.InputReportByteLength) == PA_REPORT_LENGTH
                        && usize::from(caps.OutputReportByteLength) == PA_REPORT_LENGTH
                    {
                        let running = *synapse_running.get_or_insert_with(synapse_is_running);
                        // the Barracuda's vendor collection is shareable; Synapse can own the BlackShark's exclusively
                        if pid == BARRACUDA_PID || !running {
                            read_pa_device(&wide, &info, pid, deadline)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else if usize::from(caps.FeatureReportByteLength) == REPORT_LENGTH {
                    read_feature_device(&info, pid, deadline)
                } else {
                    None
                };
                if let Some(d) = device {
                    handled.insert(physical);
                    readings.push(d);
                }
            }
            let _ = SetupDiDestroyDeviceInfoList(set);
        }
        (merge_into_bluetooth(&[], &readings), None)
    }

    fn synapse_is_running() -> bool {
        SYNAPSE
            .iter()
            .any(|n| !crate::process_monitor::processes_named(n).is_empty())
    }

    /// The parent USB device node, so several interfaces of one receiver count once while two identical receivers on
    /// different ports stay apart.
    fn physical_device_id(path: &str, dev_inst: u32) -> String {
        let mut current = dev_inst;
        for _ in 0..3 {
            let mut parent = 0u32;
            // SAFETY: plain cfgmgr32 queries with out pointers and a sized buffer
            unsafe {
                if CM_Get_Parent(&mut parent, current, 0) != CR_SUCCESS {
                    break;
                }
                current = parent;
                let mut buf = [0u16; 256];
                if CM_Get_Device_IDW(current, &mut buf, 0) == CR_SUCCESS {
                    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
                    let id = String::from_utf16_lossy(&buf[..end]);
                    let upper = id.to_uppercase();
                    if upper.starts_with("USB\\") && !upper.contains("&MI_") {
                        return id;
                    }
                }
            }
        }
        if dev_inst != 0 {
            format!("devinst:{dev_inst}")
        } else {
            path.to_string()
        }
    }

    fn hid_caps(handle: HANDLE) -> Option<HIDP_CAPS> {
        let mut pp = PHIDP_PREPARSED_DATA::default();
        // SAFETY: the preparsed data is freed once after use
        unsafe {
            if !HidD_GetPreparsedData(handle, &mut pp) {
                return None;
            }
            let mut caps = HIDP_CAPS::default();
            let ok = HidP_GetCaps(pp, &mut caps).0 >= 0;
            let _ = HidD_FreePreparsedData(pp);
            ok.then_some(caps)
        }
    }

    fn name_and_kind(handle: HANDLE, pid: u16) -> (String, Option<&'static str>) {
        let known = device_info(pid);
        let mut buf = [0u16; 256];
        // SAFETY: the buffer's length in bytes is passed
        let got = unsafe {
            HidD_GetProductString(handle, buf.as_mut_ptr().cast(), (buf.len() * 2) as u32)
        };
        let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        let name = if got && end > 0 {
            String::from_utf16_lossy(&buf[..end]).trim().to_string()
        } else {
            known.map_or_else(|| format!("Razer Device ({pid:04X})"), |k| k.0.to_string())
        };
        (name, known.map(|k| k.1))
    }

    fn read_feature_device(handle: &Handle, pid: u16, deadline: i64) -> Option<DeviceBattery> {
        let preferred = PREFERRED_TX
            .lock()
            .ok()
            .and_then(|m| m.as_ref().and_then(|m| m.get(&pid).copied()));
        let mut send = |req: &[u8]| -> Option<Vec<u8>> {
            let mut resp = vec![0u8; REPORT_LENGTH];
            // SAFETY: both buffers are passed with their lengths
            unsafe {
                if !HidD_SetFeature(handle.0, req.as_ptr().cast(), req.len() as u32) {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
                if !HidD_GetFeature(handle.0, resp.as_mut_ptr().cast(), resp.len() as u32) {
                    return None;
                }
            }
            if resp[1] == STATUS_BUSY {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Some(resp)
        };
        let clock = now;
        let (level, charging, tx) =
            query_feature_device(pid, preferred, &mut send, Some(deadline), Some(&clock));
        if let Some(tx) = tx
            && let Ok(mut m) = PREFERRED_TX.lock()
        {
            m.get_or_insert_with(HashMap::new).insert(pid, tx);
        }
        let level = level?;
        let (name, kind) = name_and_kind(handle.0, pid);
        let kind = kind.map_or_else(|| crate::battery_monitor::kind_of(&name), String::from);
        Some(DeviceBattery {
            name,
            level,
            kind,
            charging,
        })
    }

    fn read_pa_device(
        path: &[u16],
        info: &Handle,
        pid: u16,
        deadline: i64,
    ) -> Option<DeviceBattery> {
        if now() >= deadline {
            return None;
        }
        // GENERIC_READ | GENERIC_WRITE, overlapped
        let rw = open(path, 0xC000_0000, FILE_FLAG_OVERLAPPED)?;
        for _ in 0..16 {
            if now() >= deadline {
                break;
            }
            match io(false, &rw, PA_REPORT_LENGTH, None, 10) {
                Io::Fatal | Io::Timeout => break,
                Io::Done(_) => {}
            }
        }
        let barracuda = pid == BARRACUDA_PID;
        let remote = |on: bool| {
            if barracuda {
                build_barracuda_remote_frame(on)
            } else {
                build_pa_remote_frame(on)
            }
        };
        if now() >= deadline {
            return None;
        }
        let mut woke = false;
        for attempt in 0..(if barracuda { 3 } else { 1 }) {
            if now() >= deadline {
                break;
            }
            if write(&rw, &remote(true), 200) {
                woke = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150 * (attempt + 1)));
        }
        if !woke {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(35));
        let result = (|| {
            let battery =
                query_pa(&rw, PA_CMD_BATTERY, deadline, barracuda).filter(|b| *b <= 100)?;
            let charging = query_pa(&rw, PA_CMD_CHARGING, deadline, barracuda).map(|c| c != 0);
            let (name, kind) = name_and_kind(info.0, pid);
            let kind = kind.map_or_else(|| crate::battery_monitor::kind_of(&name), String::from);
            Some(DeviceBattery {
                name,
                level: i32::from(battery),
                kind,
                charging,
            })
        })();
        // remote mode off again, as our "on" write succeeded
        write(&rw, &remote(false), 150);
        result
    }

    fn query_pa(handle: &Handle, cmd: u8, deadline: i64, barracuda: bool) -> Option<u8> {
        let query = if barracuda {
            build_barracuda_query_frame(cmd)
        } else {
            build_pa_query_frame(cmd)
        };
        for _ in 0..(if barracuda { 4 } else { 2 }) {
            if now() >= deadline || !write(handle, &query, 200) {
                break;
            }
            let reply_deadline = deadline.min(now() + 180);
            while now() < reply_deadline {
                match io(false, handle, PA_REPORT_LENGTH, None, 50) {
                    Io::Fatal => return None,
                    Io::Timeout => {}
                    Io::Done(reply) => {
                        let value = if barracuda {
                            parse_barracuda_reply(&reply, cmd)
                        } else {
                            parse_pa_reply(&reply, cmd)
                        };
                        if value.is_some() {
                            return value;
                        }
                    }
                }
            }
        }
        None
    }

    fn write(handle: &Handle, data: &[u8], timeout_ms: u32) -> bool {
        matches!(io(true, handle, data.len(), Some(data), timeout_ms), Io::Done(n) if n.len() == data.len())
    }

    enum Io {
        /// The bytes read, or (for a write) a buffer as long as what was written.
        Done(Vec<u8>),
        Timeout,
        Fatal,
    }

    /// One overlapped read or write with a timeout. When the driver does not finish a cancelled operation in time,
    /// its buffer and OVERLAPPED are leaked on purpose: the driver may still write to them.
    fn io(is_write: bool, handle: &Handle, len: usize, data: Option<&[u8]>, timeout_ms: u32) -> Io {
        // SAFETY: the event, buffer and OVERLAPPED stay alive until the operation is known to be complete (or are
        // leaked when it may still complete); GetOverlappedResult only runs on a completed operation
        unsafe {
            let Ok(event) = CreateEventW(None, true, false, PCWSTR::null()) else {
                return Io::Fatal;
            };
            let mut buf: Box<[u8]> = match data {
                Some(d) => d.into(),
                None => vec![0u8; len].into_boxed_slice(),
            };
            let mut ov = Box::new(OVERLAPPED {
                hEvent: event,
                ..Default::default()
            });
            let started = if is_write {
                WriteFile(handle.0, Some(&buf), None, Some(&mut *ov))
            } else {
                ReadFile(handle.0, Some(&mut buf), None, Some(&mut *ov))
            };
            if started.is_err() {
                if GetLastError() != ERROR_IO_PENDING {
                    let _ = CloseHandle(event);
                    return Io::Fatal; // an immediate error (e.g. unplugged)
                }
                if WaitForSingleObject(event, timeout_ms) != WAIT_OBJECT_0 {
                    let _ = CancelIoEx(handle.0, Some(&*ov));
                    if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                        // the driver did not finish the cancellation: keep its memory alive forever
                        std::mem::forget(buf);
                        std::mem::forget(ov);
                        return Io::Fatal;
                    }
                    // it may have completed just before the cancellation took effect
                    let mut late = 0u32;
                    let done = GetOverlappedResult(handle.0, &*ov, &mut late, false).is_ok();
                    let _ = CloseHandle(event);
                    return if done {
                        Io::Done(buf[..late as usize].to_vec())
                    } else {
                        Io::Timeout
                    };
                }
            }
            let mut transferred = 0u32;
            let done = GetOverlappedResult(handle.0, &*ov, &mut transferred, false).is_ok();
            let _ = CloseHandle(event);
            if !done {
                return Io::Fatal;
            }
            if transferred == 0 {
                return Io::Timeout;
            }
            Io::Done(buf[..(transferred as usize).min(buf.len())].to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
    fn frame(head: &str, tail: &str) -> Vec<u8> {
        hex(&format!("{head}{}{tail}", "0".repeat(78 * 2)))
    }

    #[test]
    fn build_razer_report_constructs_valid_battery_and_charging_requests() {
        let battery = build_report(0x1F, CMD_BATTERY);
        assert_eq!(battery.len(), REPORT_LENGTH);
        assert_eq!(
            (
                battery[0], battery[1], battery[2], battery[6], battery[7], battery[8]
            ),
            (0, STATUS_NEW, 0x1F, 0x02, CLASS_POWER, CMD_BATTERY)
        );
        // CRC is XOR of Razer frame bytes [2..87] (HID buffer indices [3..88]) = 0x02 ^ 0x07 ^ 0x80 = 0x85
        assert_eq!(battery[89], 0x85);
        let charging = build_report(0x3F, CMD_CHARGING);
        assert_eq!(charging.len(), REPORT_LENGTH);
        assert_eq!(
            (charging[2], charging[6], charging[7], charging[8]),
            (0x3F, 0x02, CLASS_POWER, CMD_CHARGING)
        );
        assert_eq!(charging[89], 0x81);
    }

    #[test]
    fn parse_razer_battery_and_charging_from_captured_device_responses() {
        // real 91-byte HidD_GetFeature responses captured from a Razer Basilisk V3 Pro (PID 0x00AB, wireless)
        assert_eq!(
            parse_battery(&frame("00021F0000000207800039", "BC00")),
            Some(22)
        );
        assert_eq!(
            parse_charging(&frame("00021F0000000207840000", "8100")),
            Some(false)
        );
        assert_eq!(
            parse_battery(&frame("00021F0000000207800078", "FD00")),
            Some(47)
        );
        assert_eq!(
            parse_battery(&frame("00023F00000002078000FF", "7A00")),
            Some(100)
        );
        assert_eq!(
            parse_charging(&frame("00023F0000000207840001", "8000")),
            Some(true)
        );
        // sleeping/off (raw = 0), busy (status 0x01), a foreign command class (0x0F), a short buffer
        assert_eq!(
            parse_battery(&frame("00021F0000000207800000", "8500")),
            None
        );
        assert_eq!(
            parse_battery(&frame("00011F0000000207800078", "FD00")),
            None
        );
        let foreign = frame("00021F0000002C0F030000", "0000");
        assert_eq!(parse_battery(&foreign), None);
        assert_eq!(parse_charging(&foreign), None);
        assert_eq!(parse_battery(&[0; 16]), None);
        assert_eq!(parse_charging(&[0; 16]), None);
    }

    #[test]
    fn razer_pa_protocol_builds_frames_and_parses_captured_headset_responses() {
        let on = build_pa_remote_frame(true);
        assert_eq!(on.len(), PA_REPORT_LENGTH);
        assert_eq!(
            (on[0], on[5], on[6], on[10], on[11]),
            (PA_REPORT_ID, b'P', b'A', 0xE1, 1)
        );
        assert_eq!(build_pa_remote_frame(false)[11], 0);
        let query = build_pa_query_frame(PA_CMD_BATTERY);
        assert_eq!(query.len(), PA_REPORT_LENGTH);
        assert_eq!((query[0], query[10]), (PA_REPORT_ID, PA_CMD_BATTERY));
        // real 64-byte PA replies captured from a Razer BlackShark V2 Pro 2.4 (PID 0x0555, wireless)
        let tail = format!(
            "504908C683FB4C0004002A0201000002060B0002060B{}",
            "0".repeat(26 * 2)
        );
        let battery = hex(&format!("020E504908C8ED6B510004002101011D{tail}"));
        let charging = hex(&format!("020E504908C9126C510004002A010100{tail}"));
        assert_eq!(parse_pa_reply(&battery, PA_CMD_BATTERY), Some(29));
        assert_eq!(parse_pa_reply(&battery, PA_CMD_CHARGING), None);
        assert_eq!(parse_pa_reply(&charging, PA_CMD_CHARGING), Some(0));
        assert_eq!(parse_pa_reply(&[0; 10], PA_CMD_BATTERY), None);
    }

    #[test]
    fn barracuda_pa_protocol_uses_its_own_offsets_and_parses_replies_with_or_without_report_id() {
        let remote = build_barracuda_remote_frame(true);
        assert_eq!(
            (remote[0], remote[3], remote[4], remote[8], remote[9]),
            (BARRACUDA_REPORT_ID, b'P', b'A', 0xE1, 1)
        );
        assert_eq!(
            build_barracuda_query_frame(PA_CMD_BATTERY)[8],
            PA_CMD_BATTERY
        );
        let mut reply = vec![0u8; PA_REPORT_LENGTH];
        reply[0] = BARRACUDA_REPORT_ID;
        reply[3] = b'P';
        reply[4] = b'I';
        reply[13] = PA_CMD_BATTERY;
        reply[14] = 0x01;
        reply[15] = 0x01;
        reply[16] = 73;
        assert_eq!(parse_barracuda_reply(&reply, PA_CMD_BATTERY), Some(73));
        assert_eq!(parse_barracuda_reply(&reply[1..], PA_CMD_BATTERY), Some(73));
        assert_eq!(parse_barracuda_reply(&reply, PA_CMD_CHARGING), None);
    }

    #[test]
    fn razer_kind_from_pid_maps_known_devices_and_falls_back_to_name() {
        for (pid, kind) in [
            (0x00AB, Some("mouse")),
            (0x00C1, Some("mouse")),
            (0x0555, Some("headphones")),
            (0x053A, Some("headphones")),
            (0x025C, Some("keyboard")),
            (0xFFFF, None),
        ] {
            assert_eq!(kind_from_pid(pid), kind, "{pid:04X}");
        }
        assert_eq!(
            device_info(0x00AB),
            Some(("Razer Basilisk V3 Pro", "mouse"))
        );
    }

    #[test]
    fn merge_razer_into_bluetooth_preserves_bluetooth_duplicates_and_collapses_cable_with_dongle() {
        let bluetooth = [
            DeviceBattery::new("Office Mouse", 60, "mouse", None),
            DeviceBattery::new("office mouse", 40, "mouse", None),
            DeviceBattery::new("Razer Basilisk V3 Pro", 45, "mouse", None),
        ];
        let hid = [
            DeviceBattery::new("Razer Basilisk V3 Pro", 46, "mouse", Some(false)), // wireless dongle
            DeviceBattery::new("Razer Basilisk V3 Pro", 47, "mouse", Some(true)),  // USB cable
            DeviceBattery::new("Razer BlackWidow V3 Pro", 80, "keyboard", Some(false)),
        ];
        let merged = merge_into_bluetooth(&bluetooth, &hid);
        assert_eq!(merged.len(), 4);
        assert_eq!(
            merged
                .iter()
                .filter(|d| d.name.eq_ignore_ascii_case("Office Mouse"))
                .count(),
            2
        );
        let mouse: Vec<&DeviceBattery> = merged
            .iter()
            .filter(|d| d.name == "Razer Basilisk V3 Pro")
            .collect();
        assert_eq!(mouse.len(), 1);
        assert_eq!((mouse[0].charging, mouse[0].level), (Some(true), 47));
    }

    #[test]
    fn query_razer_feature_device_every_sent_frame_has_read_bit_set() {
        let mut sent: Vec<Vec<u8>> = vec![];
        for pid in feature_pids() {
            // a device that answers NotSupported for every transaction id
            query_feature_device(
                pid,
                None,
                &mut |req| {
                    sent.push(req.to_vec());
                    let mut resp = req.to_vec();
                    resp[1] = STATUS_NOT_SUPPORTED;
                    Some(resp)
                },
                None,
                None,
            );
            // a device that answers OK to the battery and charging queries
            query_feature_device(
                pid,
                None,
                &mut |req| {
                    sent.push(req.to_vec());
                    let mut resp = req.to_vec();
                    resp[1] = STATUS_OK;
                    resp[10] = if req[8] == CMD_BATTERY { 0x80 } else { 0x01 };
                    Some(resp)
                },
                None,
                None,
            );
        }
        assert!(!sent.is_empty());
        for f in &sent {
            assert_eq!(f[7], CLASS_POWER);
            assert!(
                f[8] & 0x80 != 0,
                "sent command 0x{:02X} must have the read bit (bit 7) set",
                f[8]
            );
        }
    }

    #[test]
    fn query_razer_feature_device_never_sends_to_unknown_or_headset_only_pids() {
        let mut sent = 0;
        let mut spy = |_: &[u8]| -> Option<Vec<u8>> {
            sent += 1;
            None
        };
        assert_eq!(
            query_feature_device(0x9999, None, &mut spy, None, None),
            (None, None, None)
        );
        assert_eq!(
            query_feature_device(0x0555, None, &mut spy, None, None),
            (None, None, None)
        );
        assert_eq!(sent, 0);
    }

    #[test]
    fn query_razer_feature_device_stops_on_ok_and_uses_preferred_tx_first() {
        let mut sent_tx = vec![];
        let r = query_feature_device(
            0x00AB,
            None,
            &mut |req| {
                sent_tx.push(req[2]);
                let mut resp = req.to_vec();
                resp[1] = STATUS_OK;
                resp[10] = 0x00; // sleeping
                Some(resp)
            },
            None,
            None,
        );
        assert_eq!(r, (None, None, Some(0x1F)));
        assert_eq!(sent_tx, [0x1F]);

        sent_tx.clear();
        let r = query_feature_device(
            0x00AB,
            Some(0x3F),
            &mut |req| {
                sent_tx.push(req[2]);
                let mut resp = req.to_vec();
                resp[1] = STATUS_OK;
                resp[10] = if req[8] == CMD_BATTERY { 0x78 } else { 0x01 };
                Some(resp)
            },
            None,
            None,
        );
        assert_eq!(r, (Some(47), Some(true), Some(0x3F)));
        assert_eq!(sent_tx, [0x3F, 0x3F]);
    }

    #[test]
    fn query_feature_device_respects_the_deadline() {
        let mut sent = 0;
        let r = query_feature_device(
            0x00AB,
            None,
            &mut |_| {
                sent += 1;
                None
            },
            Some(100),
            Some(&|| 100),
        );
        assert_eq!((r, sent), ((None, None, None), 0));
    }
}
