//! Port of `Unix/UnixSystem.cs`: CPU, memory and temperature readings on Linux (/proc, /sys) and macOS (Mach, sysctl,
//! vm_stat).

#[cfg(target_os = "macos")]
use std::sync::Mutex;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use super::sh;

/// Cumulative CPU ticks: (idle, total). The load is the change of busy ticks over the change of total ticks.
pub fn cpu_times() -> (i64, i64) {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn mach_host_self() -> u32;
            fn host_statistics(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
        }
        let mut ticks = [0i32; 4]; // user, system, idle, nice
        let mut count = 4u32;
        // SAFETY: HOST_CPU_LOAD_INFO (3) fills four integers; `count` says the buffer holds four.
        if unsafe { host_statistics(mach_host_self(), 3, ticks.as_mut_ptr(), &mut count) } != 0 {
            return (0, 0);
        }
        let t = ticks.map(|v| i64::from(v as u32));
        (t[2], t[0] + t[1] + t[2] + t[3])
    }
    #[cfg(not(target_os = "macos"))]
    {
        proc_stat_times(&std::fs::read_to_string("/proc/stat").unwrap_or_default())
    }
}

/// The first line of /proc/stat: cpu user nice system idle iowait irq softirq steal …; idle is idle + iowait.
pub fn proc_stat_times(stat: &str) -> (i64, i64) {
    let f: Vec<i64> = stat
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|v| v.parse().unwrap_or(0))
        .collect();
    if f.len() < 5 {
        return (0, 0);
    }
    (f[3] + f[4], f.iter().sum())
}

/// (used, total) memory in bytes. macOS: app + wired + compressed pages from vm_stat (cached for 5 s); Linux: total
/// minus available.
pub fn memory() -> (f64, f64) {
    #[cfg(target_os = "macos")]
    {
        static CACHE: Mutex<Option<(Instant, f64, f64)>> = Mutex::new(None);
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, used, total)) = *cache
            && at.elapsed() < Duration::from_secs(5)
            && total > 0.0
        {
            return (used, total);
        }
        let total = sh::run("sysctl", &["-n", "hw.memsize"])
            .and_then(|t| t.trim().parse::<f64>().ok())
            .unwrap_or(0.0);
        let used = vm_stat_used(&sh::run("vm_stat", &[]).unwrap_or_default());
        *cache = Some((Instant::now(), used, total));
        (used, total)
    }
    #[cfg(not(target_os = "macos"))]
    {
        meminfo_memory(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default())
    }
}

/// Used bytes from vm_stat's text: (anonymous − purgeable, or active, pages) + wired + compressor, times the page size.
pub fn vm_stat_used(vm: &str) -> f64 {
    let page = vm
        .split("page size of ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|n| n.parse::<f64>().ok())
        .unwrap_or(4096.0);
    let pages = |name: &str| -> f64 {
        vm.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|v| v.trim().trim_end_matches('.').parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let anon = pages("Anonymous pages");
    let app = if anon > 0.0 {
        (anon - pages("Pages purgeable")).max(0.0)
    } else {
        pages("Pages active")
    };
    (app + pages("Pages wired down") + pages("Pages occupied by compressor")) * page
}

/// (MemTotal − MemAvailable, MemTotal) in bytes from /proc/meminfo.
pub fn meminfo_memory(meminfo: &str) -> (f64, f64) {
    let kb = |key: &str| -> f64 {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|n| n.parse::<f64>().ok())
            .unwrap_or(0.0)
            * 1024.0
    };
    let total = kb("MemTotal");
    (total - kb("MemAvailable"), total)
}

pub fn cpu_name() -> String {
    #[cfg(target_os = "macos")]
    {
        match sh::run("sysctl", &["-n", "machdep.cpu.brand_string"]) {
            Some(n) if !n.trim().is_empty() => n.trim().to_string(),
            _ => "CPU".into(),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        cpuinfo_name(&std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default())
    }
}

/// The first of "model name", "Model", "Hardware" in /proc/cpuinfo (`^key\s*:\s*(.+)$`), else "CPU".
pub fn cpuinfo_name(info: &str) -> String {
    for key in ["model name", "Model", "Hardware"] {
        let hit = info.lines().find_map(|l| {
            let v = l.strip_prefix(key)?.trim_start().strip_prefix(':')?;
            (!v.is_empty()).then(|| v.trim().to_string())
        });
        if let Some(name) = hit {
            return name;
        }
    }
    "CPU".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers_read_the_csharp_fields() {
        assert_eq!(
            proc_stat_times("cpu  10 1 5 100 4 0 2 0 9 9\ncpu0 1 1 1 1"),
            (104, 122)
        );
        assert_eq!(
            meminfo_memory("MemTotal:  1000 kB\nMemFree: 1 kB\nMemAvailable:   400 kB\n"),
            (600.0 * 1024.0, 1000.0 * 1024.0)
        );
        let vm = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages active:   10.\n\
                  Pages wired down:  3.\nPages purgeable:  2.\nAnonymous pages:  7.\nPages occupied by compressor:  1.\n";
        assert_eq!(vm_stat_used(vm), (5.0 + 3.0 + 1.0) * 16384.0);
        assert_eq!(
            cpuinfo_name("processor\t: 0\nmodel name\t: AMD Ryzen 9  \n"),
            "AMD Ryzen 9"
        );
        assert_eq!(cpuinfo_name("Hardware\t: BCM2835\n"), "BCM2835");
        assert_eq!(cpuinfo_name(""), "CPU");
    }
}
