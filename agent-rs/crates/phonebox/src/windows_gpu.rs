//! Port of `WindowsGpu.cs`: any vendor's GPU on Windows without vendor tools. The graphics kernel (D3DKMT) names the
//! adapters and gives their memory sizes, temperature, memory clock and fan; the "GPU Engine" and "GPU Adapter Memory"
//! performance counters give the load and the memory in use. Task Manager reads the same sources. The parsing and the
//! arithmetic are plain functions (tested on every OS); the system calls are Windows only.

use std::collections::HashMap;

/// A counter instance name: `pid_1234_luid_0x00000000_0x0001C621_phys_0_eng_3_engtype_3D` or
/// `luid_0x00000000_0x0001C621_phys_0`.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    /// "hhhhhhhh_llllllll", lower case: the adapter's key.
    pub luid: String,
    pub phys: String,
    pub eng: Option<String>,
}

static INSTANCE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)luid_0x([0-9a-f]{8})_0x([0-9a-f]{8})_phys_(\d+)(?:_eng_(\d+))?")
        .expect("Instance")
});

pub fn parse_instance(name: &str) -> Option<Instance> {
    let m = INSTANCE.captures(name)?;
    Some(Instance {
        luid: format!("{}_{}", m[1].to_lowercase(), m[2].to_lowercase()),
        phys: m[3].to_string(),
        eng: m.get(4).map(|e| e.as_str().to_string()),
    })
}

/// The adapter loads from "GPU Engine(*)\Utilization Percentage" readings: per engine summed over the processes using
/// it (at most 100), and an adapter's load is its busiest engine.
pub fn engine_load(readings: &[(String, f64)]) -> HashMap<String, f64> {
    let mut engines: HashMap<(String, String, Option<String>), f64> = HashMap::new();
    for (inst, v) in readings {
        if let Some(i) = parse_instance(inst) {
            *engines.entry((i.luid, i.phys, i.eng)).or_default() += v;
        }
    }
    let mut util: HashMap<String, f64> = HashMap::new();
    for ((luid, _, _), v) in engines {
        let e = util.entry(luid).or_default();
        *e = e.max(v.min(100.0));
    }
    util
}

/// Memory per adapter from "GPU Adapter Memory(*)" readings, summed per adapter.
pub fn memory_by_adapter(readings: &[(String, f64)]) -> HashMap<String, f64> {
    let mut out: HashMap<String, f64> = HashMap::new();
    for (inst, v) in readings {
        if let Some(i) = parse_instance(inst) {
            *out.entry(i.luid).or_default() += v;
        }
    }
    out
}

/// D3DKMT_ADAPTER_PERFDATA (WDDM 2.4+): (temperature °C, memory clock MHz, power % of the board limit, fan RPM).
#[derive(Debug, Clone, PartialEq)]
pub struct Perf {
    pub temp: Option<f64>,
    pub mem_clock: Option<f64>,
    pub power_pct: Option<f64>,
    pub fan_rpm: Option<f64>,
}

/// Temperature in tenths of a degree, power in tenths of a percent of the board limit.
pub fn parse_perf(buf: &[u8; 64]) -> Perf {
    use crate::system_monitor::round_dotnet;
    let u32_at = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let mem_hz = u64::from_le_bytes(buf[8..16].try_into().expect("8 bytes"));
    let (fan, power, temp) = (u32_at(48), u32_at(52), u32_at(56));
    Perf {
        temp: (temp > 0 && temp < 2000).then(|| round_dotnet(f64::from(temp) / 10.0, 1)),
        mem_clock: (mem_hz > 0).then(|| round_dotnet(mem_hz as f64 / 1e6, 0)),
        power_pct: (power > 0 && power <= 2000).then(|| round_dotnet(f64::from(power) / 10.0, 1)),
        fan_rpm: Some(f64::from(fan)),
    }
}

