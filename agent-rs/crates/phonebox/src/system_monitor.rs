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

/// The GPU from the ioreg text (`ParseAppleGpu`).
pub fn parse_apple_gpu(ioreg: &str, total_memory_bytes: f64) -> Option<crate::gpus::GpuInfo> {
    use std::sync::LazyLock;
    static UTIL: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#""Device Utilization %"\s*=\s*(\d+)"#).expect("util"));
    static MODEL: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#""model"\s*=\s*"([^"]+)""#).expect("model"));
    static ALLOC: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#""Alloc system memory"\s*=\s*(\d+)"#).expect("alloc"));
    let util: f64 = UTIL.captures(ioreg)?[1].parse().ok()?; // no accelerator entry: nothing to show
    Some(crate::gpus::GpuInfo {
        name: MODEL
            .captures(ioreg)
            .map_or_else(|| "Apple GPU".to_string(), |m| m[1].to_string()),
        util: Some(util),
        vram_used: ALLOC
            .captures(ioreg)
            .and_then(|m| m[1].parse::<f64>().ok())
            .map(|a| round_dotnet(a / GB, 1)),
        vram_total: (total_memory_bytes > 0.0).then(|| round_dotnet(total_memory_bytes / GB, 1)),
        vendor: Some("apple".into()),
        source: Some("ioreg".into()),
        integrated: true,
        ..Default::default()
    })
}

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

/// `PcSnapshot`: what `pc` in the state holds, in the C# record's order.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PcSnapshot {
    pub cpu: Option<f64>,
    pub cpu_temp: Option<f64>,
    pub cpu_name: String,
    pub ram_used_gb: f64,
    pub ram_total_gb: f64,
    pub gpu: Option<crate::gpus::GpuInfo>,
    pub net_down_bps: f64,
    pub net_up_bps: f64,
    pub disks: Vec<DiskInfo>,
    pub uptime_sec: i64,
    pub gpus: Vec<crate::gpus::GpuInfo>,
}

/// What the GPU, temperature and disk loops found last; the sampling loop composes `pc` from it.
#[derive(Default)]
struct Found {
    // GPU sources, highest priority first: nvidia-smi, LibreHardwareMonitor, the OS (D3DKMT + counters / sysfs / ioreg)
    nvidia: Option<Vec<crate::gpus::GpuInfo>>,
    lhm: Option<Vec<crate::gpus::GpuInfo>>,
    os: Option<Vec<crate::gpus::GpuInfo>>,
    cpu_temp: Option<f64>,
    apple_gpu_temp: Option<f64>, // macOS: the GPU's temperature from the SMC, added to the ioreg GPU
}

/// Samples CPU, RAM, network, disk and GPU metrics once a second (`SystemMonitor`).
pub struct SystemMonitor {
    cfg: std::sync::Arc<crate::config::store::ConfigStore>,
    snap: std::sync::Mutex<Option<PcSnapshot>>,
    found: std::sync::Mutex<Found>,
}

