//! Port of `BatteryMonitor.cs`: battery levels of wireless devices (headphones, mice, keyboards, controllers, and a
//! laptop's own battery on Linux). Windows: the Bluetooth battery property (SetupAPI) and Razer USB HID devices;
//! Linux: UPower; macOS: ioreg and system_profiler. Read every 20 s (a minute on macOS), only while a phone shows the
//! widget.

pub mod razer;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

/// `kind`: headphones, mouse, keyboard, controller, pen, laptop or other (for the icon).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceBattery {
    pub name: String,
    pub level: i32,
    pub kind: String,
    pub charging: Option<bool>,
}

impl DeviceBattery {
    pub fn new(name: &str, level: i32, kind: &str, charging: Option<bool>) -> DeviceBattery {
        DeviceBattery {
            name: name.into(),
            level,
            kind: kind.into(),
            charging,
        }
    }
}

pub const ACTIVE_REFRESH_INTERVAL_MS: u64 = 20_000;
/// macOS reads headphones with system_profiler, which takes seconds of CPU: keep it at once a minute.
pub const MAC_REFRESH_INTERVAL_MS: u64 = 60_000;
/// Reads stop two minutes after the last request.
const WANTED_MS: i64 = 120_000;

pub fn refresh_interval_ms(mac_os: bool) -> u64 {
    if mac_os {
        MAC_REFRESH_INTERVAL_MS
    } else {
        ACTIVE_REFRESH_INTERVAL_MS
    }
}

/// Guesses the kind of device from its name.
pub fn kind_of(name: &str) -> String {
    use std::sync::OnceLock;
    static KINDS: OnceLock<Vec<(regex::Regex, &str)>> = OnceLock::new();
    let kinds = KINDS.get_or_init(|| {
        [
            (
                r"buds|airpods|headphone|headset|earbud|earphone|\bwh-|\bwf-|jabra|bose|beats|sennheiser|soundcore|\bqc\d|xm\d|arctis|hyperx cloud|blackshark|barracuda|kraken|nari|kaira|opus",
                "headphones",
            ),
            (
                r"mouse|mx master|mx anywhere|pebble|trackball|trackpad|\bm\d{3}\b|g\d{3} |basilisk|viper|deathadder|naga|orochi|cobra|lancehead|mamba",
                "mouse",
            ),
            (
                r"keyboard|keys|\bk\d{3}\b|slim|craft|magic keyboard|blackwidow|huntsman|deathstalker|ornata|tartarus",
                "keyboard",
            ),
            (r"controller|gamepad|xbox|dualsense|dualshock|joy-con|8bitdo", "controller"),
            (r"pencil|stylus|pen\b", "pen"),
        ]
        .into_iter()
        .map(|(re, kind)| (regex::Regex::new(re).expect("kind regex"), kind))
        .collect()
    });
    let n = name.to_lowercase();
    kinds
        .iter()
        .find(|(re, _)| re.is_match(&n))
        .map_or("other", |(_, k)| k)
        .to_string()
}

/// A Bluetooth device whose "connected" property says 0 is paired but away.
pub fn include_windows_battery_device(has_connected_property: bool, value: &[u8]) -> bool {
    !has_connected_property || value.first().is_none_or(|v| *v != 0)
}

/// `int.TryParse`: surrounding whitespace and a sign allowed.
fn parse_int(text: &str) -> Option<i32> {
    let t = text.trim();
    t.strip_prefix('+').unwrap_or(t).parse().ok()
}

