//! Port of `SystemMonitor.cs`: CPU, RAM, network, disk and GPU metrics for `pc` in the state.

use serde::Serialize;

/// One disk (`DiskInfo`): used and total size in GB, whole numbers.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskInfo {
    pub name: String,
    pub used_gb: f64,
    pub total_gb: f64,
}

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// `Math.Round(x, digits)`: half to even, like .NET.
pub fn round_dotnet(x: f64, digits: i32) -> f64 {
    let scale = 10f64.powi(digits);
    (x * scale).round_ties_even() / scale
}

/// Cumulative CPU ticks: (idle, total). Windows: kernel time includes idle time.
pub fn cpu_times() -> (i64, i64) {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::GetSystemTimes;
        let (mut idle, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: three valid FILETIME out-pointers.
        if unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.is_err() {
            return (0, 0);
        }
        let n =
            |f: FILETIME| ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) as i64;
        (n(idle), n(kernel) + n(user))
    }
    #[cfg(not(windows))]
    crate::unix::unix_system::cpu_times()
}

/// (used, total) memory in bytes.
pub fn memory() -> (f64, f64) {
    #[cfg(windows)]
    {
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: dwLength is set as the API requires.
        if unsafe { GlobalMemoryStatusEx(&mut m) }.is_err() {
            return (0.0, 0.0);
        }
        (
            (m.ullTotalPhys - m.ullAvailPhys) as f64,
            m.ullTotalPhys as f64,
        )
    }
    #[cfg(not(windows))]
    crate::unix::unix_system::memory()
}

/// A mounted volume as `DriveInfo` sees it: name, whether it is a fixed drive, total and free bytes (free counts the
/// blocks reserved for root, as `TotalFreeSpace` does).
#[derive(Debug, Clone)]
pub struct Volume {
    pub name: String,
    pub fixed: bool,
    pub total: u64,
    pub free: u64,
}

/// `ReadDisks`: fixed, ready drives over 2 GB; on Linux and macOS only real volumes, one per (size, free GB); at most 6.
pub fn read_disks() -> Vec<DiskInfo> {
    pick_disks(volumes())
}

pub fn pick_disks(volumes: Vec<Volume>) -> Vec<DiskInfo> {
    let mut drives: Vec<Volume> = volumes
        .into_iter()
        .filter(|v| v.fixed && v.total as f64 > 2.0 * GB)
        .collect();
    if !cfg!(windows) {
        let mut seen = std::collections::HashSet::new();
        drives.retain(|d| {
            let n = d.name.as_str();
            (n == "/"
                || n.starts_with("/home")
                || n.starts_with("/mnt/")
                || n.starts_with("/media/")
                || n.starts_with("/Volumes/")
                || n.starts_with("/data"))
                && !n.contains("/wsl")
                && !n.contains("docker")
                && !n.starts_with("/mnt/c")
                && !n.is_empty()
                && seen.insert((d.total, d.free / (1 << 30)))
        });
    }
    drives
        .into_iter()
        .take(6)
        .map(|d| DiskInfo {
            name: disk_label(&d.name),
            used_gb: round_dotnet((d.total - d.free.min(d.total)) as f64 / GB, 0),
            total_gb: round_dotnet(d.total as f64 / GB, 0),
        })
        .collect()
}

pub fn disk_label(name: &str) -> String {
    if cfg!(windows) {
        name.trim_end_matches('\\').to_string()
    } else if name == "/" {
        "/".into()
    } else {
        let trimmed = name.trim_end_matches('/');
        trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
    }
}

/// .NET's `DriveType` for a Unix file system name (`Interop.MountPoints.FormatInfo`): only the fixed kinds matter here;
/// network, RAM, optical, virtual and unknown file systems are not fixed drives.
pub fn is_fixed_file_system(fs: &str) -> bool {
    matches!(
        fs.to_ascii_lowercase().as_str(),
        "adfs"
            | "affs"
            | "apfs"
            | "befs"
            | "bfs"
            | "btrfs"
            | "drvfs"
            | "ecryptfs"
            | "efs"
            | "exfat"
            | "ext"
            | "ext2"
            | "ext3"
            | "ext4"
            | "ext4dev"
            | "f2fs"
            | "fat"
            | "fuseblk"
            | "fuseext2"
            | "fusefat"
            | "hfs"
            | "hfs+"
            | "hfsplus"
            | "hfsx"
            | "hpfs"
            | "jffs"
            | "jffs2"
            | "jfs"
            | "lxfs"
            | "minix"
            | "msdos"
            | "nilfs"
            | "ntfs"
            | "ntfs3"
            | "ocfs2"
            | "overlay"
            | "overlayfs"
            | "qnx4"
            | "qnx6"
            | "reiserfs"
            | "squashfs"
            | "sysv"
            | "ubifs"
            | "ufs"
            | "ufs2"
            | "umsdos"
            | "vboxsf"
            | "vfat"
            | "vxfs"
            | "wslfs"
            | "xenix"
            | "xfs"
            | "zfs"
            | "zfs-fuse"
    )
}