/// Runs `f` until `stop`, sleeping what it returns between rounds.
async fn every<F, Fut>(stop: &crate::server::Stop, first: std::time::Duration, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::time::Duration>,
{
    let mut wait = first;
    loop {
        tokio::select! {
            _ = stop.stopped() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        wait = f().await;
    }
}

impl SystemMonitor {
    pub fn new(
        cfg: std::sync::Arc<crate::config::store::ConfigStore>,
    ) -> std::sync::Arc<SystemMonitor> {
        std::sync::Arc::new(SystemMonitor {
            cfg,
            snap: Default::default(),
            found: Default::default(),
        })
    }

    pub fn snapshot(&self) -> Option<PcSnapshot> {
        self.snap.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn found(&self) -> std::sync::MutexGuard<'_, Found> {
        self.found.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts every loop (`ExecuteAsync`, `GpuLoop`, `CpuTempLoop`); each ends on `stop`.
    pub fn start(
        self: &std::sync::Arc<Self>,
        stop: crate::server::Stop,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let mut tasks = vec![self.start_sampling(stop.clone())];
        if cfg!(target_os = "macos") {
            tasks.push(tokio::spawn(self.clone().apple_gpu_loop(stop.clone())));
        } else {
            tasks.push(tokio::spawn(self.clone().os_gpu_loop(stop.clone())));
            tasks.push(tokio::spawn(self.clone().nvidia_loop(stop.clone())));
        }
        tasks.push(tokio::spawn(self.clone().cpu_temp_loop(stop)));
        tasks
    }

    /// Only the once-a-second sampling loop: what a machine without GPU or temperature sources gets.
    pub fn start_sampling(
        self: &std::sync::Arc<Self>,
        stop: crate::server::Stop,
    ) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let chosen = {
                let cfg = me.cfg.clone();
                move || cfg.get().ping.adapters.clone()
            };
            let c = chosen.clone();
            let Ok((cpu_name, cpu0, net0)) = tokio::task::spawn_blocking(move || {
                (read_cpu_name(), cpu_times(), net_bytes(&c()))
            })
            .await
            else {
                return;
            };
            let mut prev = (cpu0, net0, std::time::Instant::now());
            let mut disks: Vec<DiskInfo> = Vec::new();
            let mut tick = 0u64;
            loop {
                tokio::select! {
                    _ = stop.stopped() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                }
                let (me2, cpu_name, chosen) = (me.clone(), cpu_name.clone(), chosen.clone());
                let (p, d, n) = (prev.clone(), disks.clone(), tick);
                let round = tokio::task::spawn_blocking(move || {
                    let ((idle0, total0), net0, t0) = p;
                    let (idle1, total1) = cpu_times();
                    let total = total1 - total0;
                    let cpu = (total > 0).then(|| {
                        (100.0 * (total - (idle1 - idle0)) as f64 / total as f64).clamp(0.0, 100.0)
                    });
                    let net1 = net_bytes(&chosen());
                    let t1 = std::time::Instant::now();
                    let secs = (t1 - t0).as_secs_f64().max(0.001);
                    let (down, up) = network_rates(&net0, &net1, secs);
                    let disks = if n % 15 == 0 { read_disks() } else { d };
                    let (mem_used, mem_total) = memory();
                    let (gpus, cpu_temp) = {
                        let f = me2.found();
                        (
                            crate::gpus::compose(&[
                                f.nvidia.as_deref(),
                                f.lhm.as_deref(),
                                f.os.as_deref(),
                            ]),
                            f.cpu_temp,
                        )
                    };
                    *me2.snap.lock().unwrap_or_else(|e| e.into_inner()) = Some(PcSnapshot {
                        cpu: cpu.map(|c| round_dotnet(c, 1)),
                        cpu_temp,
                        cpu_name,
                        ram_used_gb: round_dotnet(mem_used / GB, 1),
                        ram_total_gb: round_dotnet(mem_total / GB, 1),
                        gpu: gpus.first().cloned(),
                        net_down_bps: round_dotnet(down, 0),
                        net_up_bps: round_dotnet(up, 0),
                        disks: disks.clone(),
                        uptime_sec: uptime_sec(),
                        gpus,
                    });
                    (((idle1, total1), net1, t1), disks)
                })
                .await;
                if let Ok((p, d)) = round {
                    prev = p;
                    disks = d;
                }
                tick += 1;
            }
        })
    }

    /// Keeps nvidia-smi running and reads one line per GPU per second; restarts it 10 s after it ends or fails.
    async fn nvidia_loop(self: std::sync::Arc<Self>, stop: crate::server::Stop) {
        loop {
            let _ = self.nvidia_run(&stop).await;
            self.found().nvidia = None;
            tokio::select! {
                _ = stop.stopped() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
            }
        }
    }

    async fn nvidia_run(&self, stop: &crate::server::Stop) -> std::io::Result<()> {
        use tokio::io::AsyncBufReadExt;
        fn command(args: &[&str]) -> tokio::process::Command {
            let mut c = tokio::process::Command::new("nvidia-smi");
            c.args(args)
                .current_dir(crate::app_paths::neutral_cwd())
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            #[cfg(windows)]
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            c
        }
        let names_out = command(&[
            "--query-gpu=index,name,driver_version",
            "--format=csv,noheader",
        ])
        .output()
        .await?;
        let names = crate::gpus::parse_nvidia_names(&String::from_utf8_lossy(&names_out.stdout));
        let mut child = command(&[
            "--query-gpu=index,utilization.gpu,temperature.gpu,power.draw,power.limit,memory.used,memory.total,fan.speed,clocks.gr,clocks.mem",
            "--format=csv,noheader,nounits",
            "-lms",
            "1000",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()?;
        let Some(out) = child.stdout.take() else {
            return Ok(());
        };
        let mut lines = tokio::io::BufReader::new(out).lines();
        let mut cards = std::collections::BTreeMap::new();
        loop {
            tokio::select! {
                _ = stop.stopped() => return Ok(()),
                line = lines.next_line() => match line? {
                    None => return Ok(()),
                    Some(line) => {
                        if let Some((index, gpu)) = crate::gpus::parse_nvidia_line(&line, &names) {
                            cards.insert(index, gpu);
                            self.found().nvidia = Some(cards.values().cloned().collect());
                        }
                    }
                }
            }
        }
    }

    /// Every vendor's GPU through the OS: Windows' graphics kernel and GPU counters, Linux sysfs (amdgpu, i915/xe).
    async fn os_gpu_loop(self: std::sync::Arc<Self>, stop: crate::server::Stop) {
        #[cfg(windows)]
        let mut win = Some(crate::windows_gpu::WindowsGpu::new());
        let mut failures = 0u32;
        loop {
            #[cfg(windows)]
            let (sample, back) = {
                let g = win.take();
                tokio::task::spawn_blocking(move || {
                    let mut g = g.unwrap_or_default();
                    let r = g.sample();
                    (r, Some(g))
                })
                .await
                .unwrap_or_else(|e| (Err(e.to_string()), None))
            };
            #[cfg(windows)]
            {
                win = back;
            }
            #[cfg(not(windows))]
            let sample: Result<Vec<crate::gpus::GpuInfo>, String> =
                tokio::task::spawn_blocking(|| {
                    crate::gpus::read_sysfs(
                        std::path::Path::new("/sys/class/drm"),
                        None,
                        std::path::Path::new("/proc"),
                        None,
                    )
                })
                .await
                .map_err(|e| e.to_string());
            let (list, log, wait) = os_gpu_step(sample, &mut failures);
            if let Some(message) = log {
                crate::log::warn("SystemMonitor", &message);
            }
            self.found().os = list;
            tokio::select! {
                _ = stop.stopped() => return,
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// macOS has no nvidia-smi; the GPU's load and memory come from the IOAccelerator entry of ioreg.
    async fn apple_gpu_loop(self: std::sync::Arc<Self>, stop: crate::server::Stop) {
        let me = self.clone();
        every(&stop, std::time::Duration::ZERO, || {
            let me = me.clone();
            async move {
                let text = tokio::task::spawn_blocking(|| {
                    crate::unix::sh::run_with_timeout(
                        5000,
                        "ioreg",
                        &["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"],
                    )
                    .map(|t| (t, memory().1))
                })
                .await
                .ok()
                .flatten();
                let temp = me.found().apple_gpu_temp;
                me.found().os = text
                    .and_then(|(t, total)| parse_apple_gpu(&t, total))
                    .map(|g| vec![crate::gpus::GpuInfo { temp, ..g }]);
                std::time::Duration::from_secs(2)
            }
        })
        .await;
    }

    /// CPU temperature: Linux hwmon; macOS on Apple silicon the SMC (CPU and GPU); Windows LibreHardwareMonitor's web
    /// server (port 8085), whose answer carries the GPU sensors too.
    async fn cpu_temp_loop(self: std::sync::Arc<Self>, stop: crate::server::Stop) {
        if cfg!(target_os = "macos") {
            return self.apple_temp_loop(stop).await;
        }
        if cfg!(windows) {
            return self.lhm_loop(stop).await;
        }
        let me = self.clone();
        every(&stop, std::time::Duration::ZERO, || {
            let me = me.clone();
            async move {
                let t = tokio::task::spawn_blocking(crate::unix::unix_system::cpu_temp)
                    .await
                    .ok()
                    .flatten();
                me.found().cpu_temp = t;
                std::time::Duration::from_secs(if t.is_none() { 30 } else { 2 })
            }
        })
        .await;
    }

    async fn lhm_loop(self: std::sync::Arc<Self>, stop: crate::server::Stop) {
        let Ok(http) = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(600))
            .build()
        else {
            return;
        };
        let me = self.clone();
        every(&stop, std::time::Duration::ZERO, || {
            let (me, http) = (me.clone(), http.clone());
            async move {
                let doc = async {
                    http.get("http://127.0.0.1:8085/data.json")
                        .send()
                        .await?
                        .text()
                        .await
                }
                .await;
                match doc
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                {
                    Some(root) => {
                        let mut f = me.found();
                        f.cpu_temp = find_cpu_temp(&root);
                        f.lhm = Some(crate::gpus::parse_lhm(&root));
                        std::time::Duration::from_secs(2)
                    }
                    None => {
                        let mut f = me.found();
                        (f.cpu_temp, f.lhm) = (None, None);
                        std::time::Duration::from_secs(30)
                    }
                }
            }
        })
        .await;
    }

    /// macOS: CPU and GPU temperature from the SMC every 2 s. Without an SMC or its temperature keys it stops and both
    /// stay null; a failed read clears them and opens the SMC again 30 s later.
    async fn apple_temp_loop(self: std::sync::Arc<Self>, stop: crate::server::Stop) {
        let mut smc: Option<crate::unix::apple_smc::AppleSmc> = None;
        loop {
            let held = smc.take();
            let round = tokio::task::spawn_blocking(move || {
                let smc = held.or_else(crate::unix::apple_smc::AppleSmc::open)?;
                let reading = smc.read();
                Some((smc, reading))
            })
            .await
            .ok()
            .flatten();
            let wait = match round {
                None => {
                    crate::log::info(
                        "SystemMonitor",
                        "CPU temperature: no SMC temperature sensors on this Mac",
                    );
                    return;
                }
                Some((open, Ok((cpu, gpu)))) => {
                    (self.found().cpu_temp, self.found().apple_gpu_temp) = (cpu, gpu);
                    smc = Some(open);
                    2
                }
                Some((_, Err(_))) => {
                    (self.found().cpu_temp, self.found().apple_gpu_temp) = (None, None);
                    30
                }
            };
            tokio::select! {
                _ = stop.stopped() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(wait)) => {}
            }
        }
    }
}

impl crate::state::StateSource for SystemMonitor {
    fn snapshot(&self) -> serde_json::Value {
        self.snapshot().map_or(serde_json::Value::Null, |s| {
            serde_json::to_value(s).unwrap_or_default()
        })
    }
}

/// One round of the OS GPU loop: the sample (Err when it failed), the failures so far → what to publish, whether to log
/// the failure (only the first after a success), and how long to wait (2 s with GPUs, 60 s without).
pub fn os_gpu_step(
    sample: Result<Vec<crate::gpus::GpuInfo>, String>,
    failures: &mut u32,
) -> (
    Option<Vec<crate::gpus::GpuInfo>>,
    Option<String>,
    std::time::Duration,
) {
    let (list, log) = match sample {
        Ok(list) => {
            *failures = 0;
            (Some(list), None)
        }
        Err(e) => {
            *failures += 1;
            (None, (*failures == 1).then(|| format!("gpu: {e}")))
        }
    };
    // nothing to read (no GPU driver, a VM): look again now and then
    let wait = if list.as_ref().is_some_and(|l| !l.is_empty()) {
        2
    } else {
        60
    };
    (list, log, std::time::Duration::from_secs(wait))
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

    // AppleGpuTests: the macOS GPU comes from ioreg text: load, memory the GPU holds, the machine's RAM as total.
    const SIXTEEN_GB: f64 = 16.0 * 1024.0 * 1024.0 * 1024.0;

    #[test]
    fn load_and_memory_are_read_from_the_accelerator_entry() {
        let g = parse_apple_gpu(
            r#"+-o AGXAcceleratorG14X  <class AGXAcceleratorG14X>
  {
    "model" = "Apple M2 Pro"
    "PerformanceStatistics" = {"Device Utilization %"=37,"Alloc system memory"=2147483648,"Renderer Utilization %"=30}
  }"#,
            SIXTEEN_GB,
        )
        .unwrap();
        assert_eq!(g.name, "Apple M2 Pro");
        assert_eq!(g.util, Some(37.0));
        assert_eq!(g.vram_used, Some(2.0));
        assert_eq!(g.vram_total, Some(16.0));
        assert_eq!(g.temp, None); // no sensors: the panel must not show 0 °C
        assert_eq!(g.power, None);
    }

    #[test]
    fn spaces_around_the_equals_sign_are_fine() {
        let g = parse_apple_gpu(
            "\"Device Utilization %\" = 5\n\"Alloc system memory\" = 1073741824",
            SIXTEEN_GB,
        )
        .unwrap();
        assert_eq!(g.util, Some(5.0));
        assert_eq!(g.vram_used, Some(1.0));
        assert_eq!(g.name, "Apple GPU");
    }

    #[test]
    fn without_an_accelerator_there_is_no_gpu() {
        for text in ["", "nothing useful here"] {
            assert_eq!(parse_apple_gpu(text, SIXTEEN_GB), None);
        }
    }

    #[test]
    fn without_a_memory_figure_the_memory_stays_empty() {
        let g = parse_apple_gpu("\"Device Utilization %\"=9", 0.0).unwrap();
        assert_eq!(g.util, Some(9.0));
        assert_eq!((g.vram_used, g.vram_total), (None, None));
    }

    // Review Focus 1: no GPU, no temperature sensor, no nvidia-smi (a VM): pc is still published every second.
    #[tokio::test]
    async fn a_machine_without_gpu_or_temperature_sources_still_publishes_pc() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = std::sync::Arc::new(crate::config::store::ConfigStore::new(
            dir.path().join("config.json"),
        ));
        let monitor = SystemMonitor::new(cfg);
        let stop = crate::server::Stop::default();
        let task = monitor.start_sampling(stop.clone());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while monitor.snapshot().is_none() && std::time::Instant::now() < end {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let pc = monitor.snapshot().expect("no pc within 4 s");
        assert_eq!(
            (pc.gpu.clone(), pc.gpus.len(), pc.cpu_temp),
            (None, 0, None)
        );
        assert!(
            !pc.cpu_name.is_empty() && pc.ram_total_gb > 0.0 && pc.uptime_sec > 0,
            "{pc:?}"
        );
        assert!(pc.cpu.is_none_or(|c| (0.0..=100.0).contains(&c)));
        let json = crate::state::StateSource::snapshot(&*monitor);
        assert_eq!(json["gpu"], serde_json::Value::Null);
        stop.stop();
        tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .expect("the loop did not end")
            .unwrap();
    }

    #[test]
    fn gpu_failures_are_logged_once_until_a_sample_works() {
        let gpu = crate::gpus::GpuInfo {
            name: "AMD GPU".into(),
            ..Default::default()
        };
        let mut failures = 0;
        let (list, log, wait) = os_gpu_step(Err("no access".into()), &mut failures);
        assert_eq!(
            (list, log.as_deref(), wait.as_secs()),
            (None, Some("gpu: no access"), 60)
        );
        let (_, log, _) = os_gpu_step(Err("no access".into()), &mut failures);
        assert_eq!(log, None);
        let (list, log, wait) = os_gpu_step(Ok(vec![gpu.clone()]), &mut failures);
        assert_eq!((list, log, wait.as_secs()), (Some(vec![gpu]), None, 2));
        let (list, _, wait) = os_gpu_step(Ok(vec![]), &mut failures); // nothing to read (a VM): look again later
        assert_eq!((list, wait.as_secs()), (Some(vec![]), 60));
        let (_, log, _) = os_gpu_step(Err("gone".into()), &mut failures);
        assert_eq!(log.as_deref(), Some("gpu: gone"));
    }

    #[test]
    fn pc_json_has_the_csharp_keys_in_order() {
        let pc = PcSnapshot {
            cpu: Some(12.5),
            cpu_temp: None,
            cpu_name: "CPU".into(),
            ram_used_gb: 3.0,
            ram_total_gb: 16.0,
            gpu: None,
            net_down_bps: 0.0,
            net_up_bps: 10.0,
            disks: vec![DiskInfo {
                name: "/".into(),
                used_gb: 10.0,
                total_gb: 100.0,
            }],
            uptime_sec: 5,
            gpus: vec![],
        };
        let v = serde_json::to_value(&pc).unwrap();
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "cpu",
                "cpuTemp",
                "cpuName",
                "ramUsedGb",
                "ramTotalGb",
                "gpu",
                "netDownBps",
                "netUpBps",
                "disks",
                "uptimeSec",
                "gpus"
            ]
        );
        assert_eq!(v["cpuTemp"], serde_json::Value::Null);
        assert_eq!(
            v["disks"][0],
            serde_json::json!({"name": "/", "usedGb": 10.0, "totalGb": 100.0})
        );
        let gpu = serde_json::to_value(crate::gpus::GpuInfo::default()).unwrap();
        assert_eq!(gpu.as_object().unwrap().len(), 21); // every GpuInfo field is written, nulls too
    }
}
