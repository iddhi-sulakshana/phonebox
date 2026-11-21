//! Port of `Gpus.cs`: GPU sources (nvidia-smi, LibreHardwareMonitor, Linux sysfs) and how they combine.

use std::sync::LazyLock;

use regex::Regex;

use crate::system_monitor::round_dotnet;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

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
