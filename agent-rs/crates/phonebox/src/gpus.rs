//! Port of `Gpus.cs`: GPU sources (nvidia-smi, LibreHardwareMonitor, Linux sysfs) and how they combine.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::drm_usage::DrmUsage;
use crate::system_monitor::round_dotnet;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// One GPU (`GpuInfo`). Units: load and fan duty in %, temperatures in °C, power in W, memory in GB, clocks in MHz.
/// `power_pct` is the board power as a share of its limit; `shared_*` is system memory the GPU uses.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuInfo {
    pub name: String,
    pub util: Option<f64>,
    pub temp: Option<f64>,
    pub power: Option<f64>,
    pub power_limit: Option<f64>,
    pub vram_used: Option<f64>,
    pub vram_total: Option<f64>,
    pub fan: Option<f64>,
    pub clock: Option<f64>,
    pub vendor: Option<String>,
    pub source: Option<String>,
    pub integrated: bool,
    pub hot_spot: Option<f64>,
    pub mem_temp: Option<f64>,
    pub mem_clock: Option<f64>,
    pub shared_used: Option<f64>,
    pub shared_total: Option<f64>,
    pub power_pct: Option<f64>,
    pub fan_rpm: Option<f64>,
    pub temp_max: Option<f64>,
    pub driver: Option<String>,
}

/// Merges the sources (highest priority first) and puts the main GPU first: discrete before integrated, then the most
/// video memory. The same card from several sources is matched by name; each entry is matched once per source.
pub fn compose(sources: &[Option<&[GpuInfo]>]) -> Vec<GpuInfo> {
    let mut list: Vec<GpuInfo> = Vec::new();
    for src in sources.iter().flatten() {
        let mut taken: Vec<bool> = vec![false; list.len()];
        for g in src.iter() {
            let k = key(&g.name);
            match (0..list.len()).find(|&j| !taken[j] && key(&list[j].name) == k) {
                None => {
                    list.push(g.clone());
                    taken.push(true);
                }
                Some(i) => {
                    list[i] = merge(&list[i], g);
                    taken[i] = true;
                }
            }
        }
    }
    let mut indexed: Vec<(usize, GpuInfo)> = list.into_iter().enumerate().collect();
    indexed.sort_by(|(ia, a), (ib, b)| {
        a.integrated
            .cmp(&b.integrated)
            .then(
                b.vram_total
                    .unwrap_or(0.0)
                    .total_cmp(&a.vram_total.unwrap_or(0.0)),
            )
            .then(ia.cmp(ib))
    });
    indexed.into_iter().map(|(_, g)| g).collect()
}

/// Fields of `a` win; `b` fills the gaps. One exception: the Windows adapter list (merged last) marking a GPU
/// integrated is kept, since Windows knows a hybrid laptop's integrated GPU that the name heuristic can miss.
pub fn merge(a: &GpuInfo, b: &GpuInfo) -> GpuInfo {
    let source = match (&a.source, &b.source) {
        (None, s) => s.clone(),
        (Some(sa), None) => Some(sa.clone()),
        (Some(sa), Some(sb)) if sa.contains(sb.as_str()) => Some(sa.clone()),
        (Some(sa), Some(sb)) => Some(format!("{sa}+{sb}")),
    };
    GpuInfo {
        name: a.name.clone(),
        util: a.util.or(b.util),
        temp: a.temp.or(b.temp),
        power: a.power.or(b.power),
        power_limit: a.power_limit.or(b.power_limit),
        vram_used: a.vram_used.or(b.vram_used),
        vram_total: a.vram_total.or(b.vram_total),
        fan: a.fan.or(b.fan),
        clock: a.clock.or(b.clock),
        vendor: a.vendor.clone().or_else(|| b.vendor.clone()),
        source,
        integrated: a.integrated || (b.integrated && b.source.as_deref() == Some("windows")),
        hot_spot: a.hot_spot.or(b.hot_spot),
        mem_temp: a.mem_temp.or(b.mem_temp),
        mem_clock: a.mem_clock.or(b.mem_clock),
        shared_used: a.shared_used.or(b.shared_used),
        shared_total: a.shared_total.or(b.shared_total),
        power_pct: a.power_pct.or(b.power_pct),
        fan_rpm: a.fan_rpm.or(b.fan_rpm),
        temp_max: a.temp_max.or(b.temp_max),
        driver: a.driver.clone().or_else(|| b.driver.clone()),
    }
}

/// "AMD Radeon(TM) Graphics" and "AMD Radeon Graphics" are the same card.
pub fn key(name: &str) -> String {
    name.to_lowercase()
        .replace("(tm)", "")
        .replace("(r)", "")
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

fn contains_ci(s: &str, needle: &str) -> bool {
    s.to_lowercase().contains(needle)
}

pub fn vendor_of_name(name: &str) -> Option<String> {
    let v = if contains_ci(name, "nvidia") || contains_ci(name, "geforce") {
        "nvidia"
    } else if contains_ci(name, "amd") || contains_ci(name, "radeon") {
        "amd"
    } else if contains_ci(name, "intel") {
        "intel"
    } else {
        return None;
    };
    Some(v.into())
}

pub fn vendor_of_pci(pci_vendor: u32) -> Option<String> {
    let v = match pci_vendor {
        0x10de => "nvidia",
        0x1002 | 0x1022 => "amd",
        0x8086 => "intel",
        0x106b => "apple",
        0x5143 => "qualcomm",
        _ => return None,
    };
    Some(v.into())
}

static INTEL_DISCRETE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Arc(?:\(TM\)|™)?\s+(?:Pro\s+)?[AB]\d{2,3}").expect("IntelDiscrete")
});
static AMD_DISCRETE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bRX\s+\d{3,4}|\bPro\s+W\d|\bW\d{4}").expect("AmdDiscrete"));
static CARD_DIR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^card\d+$").expect("CardDir"));
static DPM_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\d+:\s*(\d+)\s*[Mm][Hh]z").expect("DpmLine"));
static BRACKET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]").expect("Bracket"));
static PCI_SLOT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9a-fA-F]{4,}:[0-9a-fA-F]{2}:[0-9a-fA-F]{2}\.[0-7]$").expect("PciSlot")
});

/// Integrated graphics by name: AMD APUs are "Radeon(TM) Graphics", "Radeon 780M Graphics", "Radeon RX Vega 11
/// Graphics"; Intel's are everything but the Arc A/B cards.
pub fn looks_integrated(name: &str) -> bool {
    if contains_ci(name, "intel") {
        return !INTEL_DISCRETE.is_match(name);
    }
    if contains_ci(name, "radeon") || name.to_lowercase().starts_with("amd") {
        return name.trim_end().to_lowercase().ends_with("graphics")
            && !AMD_DISCRETE.is_match(name);
    }
    false
}

// ---------------------------------------------------------------- nvidia-smi