#[cfg(target_os = "linux")]
fn volumes() -> Vec<Volume> {
    // /proc/mounts as getmntent reads it: source, mount point (octal escapes), type, …
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (_, point, fs) = (f.next()?, f.next()?, f.next()?);
            let name = unescape_mount(point);
            let (total, free) = statvfs(&name)?;
            Some(Volume {
                fixed: is_fixed_file_system(fs),
                name,
                total,
                free,
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(target_os = "linux")]
fn statvfs(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    // SAFETY: zeroed statvfs is a valid out-buffer; `c` is a NUL-terminated path.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let unit = s.f_frsize as u64;
    Some((s.f_blocks as u64 * unit, s.f_bfree as u64 * unit))
}

#[cfg(target_os = "macos")]
fn volumes() -> Vec<Volume> {
    let mut list: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo returns a pointer to its own static array of `n` entries (valid until the next call).
    let n = unsafe { libc::getmntinfo(&mut list, libc::MNT_NOWAIT) };
    if n <= 0 || list.is_null() {
        return Vec::new();
    }
    let all = unsafe { std::slice::from_raw_parts(list, n as usize) };
    let text = |chars: &[libc::c_char]| -> String {
        let bytes: Vec<u8> = chars
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    all.iter()
        .map(|s| Volume {
            name: text(&s.f_mntonname),
            fixed: is_fixed_file_system(&text(&s.f_fstypename)),
            total: s.f_blocks * u64::from(s.f_bsize),
            free: s.f_bfree * u64::from(s.f_bsize),
        })
        .collect()
}

#[cfg(windows)]
fn volumes() -> Vec<Volume> {
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
    };
    use windows::core::HSTRING;
    const DRIVE_FIXED: u32 = 3;
    // SAFETY: no arguments; a bit per drive letter.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| {
            let name = format!("{}:\\", (b'A' + i) as char);
            let root = HSTRING::from(name.as_str());
            // SAFETY: `root` is a valid NUL-terminated wide string; the out-pointers are valid u64s.
            let fixed = unsafe { GetDriveTypeW(&root) } == DRIVE_FIXED;
            let (mut total, mut free) = (0u64, 0u64);
            unsafe { GetDiskFreeSpaceExW(&root, None, Some(&mut total), Some(&mut free)) }.ok()?;
            Some(Volume {
                name,
                fixed,
                total,
                free,
            })
        })
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn volumes() -> Vec<Volume> {
    Vec::new()
}

pub fn read_cpu_name() -> String {
    #[cfg(windows)]
    {
        use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
        use windows::core::w;
        let mut buf = [0u16; 256];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `size` bytes; the key and value names are static wide strings.
        let found = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                w!(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0"),
                w!("ProcessorNameString"),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        }
        .is_ok();
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        let name = String::from_utf16_lossy(&buf[..len]).trim().to_string();
        if found && !name.is_empty() {
            name
        } else {
            "CPU".into()
        }
    }
    #[cfg(not(windows))]
    crate::unix::unix_system::cpu_name()
}

/// Seconds since boot, on the clock `Environment.TickCount64` reads (Linux CLOCK_MONOTONIC, macOS CLOCK_UPTIME_RAW,
/// Windows GetTickCount64).
pub fn uptime_sec() -> i64 {
    #[cfg(windows)]
    {
        // SAFETY: no arguments.
        (unsafe { windows::Win32::System::SystemInformation::GetTickCount64() } / 1000) as i64
    }
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn clock_gettime_nsec_np(clock_id: libc::clockid_t) -> u64;
        }
        // SAFETY: a clock id the system defines; the call has no other preconditions.
        (unsafe { clock_gettime_nsec_np(libc::CLOCK_UPTIME_RAW) } / 1_000_000_000) as i64
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        // SAFETY: a zeroed timespec is a valid out-buffer.
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec as i64
    }
}