/// AirPods and other headphones from `system_profiler SPBluetoothDataType -json`: the lowest of the left, right and
/// main levels of each connected device that has one; a name already listed (by ioreg) is kept as it is.
pub fn add_mac_bluetooth_batteries(root: &Value, list: &mut Vec<DeviceBattery>) {
    let Some(sections) = root.get("SPBluetoothDataType").and_then(Value::as_array) else {
        return;
    };
    for section in sections {
        let Some(connected) = section.get("device_connected").and_then(Value::as_array) else {
            continue;
        };
        for device in connected.iter().filter_map(Value::as_object) {
            for (name, props) in device {
                if !props.is_object() {
                    continue;
                }
                let level = |key: &str| {
                    props
                        .get(key)
                        .and_then(Value::as_str)
                        .and_then(|v| parse_int(v.trim_end_matches('%')))
                };
                let lowest = [
                    "device_batteryLevelLeft",
                    "device_batteryLevelRight",
                    "device_batteryLevelMain",
                ]
                .into_iter()
                .filter_map(level)
                .min();
                let Some(lowest) = lowest else { continue };
                if list.iter().any(|d| d.name == *name) {
                    continue;
                }
                let kind = kind_of(name);
                let kind = if kind == "other" {
                    "headphones".to_string()
                } else {
                    kind
                };
                list.push(DeviceBattery {
                    name: name.clone(),
                    level: lowest,
                    kind,
                    charging: None,
                });
            }
        }
    }
}