/// One line of `--query-gpu=index,utilization.gpu,temperature.gpu,power.draw,power.limit,memory.used,memory.total,
/// fan.speed,clocks.gr,clocks.mem`.
pub fn parse_nvidia_line(
    line: &str,
    names: &HashMap<i32, (String, Option<String>)>,
) -> Option<(i32, GpuInfo)> {
    let f: Vec<Option<f64>> = line
        .split(',')
        .map(|s| s.trim().parse::<f64>().ok().filter(|v| v.is_finite()))
        .collect();
    if f.len() < 9 {
        return None;
    }
    let idx = f[0].filter(|i| *i >= 0.0)? as i32;
    let (name, driver) = names
        .get(&idx)
        .cloned()
        .unwrap_or_else(|| ("NVIDIA GPU".into(), None));
    Some((
        idx,
        GpuInfo {
            name,
            util: f[1],
            temp: f[2],
            power: f[3],
            power_limit: f[4],
            vram_used: f[5].map(|v| v / 1024.0),
            vram_total: f[6].map(|v| v / 1024.0),
            fan: f[7],
            clock: f[8],
            vendor: Some("nvidia".into()),
            source: Some("nvidia-smi".into()),
            mem_clock: f.get(9).copied().flatten(),
            driver,
            ..Default::default()
        },
    ))
}

/// The `--query-gpu=index,name,driver_version` answer (a name may hold commas).
pub fn parse_nvidia_names(text: &str) -> HashMap<i32, (String, Option<String>)> {
    let mut map = HashMap::new();
    for line in text.split('\n') {
        let Some(first) = line.find(',').filter(|f| *f > 0) else {
            continue;
        };
        let Ok(i) = line[..first].trim().parse::<i32>() else {
            continue;
        };
        let last = line.rfind(',').unwrap_or(first);
        let name = if last > first {
            &line[first + 1..last]
        } else {
            &line[first + 1..]
        }
        .trim();
        if name.is_empty() {
            continue;
        }
        let drv = if last > first {
            line[last + 1..].trim()
        } else {
            ""
        };
        let driver = (!drv.is_empty() && drv != "[N/A]").then(|| drv.to_string());
        map.insert(i, (name.to_string(), driver));
    }
    map
}

// ---------------------------------------------------------------- LibreHardwareMonitor (web server, /data.json)

fn str_of<'a>(node: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    node.get(key).and_then(|v| v.as_str())
}

/// The GPUs in LibreHardwareMonitor's tree, recognised by HardwareId or, on older versions, by their icon.
pub fn parse_lhm(root: &serde_json::Value) -> Vec<GpuInfo> {
    fn walk(node: &serde_json::Value, list: &mut Vec<GpuInfo>) {
        if !node.is_object() {
            return;
        }
        if let Some(vendor) = lhm_gpu_vendor(node)
            && let Some(name) = str_of(node, "Text").filter(|n| !n.is_empty())
        {
            list.push(lhm_gpu(node, name, &vendor));
            return;
        }
        if let Some(kids) = node.get("Children").and_then(|k| k.as_array()) {
            for k in kids {
                walk(k, list);
            }
        }
    }
    let mut list = Vec::new();
    walk(root, &mut list);
    list
}

fn lhm_gpu_vendor(node: &serde_json::Value) -> Option<String> {
    if let Some(id) = str_of(node, "HardwareId") {
        return if id.starts_with("/gpu-nvidia") {
            Some("nvidia".into())
        } else if id.starts_with("/gpu-amd") || id.starts_with("/atigpu") {
            Some("amd".into())
        } else if id.starts_with("/gpu-intel") {
            Some("intel".into())
        } else if id.starts_with("/gpu") {
            Some(vendor_of_name(str_of(node, "Text").unwrap_or("")).unwrap_or_else(|| "gpu".into()))
        } else {
            None
        };
    }
    if str_of(node, "SensorId").is_some() {
        return None;
    }
    match str_of(node, "ImageURL") {
        Some(u) if u.ends_with("/nvidia.png") => Some("nvidia".into()),
        Some(u) if u.ends_with("/ati.png") => Some("amd".into()),
        Some(u) if u.ends_with("/intel.png") => Some("intel".into()),
        _ => None,
    }
}