const CPU_TEMP_NAMES: [&str; 6] = [
    "Core (Tctl/Tdie)",
    "Tctl/Tdie",
    "CPU Package",
    "Tdie",
    "Tctl",
    "Core (Tdie)",
];

static CORE_SENSOR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)^(?:CPU )?Core #\d+$").expect("CoreSensor")
});

/// The CPU temperature from LibreHardwareMonitor's data.json (`FindCpuTemp`): the package (Intel) or Tctl/Tdie (AMD)
/// reading when there is one; otherwise the hottest "CPU Core #n" / "Core #n" temperature under a CPU's own node.
pub fn find_cpu_temp(root: &serde_json::Value) -> Option<f64> {
    if let Some(package) = find_temp(root, &CPU_TEMP_NAMES) {
        return Some(package);
    }
    fn visit(node: &serde_json::Value, in_cpu: bool, hottest: &mut Option<f64>) {
        // a null or non-object entry is skipped, not allowed to throw away the readings after it
        if !node.is_object() {
            return;
        }
        let in_cpu = in_cpu || is_cpu_node(node);
        if in_cpu
            && let Some(name) = str_prop(node, "Text")
            && CORE_SENSOR.is_match(name)
            && let Some(t) = read_celsius(node)
            && hottest.is_none_or(|h| t > h)
        {
            *hottest = Some(t);
        }
        if let Some(kids) = node.get("Children").and_then(|k| k.as_array()) {
            for k in kids {
                visit(k, in_cpu, hottest);
            }
        }
    }
    let mut hottest = None;
    visit(root, false, &mut hottest);
    hottest
}

/// A string property of an object node; None when it is missing or not a string.
fn str_prop<'a>(node: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    node.get(name).and_then(|v| v.as_str())
}

fn is_cpu_node(node: &serde_json::Value) -> bool {
    let starts = |s: &str, p: &str| s.len() >= p.len() && s[..p.len()].eq_ignore_ascii_case(p);
    str_prop(node, "HardwareId")
        .is_some_and(|hw| starts(hw, "/intelcpu/") || starts(hw, "/amdcpu/"))
        || str_prop(node, "ImageURL").is_some_and(|url| {
            url.len() >= 8
                && url.is_char_boundary(url.len() - 8)
                && url[url.len() - 8..].eq_ignore_ascii_case("/cpu.png")
        })
}

/// A temperature value such as "54,0 °C" or "140.0 °F"; load and clock sensors share the core names but have no °.
fn read_celsius(node: &serde_json::Value) -> Option<f64> {
    let s = node.get("Value")?.as_str()?;
    if !s.contains('°') {
        return None;
    }
    // an older reply has no type; one that names a type must name a temperature
    if let Some(kind) = node.get("Type")
        && !kind.is_null()
        && kind.as_str() != Some("Temperature")
    {
        return None;
    }
    match crate::gpus::parse_lhm_value(s, Some("Temperature")) {
        Some(("temp", v)) => Some(v),
        _ => None,
    }
}

fn find_temp(node: &serde_json::Value, names: &[&str]) -> Option<f64> {
    if !node.is_object() {
        return None;
    }
    if let Some(text) = str_prop(node, "Text")
        && names.contains(&text)
        && let Some(temp) = read_celsius(node)
    {
        return Some(temp);
    }
    node.get("Children")?
        .as_array()?
        .iter()
        .find_map(|k| find_temp(k, names))
}

use std::collections::HashMap;

use crate::net_adapters::AdapterKind;

/// Whether the network meters count this adapter (`Counted`): the chosen ones by name, or else the automatic choice.
pub fn counted(name: &str, description: &str, kind: AdapterKind, chosen: &[String]) -> bool {
    if !chosen.is_empty() {
        return chosen
            .iter()
            .any(|c| c.to_lowercase() == name.to_lowercase());
    }
    if !matches!(
        kind,
        AdapterKind::Ethernet | AdapterKind::Wireless80211 | AdapterKind::GigabitEthernet
    ) {
        return false;
    }
    if !cfg!(windows)
        && ["docker", "veth", "br-", "virbr", "utun", "awdl", "llw"]
            .iter()
            .any(|p| name.starts_with(p))
    {
        return false;
    }
    let d = format!("{description} {name}");
    let lower = d.to_lowercase();
    !([
        "virtual",
        "hyper-v",
        "vethernet",
        "vmware",
        "wsl",
        "bluetooth",
    ]
    .iter()
    .any(|w| lower.contains(w))
        || d.contains("TAP"))
}

