//! The actions the phone may ask for, and the shadow properties each one sends: the tcl-server dashboard's payloads.

use serde_json::{Map, Value, json};

/// The temperature power-on and Cool set, as the dashboard.
const ON_TEMPERATURE: i64 = 26;

/// A whole number in `range`.
fn whole(value: &str, range: std::ops::RangeInclusive<i64>, what: &str) -> Result<i64, String> {
    value
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|n| range.contains(n))
        .ok_or_else(|| {
            format!(
                "{what} must be a whole number from {} to {}",
                range.start(),
                range.end()
            )
        })
}

/// The properties for `action` with `value`; `all` is the all-units bar (its power-on leaves `healthy` alone, as the
/// dashboard's does). Actions: `power` on|off, `mode` cool|fan|dry|heat|auto, `temp` 16–31, `fan` 0–7 (0 auto,
/// 7 turbo), `generator` 0–3 (0 off).
pub fn properties(action: &str, value: &str, all: bool) -> Result<Map<String, Value>, String> {
    let v = match (action, value.trim()) {
        ("power", "on") if all => json!({ "powerSwitch": 1, "targetTemperature": ON_TEMPERATURE }),
        ("power", "on") => {
            json!({ "powerSwitch": 1, "healthy": 1, "targetTemperature": ON_TEMPERATURE })
        }
        ("power", "off") => json!({ "powerSwitch": 0 }),
        ("power", _) => return Err("Power is on or off".into()),
        ("mode", "cool") => json!({ "workMode": 1, "targetTemperature": ON_TEMPERATURE }),
        ("mode", "fan") => json!({ "workMode": 3, "targetTemperature": 31 }),
        ("mode", "dry") => json!({ "workMode": 2, "targetTemperature": 16 }),
        ("mode", "heat") => json!({ "workMode": 4, "targetTemperature": 31 }),
        ("mode", "auto") => json!({ "workMode": 0 }),
        ("mode", _) => return Err("The mode is cool, fan, dry, heat or auto".into()),
        ("temp", v) => json!({ "targetTemperature": whole(v, 16..=31, "The temperature")? }),
        ("fan", v) => json!({ "windSpeed7Gear": whole(v, 0..=7, "The fan speed")? }),
        ("generator", v) => json!({ "generatorMode": whole(v, 0..=3, "The generator mode")? }),
        _ => return Err("There is no such action".into()),
    };
    match v {
        Value::Object(map) => Ok(map),
        _ => unreachable!("every payload is an object"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn props(action: &str, value: &str, all: bool) -> Value {
        Value::Object(properties(action, value, all).unwrap())
    }

    #[test]
    fn every_action_has_the_web_dashboards_payload() {
        let single = |a: &str, v: &str| props(a, v, false);
        assert_eq!(
            single("power", "on"),
            json!({"powerSwitch": 1, "healthy": 1, "targetTemperature": 26})
        );
        assert_eq!(
            props("power", "on", true),
            json!({"powerSwitch": 1, "targetTemperature": 26})
        );
        assert_eq!(single("power", "off"), json!({"powerSwitch": 0}));
        assert_eq!(props("power", "off", true), json!({"powerSwitch": 0}));
        assert_eq!(
            single("mode", "cool"),
            json!({"workMode": 1, "targetTemperature": 26})
        );
        assert_eq!(
            single("mode", "fan"),
            json!({"workMode": 3, "targetTemperature": 31})
        );
        assert_eq!(
            single("mode", "dry"),
            json!({"workMode": 2, "targetTemperature": 16})
        );
        assert_eq!(
            single("mode", "heat"),
            json!({"workMode": 4, "targetTemperature": 31})
        );
        assert_eq!(single("mode", "auto"), json!({"workMode": 0}));
        assert_eq!(single("temp", "16"), json!({"targetTemperature": 16}));
        assert_eq!(single("temp", " 31 "), json!({"targetTemperature": 31}));
        assert_eq!(single("fan", "0"), json!({"windSpeed7Gear": 0}));
        assert_eq!(single("fan", "7"), json!({"windSpeed7Gear": 7}));
        assert_eq!(single("generator", "0"), json!({"generatorMode": 0}));
        assert_eq!(props("generator", "3", true), json!({"generatorMode": 3}));
    }

    #[test]
    fn values_outside_the_ranges_are_refused() {
        for (action, value) in [
            ("temp", "15"),
            ("temp", "32"),
            ("temp", "24.5"),
            ("temp", ""),
            ("fan", "8"),
            ("fan", "-1"),
            ("generator", "4"),
            ("mode", "turbo"),
            ("power", "maybe"),
            ("swing", "1"),
            ("", ""),
        ] {
            assert!(
                properties(action, value, false).is_err(),
                "{action}={value}"
            );
        }
    }
}