fn lhm_gpu(hw: &serde_json::Value, name: &str, vendor: &str) -> GpuInfo {
    fn collect(
        node: &serde_json::Value,
        group: Option<&str>,
        sensors: &mut Vec<(String, &'static str, f64)>,
    ) {
        if let Some(kids) = node
            .get("Children")
            .and_then(|k| k.as_array())
            .filter(|k| !k.is_empty())
        {
            let g = str_of(node, "Text");
            for k in kids {
                collect(k, g, sensors);
            }
            return;
        }
        let (Some(text), Some(value)) = (str_of(node, "Text"), str_of(node, "Value")) else {
            return;
        };
        if let Some((kind, v)) = parse_lhm_value(value, str_of(node, "Type").or(group)) {
            sensors.push((text.to_string(), kind, v));
        }
    }
    let mut sensors = Vec::new();
    collect(hw, None, &mut sensors);
    let get = |kind: &str, names: &[&str]| -> Option<f64> {
        names.iter().find_map(|n| {
            sensors
                .iter()
                .find(|s| s.1 == kind && s.0.eq_ignore_ascii_case(n))
                .map(|s| s.2)
        })
    };
    let fan_rpm = sensors
        .iter()
        .find(|s| s.1 == "rpm" && s.0.to_lowercase().starts_with("gpu fan"))
        .map(|s| s.2);
    let integrated = str_of(hw, "HardwareId").is_some_and(|id| id.contains("integrated"))
        || looks_integrated(name);
    GpuInfo {
        name: name.to_string(),
        util: get("load", &["GPU Core"]),
        temp: get("temp", &["GPU Core"]),
        power: get(
            "power",
            &[
                "GPU Package",
                "GPU Total",
                "GPU Power",
                "GPU PPT",
                "GPU Core",
            ],
        ),
        power_limit: None,
        vram_used: get("data", &["GPU Memory Used", "D3D Dedicated Memory Used"]),
        vram_total: get("data", &["GPU Memory Total", "D3D Dedicated Memory Total"]),
        fan: get("control", &["GPU Fan"]),
        clock: get("clock", &["GPU Core"]),
        vendor: Some(vendor.to_string()),
        source: Some("lhm".into()),
        integrated,
        hot_spot: get("temp", &["GPU Hot Spot"]),
        mem_temp: get("temp", &["GPU Memory"]),
        mem_clock: get("clock", &["GPU Memory"]),
        shared_used: get("data", &["D3D Shared Memory Used"]),
        shared_total: get("data", &["D3D Shared Memory Total"]),
        fan_rpm,
        ..Default::default()
    }
}

// ---------------------------------------------------------------- Linux sysfs (amdgpu, i915 / xe) and DRM fdinfo

/// Keeps the previous fdinfo sample between calls, for the Intel load.
static INTEL_USAGE: LazyLock<DrmUsage> = LazyLock::new(DrmUsage::new);

/// AMD and Intel GPUs from `/sys/class/drm/card*/device`: amdgpu reports load, VRAM and GTT memory, clocks and its hwmon
/// sensors; Intel's i915 and xe report the clock and, on Arc, a temperature, and their load is added up from the DRM
/// fdinfo of the processes in `proc_root` (see `DrmUsage`), so it shows from the second call on. NVIDIA cards are left
/// to nvidia-smi.
pub fn read_sysfs(
    drm_root: &Path,
    pci_ids: Option<&Path>,
    proc_root: &Path,
    usage: Option<&DrmUsage>,
) -> Vec<GpuInfo> {
    let mut list: Vec<GpuInfo> = Vec::new();
    let Ok(entries) = std::fs::read_dir(drm_root) else {
        return list;
    };
    let mut cards: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| CARD_DIR.is_match(n))
        })
        .collect();
    cards.sort();
    let mut intel_load: Vec<(usize, String)> = Vec::new(); // Intel cards without gpu_busy_percent
    for card in cards {
        let dev = card.join("device");
        let Some(vendor_id) = hex(read(Some(&dev), "vendor").as_deref()) else {
            continue;
        };
        let vendor = vendor_of_pci(vendor_id);
        let vendor = match vendor.as_deref() {
            Some(v @ ("amd" | "intel")) => v.to_string(),
            _ => continue,
        };
        let device_id = hex(read(Some(&dev), "device").as_deref()).unwrap_or(0);
        let hw = std::fs::read_dir(dev.join("hwmon")).ok().and_then(|d| {
            let mut dirs: Vec<std::path::PathBuf> = d
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            dirs.into_iter().next()
        });
        let hw = hw.as_deref();
        let mut name = match read(Some(&dev), "product_name").filter(|n| !n.is_empty()) {
            Some(pn) => pn,
            None => pci_name(vendor_id, device_id, pci_ids).unwrap_or_else(|| {
                if vendor == "amd" {
                    "AMD GPU"
                } else {
                    "Intel GPU"
                }
                .into()
            }),
        };
        if vendor == "amd" && !contains_ci(&name, "amd") {
            name = format!("AMD {name}");
        }
        if vendor == "intel" && !contains_ci(&name, "intel") {
            name = format!("Intel {name}");
        }

        let vram_total = num(Some(&dev), "mem_info_vram_total");
        let temps = hwmon_temps(hw);
        let temp = |label: &str, index: i32| {
            temps
                .get(label)
                .or_else(|| temps.get(&format!("temp{index}")))
                .copied()
        };
        let power = num(hw, "power1_average").or_else(|| num(hw, "power1_input"));
        let cap = num(hw, "power1_cap");
        let fan_max = num(hw, "fan1_max");
        let fan_rpm = num(hw, "fan1_input");
        let clock = if vendor == "amd" {
            dpm_current(read(Some(&dev), "pp_dpm_sclk").as_deref())
        } else {
            num(Some(&card), "gt_cur_freq_mhz")
                .or_else(|| num(Some(&dev), "tile0/gt0/freq0/cur_freq"))
        };
        let util = num(Some(&dev), "gpu_busy_percent");
        // an APU's VRAM is a small carve-out of the RAM; Intel is integrated unless it is an Arc card
        let integrated = if vendor == "amd" {
            vram_total.is_some_and(|v| v < 4.0 * GIB) && !AMD_DISCRETE.is_match(&name)
        } else {
            !INTEL_DISCRETE.is_match(&name) && !name.contains("DG2")
        };
        list.push(GpuInfo {
            util,
            temp: temp("edge", 1),
            power: power.map(|p| round_dotnet(p / 1e6, 1)),
            power_limit: cap.filter(|c| *c > 0.0).map(|c| round_dotnet(c / 1e6, 0)),
            vram_used: num(Some(&dev), "mem_info_vram_used").map(|v| v / GIB),
            vram_total: vram_total.map(|v| v / GIB),
            fan: match (fan_rpm, fan_max) {
                (Some(r), Some(mx)) if mx > 0.0 => Some(round_dotnet(100.0 * r / mx, 0)),
                _ => None,
            },
            clock,
            vendor: Some(vendor.clone()),
            source: Some("sysfs".into()),
            integrated,
            hot_spot: temps.get("junction").copied(),
            mem_temp: temps.get("mem").copied(),
            mem_clock: if vendor == "amd" {
                dpm_current(read(Some(&dev), "pp_dpm_mclk").as_deref())
            } else {
                None
            },
            shared_used: num(Some(&dev), "mem_info_gtt_used").map(|v| v / GIB),
            shared_total: num(Some(&dev), "mem_info_gtt_total").map(|v| v / GIB),
            fan_rpm,
            temp_max: num(hw, "temp1_crit")
                .filter(|t| *t > 0.0)
                .map(|t| t / 1000.0),
            name,
            ..Default::default()
        });
        if util.is_none()
            && vendor == "intel"
            && let Some(pdev) = pci_address(&dev)
        {
            intel_load.push((list.len() - 1, pdev));
        }
    }
    if !intel_load.is_empty() {
        let pdevs: Vec<String> = intel_load.iter().map(|c| c.1.clone()).collect();
        let load = usage.unwrap_or(&INTEL_USAGE).sample(proc_root, &pdevs);
        for (i, pdev) in intel_load {
            if let Some(u) = load.get(&pdev) {
                list[i].util = Some(*u);
            }
        }
    }
    list
}

/// The card's PCI address ("0000:00:02.0"), as DRM fdinfo names it in `drm-pdev`: from the device's uevent, or the name
/// the `device` link points to.
pub fn pci_address(dev: &Path) -> Option<String> {
    for line in read(Some(dev), "uevent").unwrap_or_default().split('\n') {
        if let Some(slot) = line.strip_prefix("PCI_SLOT_NAME=").map(str::trim)
            && PCI_SLOT.is_match(slot)
        {
            return Some(slot.to_lowercase());
        }
    }
    let target = std::fs::read_link(dev).ok()?;
    let name = target
        .to_string_lossy()
        .trim_end_matches('/')
        .rsplit('/')
        .next()?
        .to_string();
    PCI_SLOT.is_match(&name).then(|| name.to_lowercase())
}

fn hwmon_temps(hw: Option<&Path>) -> HashMap<String, f64> {
    let mut temps = HashMap::new();
    let Some(hw) = hw else { return temps };
    for i in 1..=8 {
        let Some(milli) = num(Some(hw), &format!("temp{i}_input")) else {
            continue;
        };
        let t = round_dotnet(milli / 1000.0, 1);
        let label = read(Some(hw), &format!("temp{i}_label"))
            .map(|l| l.to_lowercase())
            .unwrap_or(format!("temp{i}"));
        temps.insert(label, t);
        temps.entry(format!("temp{i}")).or_insert(t);
    }
    temps
}