#[cfg(windows)]
pub use win::WindowsGpu;

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::time::{Duration, Instant};

    use super::{engine_load, memory_by_adapter, parse_perf};
    use crate::gpus::{GpuInfo, looks_integrated, vendor_of_name, vendor_of_pci};
    use crate::system_monitor::round_dotnet;

    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const PDH_MORE_DATA: u32 = 0x8000_07D2;
    const PDH_FMT_DOUBLE: u32 = 0x200;
    const PDH_FMT_NOCAP100: u32 = 0x8000;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct AdapterInfo {
        h_adapter: u32,
        luid_low: u32,
        luid_high: i32,
        num_of_sources: u32,
        precise_present_regions_preferred: i32,
    }
    #[repr(C)]
    struct EnumAdapters2 {
        num_adapters: u32,
        p_adapters: *mut AdapterInfo,
    }
    #[repr(C)]
    struct QueryAdapterInfo {
        h_adapter: u32,
        kind: i32,
        p_private_driver_data: *mut c_void,
        private_driver_data_size: u32,
    }
    #[repr(C)]
    struct FmtItem {
        name: *const u16,
        c_status: u32,
        value: f64,
    }

    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn D3DKMTEnumAdapters2(e: *mut EnumAdapters2) -> u32;
        fn D3DKMTQueryAdapterInfo(q: *mut QueryAdapterInfo) -> u32;
        fn D3DKMTCloseAdapter(h: *mut u32) -> u32;
    }
    #[link(name = "pdh")]
    unsafe extern "system" {
        fn PdhOpenQueryW(source: *const u16, user: usize, query: *mut isize) -> u32;
        fn PdhAddEnglishCounterW(
            query: isize,
            path: *const u16,
            user: usize,
            counter: *mut isize,
        ) -> u32;
        fn PdhCollectQueryData(query: isize) -> u32;
        fn PdhCloseQuery(query: isize) -> u32;
        fn PdhGetFormattedCounterArrayW(
            counter: isize,
            format: u32,
            size: *mut u32,
            count: *mut u32,
            buf: *mut c_void,
        ) -> u32;
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(
            key: isize,
            sub: *const u16,
            options: u32,
            sam: u32,
            result: *mut isize,
        ) -> u32;
        fn RegEnumKeyExW(
            key: isize,
            index: u32,
            name: *mut u16,
            name_len: *mut u32,
            reserved: *mut u32,
            class: *mut u16,
            class_len: *mut u32,
            time: *mut c_void,
        ) -> u32;
        fn RegGetValueW(
            key: isize,
            sub: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut c_void,
            size: *mut u32,
        ) -> u32;
        fn RegCloseKey(key: isize) -> u32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct Adapter {
        luid: String,
        handle: u32,
        name: String,
        vendor: Option<String>,
        integrated: bool,
        dedicated_gb: Option<f64>,
        shared_gb: Option<f64>,
        driver: Option<String>,
        temp_max: Option<f64>,
        has_fan: bool,
    }

    /// The adapters and the performance counter query (`WindowsGpu`); `Drop` closes both.
    pub struct WindowsGpu {
        adapters: Vec<Adapter>,
        enumerated_at: Option<Instant>,
        query: isize,
        engine: isize,
        dedicated: isize,
        shared: isize,
        primed: bool,
    }

    // SAFETY: the PDH and D3DKMT handles are plain numbers owned by this value and used from one thread at a time.
    unsafe impl Send for WindowsGpu {}

    impl Default for WindowsGpu {
        fn default() -> Self {
            Self::new()
        }
    }

    impl WindowsGpu {
        pub fn new() -> WindowsGpu {
            let mut g = WindowsGpu {
                adapters: Vec::new(),
                enumerated_at: None,
                query: 0,
                engine: 0,
                dedicated: 0,
                shared: 0,
                primed: false,
            };
            // SAFETY: out-pointers to owned isize handles; the counter paths are NUL-terminated wide strings.
            unsafe {
                if PdhOpenQueryW(std::ptr::null(), 0, &mut g.query) != 0 {
                    g.query = 0;
                    return g;
                }
                // English counter names: the localized ones differ per Windows language
                for (path, slot) in [
                    (r"\GPU Engine(*)\Utilization Percentage", &mut g.engine),
                    (r"\GPU Adapter Memory(*)\Dedicated Usage", &mut g.dedicated),
                    (r"\GPU Adapter Memory(*)\Shared Usage", &mut g.shared),
                ] {
                    if PdhAddEnglishCounterW(g.query, wide(path).as_ptr(), 0, slot) != 0 {
                        *slot = 0;
                    }
                }
            }
            g
        }

        /// One reading of every hardware adapter (software and indirect-display adapters are left out).
        pub fn sample(&mut self) -> Result<Vec<GpuInfo>, String> {
            // the adapter set rarely changes (driver update, eGPU); re-read it every five minutes
            if self
                .enumerated_at
                .is_none_or(|at| at.elapsed() > Duration::from_secs(300))
            {
                self.enumerate();
            }
            let (mut util, mut dedicated, mut shared) =
                (HashMap::new(), HashMap::new(), HashMap::new());
            // SAFETY: `query` is an open PDH query.
            if self.query != 0 && unsafe { PdhCollectQueryData(self.query) } == 0 && self.primed {
                util = engine_load(&read(self.engine));
                dedicated = memory_by_adapter(&read(self.dedicated));
                shared = memory_by_adapter(&read(self.shared));
            }
            self.primed = true; // the first collection of a rate counter has nothing to compare with
            Ok(self
                .adapters
                .iter()
                .map(|a| {
                    let perf = perf_data(a.handle);
                    GpuInfo {
                        name: a.name.clone(),
                        util: util.get(&a.luid).map(|u| round_dotnet(*u, 1)),
                        temp: perf.as_ref().and_then(|p| p.temp),
                        vram_used: dedicated.get(&a.luid).map(|d| round_dotnet(d / GIB, 2)),
                        vram_total: a.dedicated_gb,
                        vendor: a.vendor.clone(),
                        source: Some("windows".into()),
                        integrated: a.integrated,
                        mem_clock: perf.as_ref().and_then(|p| p.mem_clock),
                        shared_used: shared.get(&a.luid).map(|s| round_dotnet(s / GIB, 2)),
                        shared_total: a.shared_gb,
                        power_pct: perf.as_ref().and_then(|p| p.power_pct),
                        fan_rpm: if a.has_fan {
                            perf.as_ref().and_then(|p| p.fan_rpm)
                        } else {
                            None
                        },
                        temp_max: a.temp_max,
                        driver: a.driver.clone(),
                        ..Default::default()
                    }
                })
                .collect())
        }

        fn enumerate(&mut self) {
            self.enumerated_at = Some(Instant::now());
            self.close_adapters();
            let drivers = driver_versions();
            let mut buf = [AdapterInfo::default(); 32];
            let mut en = EnumAdapters2 {
                num_adapters: buf.len() as u32,
                p_adapters: buf.as_mut_ptr(),
            };
            // SAFETY: `en` points at `buf`, which holds `num_adapters` entries.
            if unsafe { D3DKMTEnumAdapters2(&mut en) } != 0 {
                return;
            }
            let count = en.num_adapters.min(buf.len() as u32) as usize;
            for a in &buf[..count] {
                let mut type_buf = [0u8; 4];
                query(a.h_adapter, 15, &mut type_buf); // ADAPTERTYPE
                let kind = u32::from_le_bytes(type_buf);
                let mut ids = [0u8; 28];
                query(a.h_adapter, 31, &mut ids); // PHYSICALADAPTERDEVICEIDS
                let vendor_id = u32::from_le_bytes([ids[4], ids[5], ids[6], ids[7]]);
                let software = kind & (1 << 2) != 0;
                let indirect = kind & (1 << 6) != 0;
                let renders = kind & 1 != 0;
                if software || indirect || !renders || vendor_id == 0x1414 {
                    close(a.h_adapter); // Microsoft Basic Render and the like
                    continue;
                }
                let mut name = "GPU".to_string();
                let mut reg = vec![0u8; 4 * 260 * 2];
                if query(a.h_adapter, 8, &mut reg) == 0 {
                    // ADAPTERREGISTRYINFO: the adapter string is the first 260 UTF-16 chars
                    let chars: Vec<u16> = reg[..520]
                        .chunks(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    let end = chars.iter().position(|c| *c == 0).unwrap_or(chars.len());
                    let n = String::from_utf16_lossy(&chars[..end]).trim().to_string();
                    if !n.is_empty() {
                        name = n;
                    }
                }
                let mut seg = [0u8; 24];
                let seg_ok = query(a.h_adapter, 3, &mut seg) == 0; // GETSEGMENTSIZE
                let dedicated = u64::from_le_bytes(seg[0..8].try_into().expect("8 bytes"));
                let shared = u64::from_le_bytes(seg[16..24].try_into().expect("8 bytes"));
                let mut caps = [0u8; 40];
                let caps_ok = query(a.h_adapter, 63, &mut caps) == 0; // ADAPTERPERFDATA_CAPS
                let max_fan = u32::from_le_bytes([caps[24], caps[25], caps[26], caps[27]]);
                let temp_max =
                    f64::from(u32::from_le_bytes([caps[28], caps[29], caps[30], caps[31]])) / 10.0;
                let hybrid_integrated = kind & (1 << 5) != 0;
                self.adapters.push(Adapter {
                    luid: format!("{:08x}_{:08x}", a.luid_high as u32, a.luid_low),
                    handle: a.h_adapter,
                    vendor: vendor_of_pci(vendor_id).or_else(|| vendor_of_name(&name)),
                    integrated: hybrid_integrated || looks_integrated(&name),
                    dedicated_gb: (seg_ok && dedicated > 0)
                        .then(|| round_dotnet(dedicated as f64 / GIB, 2)),
                    shared_gb: (seg_ok && shared > 0).then(|| round_dotnet(shared as f64 / GIB, 2)),
                    driver: drivers.get(&name.to_lowercase()).cloned(),
                    temp_max: (caps_ok && temp_max > 0.0).then_some(temp_max),
                    has_fan: caps_ok && max_fan > 0,
                    name,
                });
            }
        }

        fn close_adapters(&mut self) {
            for a in self.adapters.drain(..) {
                close(a.handle);
            }
        }
    }

    impl Drop for WindowsGpu {
        fn drop(&mut self) {
            self.close_adapters();
            if self.query != 0 {
                // SAFETY: the query was opened by PdhOpenQueryW and is closed once.
                unsafe { PdhCloseQuery(self.query) };
            }
            self.query = 0;
        }
    }

    fn perf_data(handle: u32) -> Option<super::Perf> {
        let mut buf = [0u8; 64];
        (query(handle, 62, &mut buf) == 0).then(|| parse_perf(&buf)) // ADAPTERPERFDATA
    }

    fn query(handle: u32, kind: i32, data: &mut [u8]) -> u32 {
        let mut q = QueryAdapterInfo {
            h_adapter: handle,
            kind,
            p_private_driver_data: data.as_mut_ptr().cast(),
            private_driver_data_size: data.len() as u32,
        };
        // SAFETY: `q` points at `data` with its exact length.
        unsafe { D3DKMTQueryAdapterInfo(&mut q) }
    }

    fn close(handle: u32) {
        let mut h = handle;
        // SAFETY: a handle D3DKMTEnumAdapters2 returned, closed once.
        unsafe { D3DKMTCloseAdapter(&mut h) };
    }

    /// Formatted counter values by instance name; instances of processes that just started have no valid value yet.
    fn read(counter: isize) -> Vec<(String, f64)> {
        let mut list = Vec::new();
        if counter == 0 {
            return list;
        }
        let (mut size, mut count) = (0u32, 0u32);
        let format = PDH_FMT_DOUBLE | PDH_FMT_NOCAP100;
        // SAFETY: a size query with a null buffer, as PDH documents.
        if unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                format,
                &mut size,
                &mut count,
                std::ptr::null_mut(),
            )
        } != PDH_MORE_DATA
            || size == 0
        {
            return list;
        }
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: `buf` holds `size` bytes, 8-byte aligned for the PDH_FMT_COUNTERVALUE_ITEM_W array.
        if unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                format,
                &mut size,
                &mut count,
                buf.as_mut_ptr().cast(),
            )
        } != 0
        {
            return list;
        }
        let items = buf.as_ptr() as *const FmtItem;
        for i in 0..count as usize {
            // SAFETY: PDH wrote `count` items at the start of `buf`; their names point into the same buffer.
            let item = unsafe { &*items.add(i) };
            if !(item.c_status == 0 || item.c_status == 1) || item.name.is_null() {
                continue;
            }
            let mut len = 0;
            while unsafe { *item.name.add(len) } != 0 {
                len += 1;
            }
            let name =
                String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(item.name, len) });
            list.push((name, item.value));
        }
        list
    }

    /// The installed driver version per adapter name (lower case), from the display class key.
    fn driver_versions() -> HashMap<String, String> {
        const HKLM: isize = 0x8000_0002_u32 as i32 as isize;
        const KEY_READ: u32 = 0x2_0019;
        const RRF_RT_REG_SZ: u32 = 0x2;
        let mut map = HashMap::new();
        let class =
            wide(r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}");
        let mut key = 0isize;
        // SAFETY: valid wide path and out-pointer.
        if unsafe { RegOpenKeyExW(HKLM, class.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
            return map;
        }
        let value = |sub: &[u16], name: &str| -> Option<String> {
            let mut buf = [0u16; 512];
            let mut size = (buf.len() * 2) as u32;
            // SAFETY: `buf` holds `size` bytes; the names are NUL-terminated wide strings.
            let rc = unsafe {
                RegGetValueW(
                    key,
                    sub.as_ptr(),
                    wide(name).as_ptr(),
                    RRF_RT_REG_SZ,
                    std::ptr::null_mut(),
                    buf.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
            (rc == 0).then(|| String::from_utf16_lossy(&buf[..end]))
        };
        for index in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            // SAFETY: `name` holds `len` chars; the other out-parameters are unused (null).
            let rc = unsafe {
                RegEnumKeyExW(
                    key,
                    index,
                    name.as_mut_ptr(),
                    &mut len,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc != 0 {
                break;
            }
            let sub = String::from_utf16_lossy(&name[..len as usize]);
            if sub.len() != 4 || !sub.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let sub = wide(&sub);
            if let (Some(desc), Some(ver)) =
                (value(&sub, "DriverDesc"), value(&sub, "DriverVersion"))
            {
                map.entry(desc.trim().to_lowercase()).or_insert(ver);
            }
        }
        // SAFETY: the key opened above, closed once.
        unsafe { RegCloseKey(key) };
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_parse_like_csharp() {
        assert_eq!(
            parse_instance("pid_1234_luid_0x00000000_0x0001C621_phys_0_eng_3_engtype_3D"),
            Some(Instance {
                luid: "00000000_0001c621".into(),
                phys: "0".into(),
                eng: Some("3".into())
            })
        );
        assert_eq!(
            parse_instance("luid_0x00000000_0x0001C621_phys_0"),
            Some(Instance {
                luid: "00000000_0001c621".into(),
                phys: "0".into(),
                eng: None
            })
        );
        assert_eq!(parse_instance("_Total"), None);
    }

    #[test]
    fn the_adapter_load_is_its_busiest_engine_summed_over_processes() {
        let r = |n: &str, v: f64| (n.to_string(), v);
        let load = engine_load(&[
            r(
                "pid_1_luid_0x00000000_0x0001C621_phys_0_eng_0_engtype_3D",
                30.0,
            ),
            r(
                "pid_2_luid_0x00000000_0x0001C621_phys_0_eng_0_engtype_3D",
                25.0,
            ),
            r(
                "pid_1_luid_0x00000000_0x0001C621_phys_0_eng_3_engtype_VideoDecode",
                40.0,
            ),
            r(
                "pid_3_luid_0x00000000_0x0000AAAA_phys_0_eng_0_engtype_3D",
                70.0,
            ),
            r(
                "pid_4_luid_0x00000000_0x0000AAAA_phys_0_eng_0_engtype_3D",
                60.0,
            ),
            r("not an instance", 99.0),
        ]);
        assert_eq!(load["00000000_0001c621"], 55.0);
        assert_eq!(load["00000000_0000aaaa"], 100.0); // capped
        assert_eq!(load.len(), 2);
        let mem = memory_by_adapter(&[
            r("luid_0x00000000_0x0001C621_phys_0", 100.0),
            r("luid_0x00000000_0x0001C621_phys_1", 50.0),
        ]);
        assert_eq!(mem["00000000_0001c621"], 150.0);
    }

    #[test]
    fn perf_data_reads_tenths_and_leaves_out_of_range_values() {
        let mut buf = [0u8; 64];
        buf[8..16].copy_from_slice(&7_001_000_000u64.to_le_bytes());
        buf[48..52].copy_from_slice(&1450u32.to_le_bytes());
        buf[52..56].copy_from_slice(&1503u32.to_le_bytes());
        buf[56..60].copy_from_slice(&655u32.to_le_bytes());
        assert_eq!(
            parse_perf(&buf),
            Perf {
                temp: Some(65.5),
                mem_clock: Some(7001.0),
                power_pct: Some(150.3),
                fan_rpm: Some(1450.0)
            }
        );
        buf[52..56].copy_from_slice(&2001u32.to_le_bytes());
        buf[56..60].copy_from_slice(&2000u32.to_le_bytes());
        buf[8..16].copy_from_slice(&0u64.to_le_bytes());
        let p = parse_perf(&buf);
        assert_eq!((p.temp, p.mem_clock, p.power_pct), (None, None, None));
    }
}
