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
}