/// One `upower -i <path>` device, or None for the summary device, mains power, a missing battery or no percentage.
#[cfg(any(not(windows), test))]
fn parse_upower_device(path: &str, info: &str) -> Option<DeviceBattery> {
    if path.ends_with("DisplayDevice") || path.contains("line_power") {
        return None;
    }
    let field = |key: &str| -> Option<&str> {
        info.split('\n')
            .map(str::trim)
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .map(str::trim)
    };
    let percentage = field("percentage")?.trim_end_matches('%');
    let level = parse_int(percentage.split([',', '.']).next().unwrap_or_default())?;
    if field("present") == Some("no") {
        return None;
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    let kind = if file.starts_with("battery_") {
        Some("laptop")
    } else if file.starts_with("mouse_") {
        Some("mouse")
    } else if file.starts_with("keyboard_") {
        Some("keyboard")
    } else if file.starts_with("headset_") || file.starts_with("headphones_") {
        Some("headphones")
    } else if file.starts_with("gaming_input_") {
        Some("controller")
    } else {
        None
    };
    let name = match field("model") {
        _ if kind == Some("laptop") => "This laptop".to_string(),
        Some(m) if !m.is_empty() => m.to_string(),
        _ => file.to_string(),
    };
    let charging = field("state").map(|s| s == "charging" || s == "fully-charged");
    Some(DeviceBattery {
        level: level.clamp(0, 100),
        kind: kind.map_or_else(|| kind_of(&name), String::from),
        name,
        charging,
    })
}

/// `ioreg -r -l -k BatteryPercent`: each "BatteryPercent" goes with the "Product" seen before it.
#[cfg(any(not(windows), test))]
fn parse_ioreg(text: &str) -> Vec<DeviceBattery> {
    use std::sync::OnceLock;
    static PRODUCT: OnceLock<regex::Regex> = OnceLock::new();
    static PERCENT: OnceLock<regex::Regex> = OnceLock::new();
    let product_re =
        PRODUCT.get_or_init(|| regex::Regex::new(r#""Product" = "(.+)""#).expect("regex"));
    let percent_re =
        PERCENT.get_or_init(|| regex::Regex::new(r#""BatteryPercent" = (\d+)"#).expect("regex"));
    let mut list = vec![];
    let mut product: Option<String> = None;
    for raw in text.split('\n') {
        let line = raw.trim();
        if let Some(c) = product_re.captures(line) {
            product = Some(c[1].to_string());
        }
        // the product is used up even by a level that is not a number (C# throws there)
        if let Some(c) = percent_re.captures(line)
            && let Some(name) = product.take()
            && let Ok(level) = c[1].parse::<i32>()
        {
            let kind = kind_of(&name);
            list.push(DeviceBattery {
                name,
                level,
                kind,
                charging: None,
            });
        }
    }
    list
}

/// By name, the laptop's own battery last (the C# agent compares ordinally: it runs in invariant globalization mode).
fn sort_devices(mut list: Vec<DeviceBattery>) -> Vec<DeviceBattery> {
    list.sort_by(|a, b| {
        (a.kind == "laptop")
            .cmp(&(b.kind == "laptop"))
            .then_with(|| a.name.cmp(&b.name))
    });
    list
}

#[cfg(not(windows))]
fn read_devices() -> Vec<DeviceBattery> {
    use crate::unix::sh;
    let mut list = vec![];
    if cfg!(target_os = "macos") {
        list = parse_ioreg(
            &sh::run("ioreg", &["-r", "-l", "-k", "BatteryPercent"]).unwrap_or_default(),
        );
        if let Some(json) =
            sh::run_with_timeout(8000, "system_profiler", &["SPBluetoothDataType", "-json"])
            && let Ok(doc) = serde_json::from_str::<Value>(&json)
        {
            add_mac_bluetooth_batteries(&doc, &mut list);
        }
    } else {
        let paths = sh::run("upower", &["-e"]).unwrap_or_default();
        for path in paths.split('\n').map(str::trim).filter(|p| !p.is_empty()) {
            if path.ends_with("DisplayDevice") || path.contains("line_power") {
                continue;
            }
            let info = sh::run("upower", &["-i", path]).unwrap_or_default();
            list.extend(parse_upower_device(path, &info));
        }
    }
    sort_devices(list)
}

pub struct BatteryMonitor {
    devices: Mutex<Vec<DeviceBattery>>,
    /// Milliseconds since `epoch` of the last request, or `NO_REQUEST`.
    last_request: AtomicI64,
    epoch: Instant,
    #[cfg(windows)]
    lists: Mutex<(Vec<DeviceBattery>, Vec<DeviceBattery>)>,
    #[cfg(windows)]
    razer_running: std::sync::atomic::AtomicBool,
}

const NO_REQUEST: i64 = i64::MIN;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl BatteryMonitor {
    pub fn new() -> Arc<BatteryMonitor> {
        Arc::new(BatteryMonitor {
            devices: Mutex::default(),
            last_request: AtomicI64::new(NO_REQUEST),
            epoch: Instant::now(),
            #[cfg(windows)]
            lists: Mutex::default(),
            #[cfg(windows)]
            razer_running: Default::default(),
        })
    }

    fn now_ms(&self) -> i64 {
        self.epoch.elapsed().as_millis() as i64
    }

    /// The devices of the last read; asking keeps the reads going for two minutes.
    pub fn snapshot(&self) -> Vec<DeviceBattery> {
        self.last_request.store(self.now_ms(), Ordering::Relaxed);
        lock(&self.devices).clone()
    }

    /// Whether a phone asked in the last two minutes.
    pub fn is_wanted(&self) -> bool {
        self.wanted()
    }

    fn wanted(&self) -> bool {
        let last = self.last_request.load(Ordering::Relaxed);
        last != NO_REQUEST && self.now_ms() - last < WANTED_MS
    }

    /// Checks every second; reads when a phone asked in the last two minutes and the last read is older than the
    /// refresh interval.
    pub fn start(self: &Arc<Self>, stop: crate::server::Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let interval = refresh_interval_ms(cfg!(target_os = "macos")) as i64;
            let mut read_at: Option<i64> = None;
            loop {
                let now = me.now_ms();
                if me.wanted() && read_at.is_none_or(|at| now - at > interval) {
                    read_at = Some(now);
                    me.read().await;
                }
                tokio::select! {
                    _ = stop.stopped() => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        })
    }

    #[cfg(not(windows))]
    async fn read(self: &Arc<Self>) {
        if let Ok(list) = tokio::task::spawn_blocking(read_devices).await {
            *lock(&self.devices) = list;
        }
    }

    /// Windows: the Bluetooth list now; the Razer scan (up to 1.5 s) on its own task, never two at once.
    #[cfg(windows)]
    async fn read(self: &Arc<Self>) {
        if let Ok(bt) = tokio::task::spawn_blocking(windows_bluetooth::read).await {
            let mut lists = lock(&self.lists);
            lists.0 = bt;
            *lock(&self.devices) = sort_devices(razer::merge_into_bluetooth(&lists.0, &lists.1));
        }
        if !self.razer_running.swap(true, Ordering::AcqRel) {
            let me = self.clone();
            tokio::task::spawn_blocking(move || {
                let (found, error) = razer::read_windows();
                if let Some(e) = error {
                    crate::log::debug("BatteryMonitor", &format!("batteries (razer): {e}"));
                }
                let mut lists = lock(&me.lists);
                lists.1 = found;
                *lock(&me.devices) = sort_devices(razer::merge_into_bluetooth(&lists.0, &lists.1));
                me.razer_running.store(false, Ordering::Release);
            });
        }
    }
}

#[cfg(windows)]
mod windows_bluetooth {
    use super::{DeviceBattery, include_windows_battery_device, kind_of};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        DIGCF_ALLCLASSES, DIGCF_PRESENT, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList,
        SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW,
        SetupDiGetDevicePropertyW,
    };
    use windows::Win32::Devices::Properties::DEVPROPTYPE;
    use windows::Win32::Foundation::DEVPROPKEY;
    use windows::core::{GUID, PCWSTR};

    const fn key(fmtid: u128, pid: u32) -> DEVPROPKEY {
        DEVPROPKEY {
            fmtid: GUID::from_u128(fmtid),
            pid,
        }
    }
    const BATTERY: DEVPROPKEY = key(0x104EA319_6EE2_4701_BD47_8DDBF425BBE5, 2);
    const FRIENDLY: DEVPROPKEY = key(0xa45c254e_df1c_4efd_8020_67d146a850e0, 14);
    const CONNECTED: DEVPROPKEY = key(0x83DA6326_97A6_4088_9453_A1923F573B29, 15);

    /// The Bluetooth battery property of present devices (what Settings › Bluetooth shows).
    pub(super) fn read() -> Vec<DeviceBattery> {
        let mut found: Vec<(String, i32, String)> = vec![];
        // SAFETY: the device set is destroyed once at the end; every buffer is passed with its length
        unsafe {
            let Ok(set) =
                SetupDiGetClassDevsW(None, PCWSTR::null(), None, DIGCF_PRESENT | DIGCF_ALLCLASSES)
            else {
                return vec![];
            };
            let mut data = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            let mut buf = [0u8; 1024];
            let mut id_buf = [0u16; 512];
            let mut i = 0;
            while SetupDiEnumDeviceInfo(set, i, &mut data).is_ok() {
                i += 1;
                if SetupDiGetDeviceInstanceIdW(set, &data, Some(&mut id_buf), None).is_err() {
                    continue;
                }
                let end = id_buf.iter().position(|c| *c == 0).unwrap_or(id_buf.len());
                let id = String::from_utf16_lossy(&id_buf[..end]);
                if !id.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("BTH")) {
                    continue;
                }
                let mut kind = DEVPROPTYPE::default();
                let mut size = 0u32;
                let has_connected = SetupDiGetDevicePropertyW(
                    set,
                    &data,
                    &CONNECTED,
                    &mut kind,
                    Some(&mut buf),
                    Some(&mut size),
                    0,
                )
                .is_ok();
                let n = (size as usize).min(buf.len());
                if !include_windows_battery_device(has_connected, &buf[..n]) {
                    continue;
                }
                if SetupDiGetDevicePropertyW(
                    set,
                    &data,
                    &BATTERY,
                    &mut kind,
                    Some(&mut buf),
                    Some(&mut size),
                    0,
                )
                .is_err()
                    || size < 1
                {
                    continue;
                }
                let level = i32::from(buf[0]);
                if level > 100 {
                    continue;
                }
                let name = if SetupDiGetDevicePropertyW(
                    set,
                    &data,
                    &FRIENDLY,
                    &mut kind,
                    Some(&mut buf),
                    Some(&mut size),
                    0,
                )
                .is_ok()
                {
                    let n = (size as usize).min(buf.len());
                    let units: Vec<u16> = buf[..n]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c))
                        .collect();
                    String::from_utf16_lossy(&units)
                        .trim_end_matches('\0')
                        .to_string()
                } else {
                    "Bluetooth device".to_string()
                };
                found.push((name, level, id));
            }
            let _ = SetupDiDestroyDeviceInfoList(set);
        }
        // the same model paired twice (e.g. a mouse on two Easy-Switch channels): tell them apart by the address
        let address = regex::Regex::new(r"(?i)DEV_([0-9A-F]{12})").expect("regex");
        found
            .iter()
            .map(|(name, level, id)| {
                let dupes = found
                    .iter()
                    .filter(|f| f.0.to_lowercase() == name.to_lowercase())
                    .count()
                    > 1;
                let shown = match address.captures(id) {
                    Some(c) if dupes => format!("{name} ({})", &c[1][8..]),
                    _ => name.clone(),
                };
                DeviceBattery {
                    name: shown,
                    level: *level,
                    kind: kind_of(name),
                    charging: None,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_battery_refresh_balances_responsiveness_and_receiver_wakeups() {
        assert!((15_000..=20_000).contains(&ACTIVE_REFRESH_INTERVAL_MS));
    }

    #[test]
    fn mac_batteries_stay_at_once_a_minute_because_system_profiler_is_slow() {
        assert_eq!(refresh_interval_ms(true), 60_000);
        assert_eq!(refresh_interval_ms(false), ACTIVE_REFRESH_INTERVAL_MS);
        assert_eq!(MAC_REFRESH_INTERVAL_MS, 60_000);
    }

    #[test]
    fn disconnected_windows_devices_are_excluded() {
        assert!(include_windows_battery_device(false, &[0]));
        assert!(!include_windows_battery_device(true, &[0]));
        assert!(include_windows_battery_device(true, &[1]));
        assert!(include_windows_battery_device(true, &[]));
    }

    #[test]
    fn kind_from_name() {
        for (name, kind) in [
            ("WH-1000XM5", "headphones"),
            ("Galaxy Buds2 Pro", "headphones"),
            ("AirPods Pro", "headphones"),
            ("Razer BlackShark V2 Pro 2.4", "headphones"),
            ("Razer Barracuda X", "headphones"),
            ("MX Master 3S", "mouse"),
            ("Logitech Pebble M350", "mouse"),
            ("Razer Basilisk V3 Pro", "mouse"),
            ("Razer Viper V3 Pro", "mouse"),
            ("Razer Lancehead Wireless", "mouse"),
            ("MX Keys", "keyboard"),
            ("Slim Solar+", "keyboard"),
            ("Razer BlackWidow V4 Pro", "keyboard"),
            ("Xbox Wireless Controller", "controller"),
            ("DualSense Wireless Controller", "controller"),
            ("Apple Pencil", "pen"),
            ("Some Speaker", "other"),
        ] {
            assert_eq!(kind_of(name), kind, "{name}");
        }
    }

    #[test]
    fn mac_bluetooth_keeps_valid_levels_and_devices_around_malformed_entries() {
        let doc: Value = serde_json::from_str(
            r#"{
              "SPBluetoothDataType": [
                {"device_connected": [
                  {"AirPods A":{"device_batteryLevelMain":"72%"}},
                  {"Unknown":{"device_batteryLevelMain":{}}},
                  {"AirPods B":{"device_batteryLevelLeft":false,"device_batteryLevelRight":"48%","device_batteryLevelMain":"60%"}},
                  null,
                  {"Malformed":0},
                  {"MX Keys":{"device_batteryLevelMain":"18%"}}
                ]},
                {"device_connected":null},
                {"device_connected":[{"Headset Z":{"device_batteryLevelMain":"25%"}}]}
              ]
            }"#,
        )
        .unwrap();
        let mut devices = vec![DeviceBattery::new("MX Keys", 80, "keyboard", None)];
        add_mac_bluetooth_batteries(&doc, &mut devices);
        assert_eq!(
            devices,
            [
                DeviceBattery::new("MX Keys", 80, "keyboard", None),
                DeviceBattery::new("AirPods A", 72, "headphones", None),
                DeviceBattery::new("AirPods B", 48, "headphones", None),
                DeviceBattery::new("Headset Z", 25, "headphones", None),
            ]
        );
        add_mac_bluetooth_batteries(&serde_json::json!([1]), &mut devices);
        assert_eq!(devices.len(), 4);
    }

    #[test]
    fn upower_devices_are_read_as_the_csharp_code_reads_them() {
        let laptop = "  native-path:          BAT0\n  power supply:         yes\n  battery\n    present:             yes\n    state:               discharging\n    percentage:          87.5%\n";
        assert_eq!(
            parse_upower_device("/org/freedesktop/UPower/devices/battery_BAT0", laptop),
            Some(DeviceBattery::new("This laptop", 87, "laptop", Some(false)))
        );
        let mouse = "  model:                MX Master 3\n  mouse\n    percentage:          55%\n";
        assert_eq!(
            parse_upower_device(
                "/org/freedesktop/UPower/devices/mouse_hidpp_battery_0",
                mouse
            ),
            Some(DeviceBattery::new("MX Master 3", 55, "mouse", None))
        );
        let headset = "  model:                WH-1000XM4\n    state:               fully-charged\n    percentage:          100%\n";
        assert_eq!(
            parse_upower_device("/org/freedesktop/UPower/devices/headset_dev_AA", headset),
            Some(DeviceBattery::new(
                "WH-1000XM4",
                100,
                "headphones",
                Some(true)
            ))
        );
        // no model: the kind from the name of the file
        let unnamed = "    percentage:          140%\n";
        assert_eq!(
            parse_upower_device(
                "/org/freedesktop/UPower/devices/gaming_input_dev_1",
                unnamed
            ),
            Some(DeviceBattery::new(
                "gaming_input_dev_1",
                100,
                "controller",
                None
            ))
        );
        assert_eq!(
            parse_upower_device("/org/freedesktop/UPower/devices/DisplayDevice", laptop),
            None
        );
        assert_eq!(
            parse_upower_device("/org/freedesktop/UPower/devices/line_power_AC", laptop),
            None
        );
        assert_eq!(
            parse_upower_device(
                "/org/freedesktop/UPower/devices/battery_BAT1",
                "  present: no\n  percentage: 5%\n"
            ),
            None
        );
        assert_eq!(
            parse_upower_device(
                "/org/freedesktop/UPower/devices/battery_BAT1",
                "  state: charging\n"
            ),
            None
        );
    }

    #[test]
    fn ioreg_pairs_each_level_with_the_product_before_it() {
        let io = "+-o AppleDeviceManagementHIDEventService  <class AppleDeviceManagementHIDEventService>\n\
                  |   \"Product\" = \"Magic Mouse\"\n    \"BatteryPercent\" = 64\n\
                  \"BatteryPercent\" = 12\n\
                  \"Product\" = \"Magic Keyboard with Touch ID\"\n \"BatteryPercent\" = 91\n";
        assert_eq!(
            parse_ioreg(io),
            [
                DeviceBattery::new("Magic Mouse", 64, "mouse", None),
                DeviceBattery::new("Magic Keyboard with Touch ID", 91, "keyboard", None),
            ]
        );
    }

    #[test]
    fn devices_sort_by_name_with_the_laptop_last() {
        let sorted = sort_devices(vec![
            DeviceBattery::new("This laptop", 50, "laptop", None),
            DeviceBattery::new("Zeta", 1, "other", None),
            DeviceBattery::new("Alpha", 2, "mouse", None),
        ]);
        let names: Vec<&str> = sorted.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "Zeta", "This laptop"]);
    }

    #[tokio::test]
    async fn the_snapshot_is_empty_until_a_read_and_reads_only_on_request() {
        let monitor = BatteryMonitor::new();
        assert!(monitor.snapshot().is_empty());
        assert!(monitor.wanted(), "asking for the list starts the reads");
    }
}