/// "0: 500Mhz\n1: 2482Mhz *" → 2482.
pub fn dpm_current(text: Option<&str>) -> Option<f64> {
    text?.split('\n').find_map(|line| {
        if !line.trim_end().ends_with('*') {
            return None;
        }
        DPM_LINE.captures(line)?[1].parse().ok()
    })
}

/// Looks a device up in pci.ids (once per card: the file is over a megabyte).
pub fn pci_name(vendor: u32, device: u32, pci_ids: Option<&Path>) -> Option<String> {
    use std::sync::Mutex;
    /// (pci.ids path, vendor, device) → the name found, or None.
    type NameCache = HashMap<(std::path::PathBuf, u32, u32), Option<String>>;
    static CACHE: LazyLock<Mutex<NameCache>> = LazyLock::new(Default::default);
    let path = match pci_ids {
        Some(p) => p.to_path_buf(),
        None => [
            "/usr/share/hwdata/pci.ids",
            "/usr/share/misc/pci.ids",
            "/usr/share/pci.ids",
        ]
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.is_file())?,
    };
    if !path.is_file() {
        return None;
    }
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry((path.clone(), vendor, device))
        .or_insert_with(|| look_up_pci_name(vendor, device, &path))
        .clone()
}

/// The vendor line, then a tab-indented device line under it.
fn look_up_pci_name(vendor: u32, device: u32, pci_ids: &Path) -> Option<String> {
    use std::io::BufRead;
    let v = format!("{vendor:04x}");
    let d = format!("{device:04x}");
    let file = std::fs::File::open(pci_ids).ok()?;
    let mut in_vendor = false;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with('\t') {
            if in_vendor {
                return None;
            }
            in_vendor =
                line.len() >= 6 && line[..4].eq_ignore_ascii_case(&v) && &line[4..6] == "  ";
            continue;
        }
        let b = line.as_bytes();
        if in_vendor
            && b.len() > 6
            && b[1] != b'\t'
            && line.get(1..5).is_some_and(|s| s.eq_ignore_ascii_case(&d))
        {
            let name = line[5..].trim();
            // "Navi 31 [Radeon RX 7900 XT/7900 XTX/7900 GRE/7900M]": the marketing name in brackets reads better
            return Some(match BRACKET.captures(name) {
                Some(br) => br[1].to_string(),
                None => name.to_string(),
            });
        }
    }
    None
}

fn read(dir: Option<&Path>, file: &str) -> Option<String> {
    let p = dir?.join(file);
    if !p.is_file() {
        return None;
    }
    std::fs::read(&p)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
}

fn num(dir: Option<&Path>, file: &str) -> Option<f64> {
    read(dir, file)?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

fn hex(s: Option<&str>) -> Option<u32> {
    let s = s?;
    let digits = if s.len() >= 2 && s[..2].eq_ignore_ascii_case("0x") {
        &s[2..]
    } else {
        s
    };
    u32::from_str_radix(digits.trim(), 16).ok()
}

static LHM_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(-?\d+(?:[.,]\d+)?)\s*(°C|°F|%|MHz|GHz|W|RPM|B|KB|MB|GB|TB)\s*$")
        .expect("LhmValue")
});