/// Up, non-loopback adapters for Settings › Ping & network: name, description, whether the automatic choice counts it.
pub fn adapters() -> Vec<serde_json::Value> {
    crate::net_adapters::all()
        .into_iter()
        .filter(|ni| ni.up && ni.kind != AdapterKind::Loopback)
        .map(|ni| {
            let auto = counted(&ni.name, &ni.description, ni.kind, &[]);
            serde_json::json!({"name": ni.name, "description": ni.description, "auto": auto})
        })
        .collect()
}

/// Byte counters of the counted, up adapters by id.
pub fn net_bytes(chosen: &[String]) -> HashMap<String, (i64, i64)> {
    crate::net_adapters::all()
        .into_iter()
        .filter(|ni| ni.up && counted(&ni.name, &ni.description, ni.kind, chosen))
        .filter_map(|ni| Some((ni.id, ni.bytes?)))
        .collect()
}

/// Bytes per second down and up between two readings; only adapters in both count, and a counter that went back
/// (a reset) adds nothing.
pub fn network_rates(
    before: &HashMap<String, (i64, i64)>,
    after: &HashMap<String, (i64, i64)>,
    seconds: f64,
) -> (f64, f64) {
    let (mut received, mut sent) = (0f64, 0f64);
    for (id, now) in after {
        // newly active adapters already carry lifetime totals: only compare adapters sampled in both readings
        let Some(previous) = before.get(id) else {
            continue;
        };
        if now.0 >= previous.0 {
            received += (now.0 - previous.0) as f64;
        }
        if now.1 >= previous.1 {
            sent += (now.1 - previous.1) as f64;
        }
    }
    (received / seconds, sent / seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_total_is_positive_and_used_is_within_it() {
        let (used, total) = memory();
        assert!(total > 256.0 * 1024.0 * 1024.0, "total {total}");
        assert!(used > 0.0 && used <= total, "used {used} of {total}");
    }

    #[test]
    fn cpu_times_grow_between_two_reads() {
        let (idle0, total0) = cpu_times();
        let spin = std::time::Instant::now();
        while spin.elapsed() < std::time::Duration::from_millis(300) {
            std::hint::black_box(spin.elapsed());
        }
        let (idle1, total1) = cpu_times();
        assert!(total0 > 0 && total1 > total0, "{total0} -> {total1}");
        assert!(idle1 >= idle0 && idle1 - idle0 <= total1 - total0);
    }

    #[test]
    fn disks_have_a_name_and_used_within_total() {
        let disks = read_disks();
        assert!(!disks.is_empty() && disks.len() <= 6, "{disks:?}");
        for d in &disks {
            assert!(
                !d.name.is_empty() && d.total_gb > 2.0 && d.used_gb <= d.total_gb,
                "{d:?}"
            );
            assert_eq!(d.used_gb.fract(), 0.0);
        }
    }

    #[test]
    fn disk_label_matches_csharp() {
        if cfg!(windows) {
            assert_eq!(disk_label("C:\\"), "C:");
        } else {
            assert_eq!(disk_label("/"), "/");
            assert_eq!(disk_label("/home"), "home");
            assert_eq!(disk_label("/Volumes/storage/"), "storage");
            assert_eq!(disk_label("/mnt/data"), "data");
        }
    }

    #[test]
    fn cpu_name_and_uptime_are_read() {
        let name = read_cpu_name();
        assert!(!name.is_empty() && name == name.trim(), "{name:?}");
        assert!(uptime_sec() > 0);
    }

    // CpuTempTests: the package or Tctl/Tdie reading first; on older Intel CPUs the hottest core of the CPU.
    fn temp(json: &str) -> Option<f64> {
        find_cpu_temp(&serde_json::from_str(json).unwrap())
    }

    fn sensor(text: &str, value: &str, kind: &str) -> String {
        format!(r#"{{"Text":"{text}","Value":"{value}","Type":"{kind}","Children":[]}}"#)
    }

    fn t_sensor(text: &str, value: &str) -> String {
        sensor(text, value, "Temperature")
    }

    fn hardware(text: &str, id: &str, image: &str, groups: &[String]) -> String {
        format!(
            r#"{{"Text":"{text}","HardwareId":"{id}","ImageURL":"{image}","Children":[{}]}}"#,
            groups.join(",")
        )
    }

    fn group(text: &str, sensors: &[String]) -> String {
        format!(r#"{{"Text":"{text}","Children":[{}]}}"#, sensors.join(","))
    }

    fn root(hardware: &[String]) -> String {
        format!(
            r#"{{"Text":"Sensor","Children":[{{"Text":"PC","Children":[{}]}}]}}"#,
            hardware.join(",")
        )
    }

    #[test]
    fn the_package_reading_wins_when_there_is_one() {
        let json = root(&[hardware(
            "Intel Core i7-12700K",
            "/intelcpu/0",
            "images_icon/cpu.png",
            &[group(
                "Temperatures",
                &[
                    t_sensor("CPU Core #1", "71,0 °C"),
                    t_sensor("CPU Package", "64,0 °C"),
                ],
            )],
        )]);
        assert_eq!(temp(&json), Some(64.0));
    }

    #[test]
    fn older_intel_cpus_use_the_hottest_core() {
        let json = root(&[hardware(
            "Intel Core i5-3570K",
            "/intelcpu/0",
            "images_icon/cpu.png",
            &[
                group("Clocks", &[sensor("CPU Core #1", "3400,0 MHz", "Clock")]),
                group(
                    "Temperatures",
                    &[
                        t_sensor("CPU Core #1", "48,0 °C"),
                        t_sensor("CPU Core #2", "53,5 °C"),
                        t_sensor("Core #3", "50,0 °C"),
                    ],
                ),
                group("Load", &[sensor("CPU Core #2", "97,0 %", "Load")]),
            ],
        )]);
        assert_eq!(temp(&json), Some(53.5));
    }

    #[test]
    fn core_sensors_outside_the_cpu_never_count() {
        let json = root(&[
            hardware(
                "NVIDIA GeForce RTX 4070",
                "/gpu-nvidia/0",
                "images_icon/nvidia.png",
                &[group("Temperatures", &[t_sensor("Core #1", "88,0 °C")])],
            ),
            hardware(
                "Samsung SSD 980",
                "/nvme/0",
                "images_icon/hdd.png",
                &[group("Temperatures", &[t_sensor("Core #2", "79,0 °C")])],
            ),
        ]);
        assert_eq!(temp(&json), None);
    }

    #[test]
    fn a_cpu_is_recognised_by_its_icon_when_it_has_no_hardware_id() {
        let json = root(&[hardware(
            "Intel Core 2 Quad Q9550",
            "",
            "images_icon/cpu.png",
            &[group(
                "Temperatures",
                &[
                    t_sensor("CPU Core #1", "45,0 °C"),
                    t_sensor("CPU Core #4", "47,0 °C"),
                ],
            )],
        )]);
        assert_eq!(temp(&json), Some(47.0));
    }

    #[test]
    fn amd_tctl_still_works() {
        let json = root(&[hardware(
            "AMD Ryzen 7 7800X3D",
            "/amdcpu/0",
            "images_icon/cpu.png",
            &[group(
                "Temperatures",
                &[
                    t_sensor("Core (Tctl/Tdie)", "61,3 °C"),
                    t_sensor("CCD1 (Tdie)", "58,0 °C"),
                ],
            )],
        )]);
        assert_eq!(temp(&json), Some(61.3));
    }

    #[test]
    fn no_cpu_sensors_means_no_temperature() {
        assert_eq!(temp(&root(&[])), None);
    }

    #[test]
    fn fahrenheit_sensors_convert_to_celsius_and_null_children_are_tolerated() {
        let json = r#"{"Text":"Sensor","Children":[
            {"Text":"Motherboard","Children":null},
            {"Text":"Intel Core i7","HardwareId":"/intelcpu/0","Children":[
              {"Text":"Temperatures","Children":[{"Text":"CPU Package","Value":"140,0 °F","Type":"Temperature","Children":null}]}]}]}"#;
        assert_eq!(temp(json), Some(60.0));
    }

    // CpuTempMalformedTests: a malformed reply skips that entry instead of losing the whole reading.
    #[test]
    fn non_object_nodes_and_non_string_names_are_skipped() {
        for (json, expected) in [
            (
                r#"{"Children":[null,42,"x",{"Text":"CPU Package","Value":"64,0 °C","Type":"Temperature","Children":[]}]}"#,
                64.0,
            ),
            (
                r#"{"Children":[{"Text":7,"HardwareId":5,"ImageURL":true,"Children":[null]},{"Text":"CPU","HardwareId":"/intelcpu/0","Children":[null,{"Text":"CPU Core #1","Value":"55,0 °C","Type":"Temperature"}]}]}"#,
                55.0,
            ),
        ] {
            assert_eq!(temp(json), Some(expected), "{json}");
        }
    }

    #[test]
    fn an_explicit_non_temperature_type_is_not_read_as_a_temperature() {
        let json = r#"{"Children":[
              {"Text":"CPU Package","Value":"95 °C","Type":42},
              {"Text":"CPU Package","Value":"61,0 °C","Type":"Temperature"}]}"#;
        assert_eq!(temp(json), Some(61.0));
        // an older reply without a type still counts
        assert_eq!(
            temp(r#"{"Children":[{"Text":"CPU Package","Value":"58,0 °C"}]}"#),
            Some(58.0)
        );
        assert_eq!(
            temp(r#"{"Children":[{"Text":"CPU Package","Value":"57,0 °C","Type":null}]}"#),
            Some(57.0)
        );
    }

    // NetworkAdapterTests (#190 / #169): the meters count the adapters the user picked, or else the automatic choice.
    fn none() -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn automatic_counts_ethernet_and_wifi_but_not_virtual_or_vpn() {
        use AdapterKind::*;
        assert!(counted(
            "Ethernet",
            "Realtek Gaming USB 2.5GbE Family Controller",
            Ethernet,
            &none()
        ));
        assert!(counted(
            "Wi-Fi",
            "Intel(R) Wi-Fi 6 AX201 160MHz",
            Wireless80211,
            &none()
        ));
        assert!(!counted(
            "vEthernet (WSL)",
            "Hyper-V Virtual Ethernet Adapter",
            Ethernet,
            &none()
        ));
        assert!(!counted(
            "Bluetooth Network Connection",
            "Bluetooth Device (Personal Area Network)",
            Ethernet,
            &none()
        ));
        assert!(!counted("WireGuard", "WireGuard Tunnel", Unknown, &none())); // not Ethernet / Wi-Fi
    }

    #[test]
    fn chosen_adapters_replace_the_automatic_choice() {
        use AdapterKind::*;
        let chosen = vec!["ethernet".to_string()]; // names match without case
        assert!(counted(
            "Ethernet",
            "Realtek Gaming USB 2.5GbE Family Controller",
            Ethernet,
            &chosen
        ));
        assert!(!counted(
            "Wi-Fi",
            "Intel(R) Wi-Fi 6 AX201 160MHz",
            Wireless80211,
            &chosen
        ));
        // a pick may be anything the user wants counted, a VPN tunnel too
        assert!(counted(
            "WireGuard",
            "WireGuard Tunnel",
            Unknown,
            &["WireGuard".to_string()]
        ));
    }

    #[test]
    fn the_setting_is_tidied_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        let store = crate::config::store::ConfigStore::new(path);
        assert!(store.save(r#"{ "ping": { "host": "1.1.1.1", "adapters": [" Ethernet ", "ethernet", "", "Wi-Fi"] } }"#, None).is_ok());
        assert_eq!(store.get().ping.adapters, ["Ethernet", "Wi-Fi"]);
    }

    // NetworkRatesTests
    fn rates(before: &[(&str, (i64, i64))], after: &[(&str, (i64, i64))]) -> (f64, f64) {
        let map = |l: &[(&str, (i64, i64))]| {
            l.iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect::<HashMap<_, _>>()
        };
        network_rates(&map(before), &map(after), 2.0)
    }

    #[test]
    fn an_interface_that_appears_or_disappears_does_not_change_other_traffic() {
        assert_eq!(
            rates(
                &[("wifi", (100, 200)), ("detached", (1000, 1000))],
                &[("wifi", (140, 220)), ("attached", (9000, 9500))]
            ),
            (20.0, 10.0)
        );
    }

    #[test]
    fn reset_counters_do_not_hide_traffic_from_another_interface() {
        assert_eq!(
            rates(
                &[("wifi", (100, 200)), ("ethernet", (1000, 1000))],
                &[("wifi", (140, 220)), ("ethernet", (30, 20))]
            ),
            (20.0, 10.0)
        );
    }

    #[test]
    fn adapters_lists_up_non_loopback_adapters_with_the_automatic_choice() {
        let list = adapters();
        assert!(!list.is_empty());
        for a in &list {
            let keys: Vec<&String> = a.as_object().unwrap().keys().collect();
            assert_eq!(keys, ["name", "description", "auto"], "{a}");
            assert_ne!(a["name"], "lo");
        }
    }
}