/// "45,0 °C" → ("temp", 45). Data values come back in GB; % values are load unless the sensor is a control (fan duty).
pub fn parse_lhm_value(value: &str, type_or_group: Option<&str>) -> Option<(&'static str, f64)> {
    let m = LHM_VALUE.captures(value)?;
    let v: f64 = m[1].replace(',', ".").parse().ok()?;
    let control = matches!(type_or_group, Some("Control" | "Controls"));
    Some(match &m[2] {
        "°C" => ("temp", v),
        "°F" => ("temp", round_dotnet((v - 32.0) / 1.8, 1)),
        "%" => (if control { "control" } else { "load" }, v),
        "MHz" => ("clock", v),
        "GHz" => ("clock", v * 1000.0),
        "W" => ("power", v),
        "RPM" => ("rpm", v),
        "B" => ("data", v / GIB),
        "KB" => ("data", v / (1024.0 * 1024.0)),
        "MB" => ("data", round_dotnet(v / 1024.0, 3)),
        "GB" => ("data", v),
        "TB" => ("data", v * 1024.0),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::drm_usage::fixtures::{i915, write, xe};

    fn g(name: &str, integrated: bool, vram: Option<f64>, source: Option<&str>) -> GpuInfo {
        GpuInfo {
            name: name.into(),
            vram_total: vram,
            source: source.map(Into::into),
            integrated,
            ..Default::default()
        }
    }
    fn names(list: &[GpuInfo]) -> Vec<&str> {
        list.iter().map(|g| g.name.as_str()).collect()
    }
    fn no_names() -> HashMap<i32, (String, Option<String>)> {
        HashMap::new()
    }
    fn missing(root: &Path) -> PathBuf {
        root.join("missing.ids")
    }

    // GpuSourceTests: every source, and how they merge into one list with the main GPU first.
    #[test]
    fn the_discrete_card_comes_first_even_when_the_integrated_one_is_listed_first() {
        let os = [
            g("AMD Radeon(TM) Graphics", true, Some(2.0), None),
            g("NVIDIA GeForce RTX 5080", false, Some(15.6), None),
        ];
        let list = compose(&[None, None, Some(&os)]);
        assert_eq!(
            names(&list),
            ["NVIDIA GeForce RTX 5080", "AMD Radeon(TM) Graphics"]
        );
    }

    #[test]
    #[allow(clippy::approx_constant)] // 3.14 GB of video memory, as in the C# test, not π
    fn the_same_card_from_two_sources_is_one_entry_and_the_first_source_wins() {
        let smi = GpuInfo {
            name: "NVIDIA GeForce RTX 5080".into(),
            util: Some(3.0),
            temp: Some(42.3),
            power: Some(40.7),
            power_limit: Some(360.0),
            vram_used: Some(3.1),
            vram_total: Some(15.9),
            fan: Some(0.0),
            clock: Some(210.0),
            vendor: Some("nvidia".into()),
            source: Some("nvidia-smi".into()),
            driver: Some("621.01".into()),
            ..Default::default()
        };
        let os = GpuInfo {
            name: "NVIDIA GeForce RTX 5080".into(),
            util: Some(5.0),
            temp: Some(42.1),
            vram_used: Some(3.14),
            vram_total: Some(15.6),
            vendor: Some("nvidia".into()),
            source: Some("windows".into()),
            shared_used: Some(0.24),
            shared_total: Some(100.5),
            power_pct: Some(11.0),
            temp_max: Some(88.0),
            driver: Some("32.0.16.2101".into()),
            ..Default::default()
        };
        let list = compose(&[Some(&[smi]), None, Some(&[os])]);
        assert_eq!(list.len(), 1);
        let g = &list[0];
        assert_eq!(g.util, Some(3.0)); // nvidia-smi's
        assert_eq!(g.vram_total, Some(15.9));
        assert_eq!(g.driver.as_deref(), Some("621.01"));
        assert_eq!(g.shared_used, Some(0.24)); // only the OS knows these
        assert_eq!(g.temp_max, Some(88.0));
        assert_eq!(g.source.as_deref(), Some("nvidia-smi+windows"));
    }

    #[test]
    fn without_nvidia_smi_the_os_entries_stay() {
        let os = [g("AMD Radeon RX 7900 XTX", false, Some(24.0), None)];
        assert_eq!(
            names(&compose(&[None, None, Some(&os)])),
            ["AMD Radeon RX 7900 XTX"]
        );
    }

    #[test]
    fn two_identical_cards_stay_two_entries() {
        let smi = [
            g(
                "NVIDIA GeForce RTX 3090",
                false,
                Some(24.0),
                Some("nvidia-smi"),
            ),
            g(
                "NVIDIA GeForce RTX 3090",
                false,
                Some(24.0),
                Some("nvidia-smi"),
            ),
        ];
        let os = [
            g(
                "NVIDIA GeForce RTX 3090",
                false,
                Some(24.0),
                Some("windows"),
            ),
            g(
                "NVIDIA GeForce RTX 3090",
                false,
                Some(24.0),
                Some("windows"),
            ),
        ];
        let list = compose(&[Some(&smi), None, Some(&os)]);
        assert_eq!(list.len(), 2);
        assert!(
            list.iter()
                .all(|g| g.source.as_deref() == Some("nvidia-smi+windows"))
        );
    }

    #[test]
    fn integrated_graphics_are_told_apart_by_name() {
        for (name, integrated) in [
            ("AMD Radeon(TM) Graphics", true),
            ("AMD Radeon 780M Graphics", true),
            ("AMD Radeon(TM) RX Vega 11 Graphics", true),
            ("AMD Radeon RX 7900 XTX", false),
            ("AMD Radeon RX 6600 Graphics", false),
            ("AMD Radeon Pro W7900", false),
            ("Intel(R) UHD Graphics 770", true),
            ("Intel(R) Iris(R) Xe Graphics", true),
            ("Intel(R) Arc(TM) Graphics", true),
            ("Intel(R) Arc(TM) 140V GPU (16GB)", true),
            ("Intel(R) Arc(TM) A770 Graphics", false),
            ("Intel(R) Arc(TM) B580 Graphics", false),
            ("NVIDIA GeForce RTX 4060 Laptop GPU", false),
        ] {
            assert_eq!(looks_integrated(name), integrated, "{name}");
        }
    }

    #[test]
    fn nvidia_smi_lines_carry_the_index_name_and_memory_clock() {
        let names = parse_nvidia_names(
            "0, NVIDIA GeForce RTX 5080, 621.01\r\n1, NVIDIA GeForce RTX 3060, 621.01\r\n",
        );
        let (index, g) = parse_nvidia_line(
            "1, 37, 61, 120.5, 170.00, 2048, 12288, 30, 1800, 7501",
            &names,
        )
        .unwrap();
        assert_eq!(index, 1);
        assert_eq!(g.name, "NVIDIA GeForce RTX 3060");
        assert_eq!(g.util, Some(37.0));
        assert_eq!(g.vram_used, Some(2.0));
        assert_eq!(g.vram_total, Some(12.0));
        assert_eq!(g.mem_clock, Some(7501.0));
        assert_eq!(g.driver.as_deref(), Some("621.01"));
        assert_eq!(g.vendor.as_deref(), Some("nvidia"));
    }

    #[test]
    fn nvidia_smi_not_supported_fields_are_empty_not_zero() {
        let (_, g) = parse_nvidia_line(
            "0, 5, 40, [N/A], [N/A], 512, 4096, [N/A], 300, 405",
            &no_names(),
        )
        .unwrap();
        assert_eq!(g.power, None);
        assert_eq!(g.fan, None);
        assert_eq!(g.name, "NVIDIA GPU");
    }

    #[test]
    fn nvidia_smi_noise_is_skipped() {
        for line in [
            "",
            "garbage",
            "NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver.",
        ] {
            assert_eq!(parse_nvidia_line(line, &no_names()), None, "{line}");
        }
    }

    // A data.json in the shape LibreHardwareMonitor's HttpServer writes: culture-formatted Text/Value strings, Type and
    // SensorId on sensors, HardwareId on hardware nodes.
    const LHM_AMD: &str = r#"
        {"id":0,"Text":"Sensor","Min":"Min","Value":"Value","Max":"Max","ImageURL":"","Children":[
          {"id":1,"Text":"DESKTOP","Min":"","Value":"","Max":"","ImageURL":"images_icon/computer.png","Children":[
            {"id":2,"Text":"AMD Ryzen 9 7950X","HardwareId":"/amdcpu/0","ImageURL":"images_icon/cpu.png","Min":"","Value":"","Max":"","Children":[
              {"id":3,"Text":"Temperatures","ImageURL":"images_icon/temperature.png","Min":"","Value":"","Max":"","Children":[
                {"id":4,"Text":"Core (Tctl/Tdie)","Min":"40,0 °C","Value":"61,5 °C","Max":"80,0 °C","SensorId":"/amdcpu/0/temperature/2","Type":"Temperature","RawValue":61.5,"ImageURL":"images/transparent.png","Children":[]}]}]},
            {"id":5,"Text":"AMD Radeon RX 7900 XTX","HardwareId":"/gpu-amd/0","ImageURL":"images_icon/ati.png","Min":"","Value":"","Max":"","Children":[
              {"id":6,"Text":"Temperatures","ImageURL":"images_icon/temperature.png","Min":"","Value":"","Max":"","Children":[
                {"id":7,"Text":"GPU Core","Min":"","Value":"52,0 °C","Max":"","SensorId":"/gpu-amd/0/temperature/0","Type":"Temperature","Children":[]},
                {"id":8,"Text":"GPU Memory","Min":"","Value":"58,0 °C","Max":"","SensorId":"/gpu-amd/0/temperature/1","Type":"Temperature","Children":[]},
                {"id":9,"Text":"GPU Hot Spot","Min":"","Value":"71,0 °C","Max":"","SensorId":"/gpu-amd/0/temperature/7","Type":"Temperature","Children":[]}]},
              {"id":10,"Text":"Load","ImageURL":"images_icon/load.png","Min":"","Value":"","Max":"","Children":[
                {"id":11,"Text":"GPU Core","Min":"","Value":"87,0 %","Max":"","SensorId":"/gpu-amd/0/load/0","Type":"Load","Children":[]},
                {"id":12,"Text":"GPU Memory","Min":"","Value":"40,0 %","Max":"","SensorId":"/gpu-amd/0/load/1","Type":"Load","Children":[]}]},
              {"id":13,"Text":"Clocks","ImageURL":"images_icon/clock.png","Min":"","Value":"","Max":"","Children":[
                {"id":14,"Text":"GPU Core","Min":"","Value":"2482,0 MHz","Max":"","SensorId":"/gpu-amd/0/clock/0","Type":"Clock","Children":[]},
                {"id":15,"Text":"GPU Memory","Min":"","Value":"2500,0 MHz","Max":"","SensorId":"/gpu-amd/0/clock/2","Type":"Clock","Children":[]}]},
              {"id":16,"Text":"Powers","ImageURL":"images_icon/power.png","Min":"","Value":"","Max":"","Children":[
                {"id":17,"Text":"GPU Core","Min":"","Value":"250,0 W","Max":"","SensorId":"/gpu-amd/0/power/0","Type":"Power","Children":[]},
                {"id":18,"Text":"GPU Package","Min":"","Value":"312,4 W","Max":"","SensorId":"/gpu-amd/0/power/3","Type":"Power","Children":[]}]},
              {"id":19,"Text":"Fans","ImageURL":"images_icon/fan.png","Min":"","Value":"","Max":"","Children":[
                {"id":20,"Text":"GPU Fan","Min":"","Value":"1450 RPM","Max":"","SensorId":"/gpu-amd/0/fan/0","Type":"Fan","Children":[]}]},
              {"id":21,"Text":"Controls","ImageURL":"images_icon/control.png","Min":"","Value":"","Max":"","Children":[
                {"id":22,"Text":"GPU Fan","Min":"","Value":"42,0 %","Max":"","SensorId":"/gpu-amd/0/control/0","Type":"Control","Children":[]}]},
              {"id":23,"Text":"Data","ImageURL":"images_icon/data.png","Min":"","Value":"","Max":"","Children":[
                {"id":24,"Text":"GPU Memory Used","Min":"","Value":"6,0 GB","Max":"","SensorId":"/gpu-amd/0/data/0","Type":"Data","Children":[]},
                {"id":25,"Text":"GPU Memory Total","Min":"","Value":"24,0 GB","Max":"","SensorId":"/gpu-amd/0/data/2","Type":"Data","Children":[]},
                {"id":26,"Text":"D3D Shared Memory Used","Min":"","Value":"512,0 MB","Max":"","SensorId":"/gpu-amd/0/data/6","Type":"Data","Children":[]}]}]}]}]}
        "#;

    fn lhm(json: &str) -> Vec<GpuInfo> {
        parse_lhm(&serde_json::from_str(json).unwrap())
    }

    #[test]
    fn an_amd_card_is_read_from_libre_hardware_monitor() {
        let list = lhm(LHM_AMD);
        assert_eq!(list.len(), 1); // the CPU node is not a GPU
        let g = &list[0];
        assert_eq!(g.name, "AMD Radeon RX 7900 XTX");
        assert_eq!(g.vendor.as_deref(), Some("amd"));
        assert_eq!(g.source.as_deref(), Some("lhm"));
        assert!(!g.integrated);
        assert_eq!(
            (g.util, g.temp, g.hot_spot, g.mem_temp),
            (Some(87.0), Some(52.0), Some(71.0), Some(58.0))
        );
        assert_eq!((g.clock, g.mem_clock), (Some(2482.0), Some(2500.0)));
        assert_eq!(g.power, Some(312.4)); // the board (package), not the core
        assert_eq!(g.fan_rpm, Some(1450.0));
        assert_eq!(g.fan, Some(42.0)); // the fan duty is the control, not a load
        assert_eq!(
            (g.vram_used, g.vram_total, g.shared_used),
            (Some(6.0), Some(24.0), Some(0.5))
        );
    }

    #[test]
    fn older_libre_hardware_monitor_without_ids_is_recognised_by_its_icon() {
        // no HardwareId / SensorId / Type; memory as SmallData in MB; °F chosen in its settings
        let json = r#"
            {"Text":"Sensor","Children":[{"Text":"PC","ImageURL":"images_icon/computer.png","Children":[
              {"Text":"Intel(R) Arc(TM) A770 Graphics","ImageURL":"images_icon/intel.png","Children":[
                {"Text":"Temperatures","Children":[{"Text":"GPU Core","Value":"140.0 °F","Children":[]}]},
                {"Text":"Load","Children":[{"Text":"GPU Core","Value":"12.0 %","Children":[]}]},
                {"Text":"SmallData","Children":[{"Text":"GPU Memory Used","Value":"8192.0 MB","Children":[]},{"Text":"GPU Memory Total","Value":"16384.0 MB","Children":[]}]}]}]}]}
            "#;
        let list = lhm(json);
        assert_eq!(list.len(), 1);
        let g = &list[0];
        assert_eq!(g.vendor.as_deref(), Some("intel"));
        assert!(!g.integrated);
        assert_eq!(
            (g.temp, g.util, g.vram_used, g.vram_total),
            (Some(60.0), Some(12.0), Some(8.0), Some(16.0))
        );
    }

    #[test]
    fn an_lhm_intel_integrated_gpu_is_marked_integrated() {
        let json = r#"
            {"Text":"Sensor","Children":[{"Text":"Intel(R) UHD Graphics 770","HardwareId":"/gpu-intel-integrated/0x4680","ImageURL":"images_icon/intel.png","Children":[
              {"Text":"Powers","Children":[{"Text":"GPU Power","Value":"3,1 W","Type":"Power","SensorId":"/gpu-intel-integrated/0x4680/power/0","Children":[]}]}]}]}
            "#;
        let list = lhm(json);
        assert_eq!(list.len(), 1);
        assert!(list[0].integrated);
        assert_eq!(list[0].power, Some(3.1));
        assert_eq!(list[0].util, None); // the OS counters fill this in
    }

    #[test]
    fn lhm_fills_the_sensors_the_os_cannot_read() {
        let os = GpuInfo {
            name: "AMD Radeon RX 7900 XTX".into(),
            util: Some(85.0),
            temp: Some(51.0),
            vram_used: Some(6.1),
            vram_total: Some(23.98),
            vendor: Some("amd".into()),
            source: Some("windows".into()),
            shared_used: Some(0.4),
            shared_total: Some(31.9),
            power_pct: Some(88.0),
            ..Default::default()
        };
        let parsed = lhm(LHM_AMD);
        let list = compose(&[None, Some(&parsed), Some(&[os])]);
        assert_eq!(list.len(), 1);
        let g = &list[0];
        assert_eq!(
            (g.hot_spot, g.power, g.shared_total, g.power_pct),
            (Some(71.0), Some(312.4), Some(31.9), Some(88.0))
        );
        assert_eq!(g.source.as_deref(), Some("lhm+windows"));
    }

    #[test]
    fn lhm_values_are_read_with_their_unit() {
        for (value, kind, expected_kind, expected) in [
            ("45,0 °C", None, "temp", 45.0),
            ("113.0 °F", None, "temp", 45.0),
            ("12,5 %", Some("Load"), "load", 12.5),
            ("40,0 %", Some("Control"), "control", 40.0),
            ("40,0 %", Some("Controls"), "control", 40.0),
            ("1,5 GB", Some("Data"), "data", 1.5),
            ("512,0 MB", Some("Data"), "data", 0.5),
            ("1 RPM", None, "rpm", 1.0),
            ("1,2 GHz", None, "clock", 1200.0),
        ] {
            let (k, v) = parse_lhm_value(value, kind).unwrap_or_else(|| panic!("{value}"));
            assert_eq!(k, expected_kind, "{value}");
            assert_eq!(round_dotnet(v, 3), round_dotnet(expected, 3), "{value}");
        }
    }

    #[test]
    fn other_lhm_values_are_ignored() {
        for value in ["", "-", "1,0 V"] {
            assert_eq!(parse_lhm_value(value, None), None, "{value}"); // voltages are not used
        }
    }

    fn w(dir: &Path, file: &str, text: &str) {
        write(&dir.join(file), &format!("{text}\n"));
    }

    #[test]
    fn an_amd_card_is_read_from_linux_sysfs() {
        let root = tempfile::tempdir().unwrap();
        let dev = root.path().join("card1/device");
        std::fs::create_dir_all(root.path().join("card1-DP-1")).unwrap(); // a connector, not a card
        let hw = dev.join("hwmon/hwmon3");
        w(&dev, "vendor", "0x1002");
        w(&dev, "device", "0x744c");
        w(&dev, "gpu_busy_percent", "64");
        w(&dev, "mem_info_vram_used", &(6u64 << 30).to_string());
        w(&dev, "mem_info_vram_total", &(24u64 << 30).to_string());
        w(&dev, "mem_info_gtt_used", &(1u64 << 29).to_string());
        w(&dev, "mem_info_gtt_total", &(16u64 << 30).to_string());
        w(&dev, "pp_dpm_sclk", "0: 500Mhz\n1: 2482Mhz *\n2: 2620Mhz");
        w(&dev, "pp_dpm_mclk", "0: 96Mhz\n3: 1250Mhz *");
        for (f, v) in [
            ("temp1_input", "52000"),
            ("temp1_label", "edge"),
            ("temp2_input", "71000"),
            ("temp2_label", "junction"),
            ("temp3_input", "58000"),
            ("temp3_label", "mem"),
            ("temp1_crit", "100000"),
            ("power1_average", "312400000"),
            ("power1_cap", "339000000"),
            ("fan1_input", "1450"),
            ("fan1_max", "3300"),
        ] {
            w(&hw, f, v);
        }
        let ids = root.path().join("pci.ids");
        std::fs::write(&ids, "1000  Broadcom\n\t0001  Something\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\t744c  Navi 31 [Radeon RX 7900 XT/7900 XTX/7900 GRE/7900M]\n\t\t1002 0e3b  RX 7900 XTX\n").unwrap();

        let list = read_sysfs(root.path(), Some(&ids), Path::new("/proc"), None);
        assert_eq!(list.len(), 1);
        let g = &list[0];
        assert_eq!(g.name, "AMD Radeon RX 7900 XT/7900 XTX/7900 GRE/7900M");
        assert_eq!(g.source.as_deref(), Some("sysfs"));
        assert!(!g.integrated);
        assert_eq!(
            (g.util, g.temp, g.hot_spot, g.mem_temp, g.temp_max),
            (Some(64.0), Some(52.0), Some(71.0), Some(58.0), Some(100.0))
        );
        assert_eq!((g.power, g.power_limit), (Some(312.4), Some(339.0)));
        assert_eq!(
            (g.vram_used, g.vram_total, g.shared_used, g.shared_total),
            (Some(6.0), Some(24.0), Some(0.5), Some(16.0))
        );
        assert_eq!(
            (g.clock, g.mem_clock, g.fan_rpm, g.fan),
            (Some(2482.0), Some(1250.0), Some(1450.0), Some(44.0))
        );
    }

    #[test]
    fn linux_nvidia_cards_are_left_to_nvidia_smi_and_an_apu_is_integrated() {
        let root = tempfile::tempdir().unwrap();
        let card = |name: &str, vendor: &str, vram: u64| {
            let dev = root.path().join(name).join("device");
            write(&dev.join("vendor"), vendor);
            write(&dev.join("device"), "0x15bf");
            write(&dev.join("mem_info_vram_total"), &vram.to_string());
        };
        card("card0", "0x10de", 8 << 30);
        card("card1", "0x1002", 512 << 20);
        let list = read_sysfs(
            root.path(),
            Some(&missing(root.path())),
            Path::new("/proc"),
            None,
        );
        assert_eq!(list.len(), 1);
        assert!(list[0].integrated);
        assert_eq!(list[0].name, "AMD GPU");
        assert_eq!(list[0].util, None); // no file, no value: not 0
    }

    #[test]
    fn without_sysfs_there_are_no_gpus() {
        let nowhere = std::env::temp_dir().join("pb-no-such-dir-4f1c");
        assert!(read_sysfs(&nowhere, None, Path::new("/proc"), None).is_empty());
    }

    #[test]
    fn the_current_dpm_level_is_the_starred_one() {
        assert_eq!(dpm_current(Some("0: 500Mhz\n1: 2482Mhz *\n")), Some(2482.0));
        assert_eq!(dpm_current(Some("0: 500Mhz\n1: 2482Mhz\n")), None);
        assert_eq!(dpm_current(None), None);
    }

    #[test]
    fn unicode_arc_trademarks_four_gig_radeon_sysfs_merge_priority_and_comma_nvidia_names_are_handled_cleanly()
     {
        assert!(!looks_integrated("Intel® Arc™ A770 Graphics"));
        assert!(!looks_integrated("Intel® Arc™ B580 Graphics"));
        assert!(looks_integrated("Intel® Arc™ Graphics"));
        assert!(looks_integrated("Intel® Arc™ 140V GPU"));

        let root = tempfile::tempdir().unwrap();
        let dev = root.path().join("card0/device");
        write(&dev.join("vendor"), "0x1002\n");
        write(&dev.join("device"), "0x743f\n");
        write(&dev.join("product_name"), "AMD Radeon RX 6500 XT\n");
        write(
            &dev.join("mem_info_vram_total"),
            &format!("{}\n", (4u64 << 30) - (32u64 << 20)),
        );
        let list = read_sysfs(root.path(), None, Path::new("/proc"), None);
        assert_eq!(list.len(), 1);
        assert!(!list[0].integrated);

        let primary = GpuInfo {
            name: "AMD Radeon RX 6500 XT".into(),
            util: Some(40.0),
            temp: Some(55.0),
            power: Some(80.0),
            power_limit: Some(107.0),
            vram_used: Some(2.0),
            vram_total: Some(3.98),
            fan: Some(35.0),
            clock: Some(2600.0),
            vendor: Some("amd".into()),
            source: Some("lhm".into()),
            ..Default::default()
        };
        let secondary = GpuInfo {
            name: "AMD Radeon RX 6500 XT".into(),
            vram_total: Some(3.98),
            vendor: Some("amd".into()),
            source: Some("sysfs".into()),
            integrated: true,
            ..Default::default()
        };
        assert!(!merge(&primary, &secondary).integrated);

        let nv =
            parse_nvidia_names("0, NVIDIA Tegra, Orin, 540.3.0\n1, NVIDIA Tegra, Xavier, [N/A]\n");
        assert_eq!(
            nv[&0],
            (
                "NVIDIA Tegra, Orin".to_string(),
                Some("540.3.0".to_string())
            )
        );
        assert_eq!(nv[&1], ("NVIDIA Tegra, Xavier".to_string(), None));
    }

    // GpuIntegratedMergeTests: the Windows adapter's hybrid-integrated flag survives the merge with an LHM row.
    #[test]
    fn the_windows_integrated_flag_is_kept_when_an_earlier_source_merges_the_same_gpu() {
        // a name the heuristic doesn't recognise, so only Windows knows it is the integrated GPU of a hybrid laptop
        let lhm = [
            g("Vendor iGPU 600", false, Some(2.0), Some("lhm")),
            g("Vendor dGPU 9000", false, Some(1.0), Some("lhm")),
        ];
        let os = [
            g("Vendor iGPU 600", true, Some(2.0), Some("windows")),
            g("Vendor dGPU 9000", false, Some(1.0), Some("windows")),
        ];
        let gpus = compose(&[None, Some(&lhm), Some(&os)]);
        assert_eq!(names(&gpus), ["Vendor dGPU 9000", "Vendor iGPU 600"]);
        assert!(gpus[1].integrated);
        assert!(!gpus[0].integrated);
    }

    // DrmUsageTests that go through ReadSysfs: Intel's load from DRM fdinfo.
    #[test]
    fn an_intel_card_on_linux_gets_its_load_from_fdinfo() {
        let root = tempfile::tempdir().unwrap();
        let drm = root.path().join("drm");
        let proc = root.path().join("proc");
        // card0: Intel Arc 130V (Lunar Lake) on xe; card1: an AMD card that has gpu_busy_percent and keeps it
        let intel = drm.join("card0/device");
        write(&intel.join("vendor"), "0x8086\n");
        write(&intel.join("device"), "0x64a0\n");
        write(
            &intel.join("uevent"),
            "DRIVER=xe\nPCI_CLASS=30000\nPCI_ID=8086:64A0\nPCI_SLOT_NAME=0000:00:02.0\nMODALIAS=pci:v00008086d000064A0\n",
        );
        write(&intel.join("tile0/gt0/freq0/cur_freq"), "1850\n");
        let amd = drm.join("card1/device");
        write(&amd.join("vendor"), "0x1002\n");
        write(&amd.join("device"), "0x744c\n");
        write(
            &amd.join("uevent"),
            "DRIVER=amdgpu\nPCI_SLOT_NAME=0000:03:00.0\n",
        );
        write(&amd.join("gpu_busy_percent"), "64\n");
        write(&amd.join("mem_info_vram_total"), &(24u64 << 30).to_string());
        let igpu = crate::drm_usage::fixtures::IGPU;
        let clients = |game: u64, desktop: u64, total: u64| {
            write(
                &proc.join("4242/fdinfo/0"),
                "pos:\t0\nflags:\t0100002\nmnt_id:\t26\nino:\t7\n",
            ); // not a GPU
            write(
                &proc.join("4242/fdinfo/17"),
                &xe("31", game, total, 0, igpu),
            );
            write(
                &proc.join("4242/fdinfo/18"),
                &xe("31", game, total, 0, igpu),
            ); // a dup of fd 17
            write(
                &proc.join("1001/fdinfo/9"),
                &xe("4", desktop, total, 0, igpu),
            );
            write(
                &proc.join("1001/fdinfo/10"),
                &i915("8", 9_000_000_000, 0, "0000:00:04.0"),
            ); // another card
        };
        std::fs::create_dir_all(proc.join("self/fdinfo")).unwrap();
        std::fs::create_dir_all(proc.join("77")).unwrap(); // fdinfo unreadable or gone: skipped
        write(&proc.join("cpuinfo"), "processor\t: 0\n");
        let ids = missing(root.path());

        let now = std::sync::Arc::new(std::sync::Mutex::new(100.0));
        let clock = now.clone();
        let usage = DrmUsage::with_clock(Box::new(move || *clock.lock().unwrap()), 30.0);
        clients(1_000, 500, 10_000);
        let first = read_sysfs(&drm, Some(&ids), &proc, Some(&usage));
        let intel_gpu = first
            .iter()
            .find(|x| x.vendor.as_deref() == Some("intel"))
            .unwrap();
        assert_eq!(intel_gpu.util, None); // no value yet, not 0
        assert_eq!(intel_gpu.clock, Some(1850.0));
        assert!(intel_gpu.integrated);
        assert_eq!(
            first
                .iter()
                .find(|x| x.vendor.as_deref() == Some("amd"))
                .unwrap()
                .util,
            Some(64.0)
        );

        *now.lock().unwrap() = 102.0;
        clients(1_000 + 6_000, 500 + 1_000, 10_000 + 20_000);
        let second = read_sysfs(&drm, Some(&ids), &proc, Some(&usage));
        // (6000 + 1000) / 20000 cycles on render
        assert_eq!(
            second
                .iter()
                .find(|x| x.vendor.as_deref() == Some("intel"))
                .unwrap()
                .util,
            Some(35.0)
        );
        assert_eq!(
            second
                .iter()
                .find(|x| x.vendor.as_deref() == Some("amd"))
                .unwrap()
                .util,
            Some(64.0)
        );
    }

    #[test]
    fn an_intel_card_without_a_pci_address_has_no_load() {
        let root = tempfile::tempdir().unwrap();
        let dev = root.path().join("drm/card0/device");
        write(&dev.join("vendor"), "0x8086\n");
        write(&dev.join("uevent"), "DRIVER=i915\n");
        assert_eq!(pci_address(&dev), None);
        let usage = DrmUsage::new();
        for _ in 0..2 {
            let list = read_sysfs(
                &root.path().join("drm"),
                Some(&missing(root.path())),
                root.path(),
                Some(&usage),
            );
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].util, None);
        }
    }
}
