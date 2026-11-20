//! Port of `UsbDoctor.cs`: why a phone that still talks to the agent (over Wi-Fi) is missing from adb, from the
//! phone's own view of its USB link (sent with its battery report). The usual cause is a cable or port that carries
//! power but no data.

use std::sync::Mutex;

/// `ChargeGuard`'s `PhoneBatteryReport`: the phone's battery and USB link as it last reported them.
#[derive(Debug, Clone, PartialEq)]
pub struct PhoneBatteryReport {
    pub level: i32,
    pub plugged: bool,
    pub temp: Option<f64>,
    /// Unix ms.
    pub at: i64,
    /// "ac", "usb", "wireless"…
    pub source: Option<String>,
    /// The phone's USB state, e.g. "connected,configured,adb"; None from an older app.
    pub usb: Option<String>,
}

static LAST_REPORT: Mutex<Option<PhoneBatteryReport>> = Mutex::new(None);

/// `ChargeGuard.LastReport`. Nothing sets it before the battery route is ported (with `ChargeGuard`, plan 6), so
/// until then there is never a diagnosis.
pub fn last_report() -> Option<PhoneBatteryReport> {
    LAST_REPORT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub fn set_last_report(report: PhoneBatteryReport) {
    *LAST_REPORT.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
}

/// A report older than this says nothing about the link any more.
pub const FRESH_MS: i64 = 2 * 60_000;

/// A sentence for the settings page and the log, or None when there is nothing to explain.
pub fn diagnose(r: Option<&PhoneBatteryReport>, adb_sees_phone: bool, now: i64) -> Option<String> {
    let r = r.filter(|_| !adb_sees_phone)?;
    if !(0..=FRESH_MS).contains(&(now - r.at)) {
        return None;
    }
    let usb: Vec<&str> = r
        .usb
        .as_deref()?
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect(); // None: an older app
    let connected = usb.contains(&"connected");
    if connected && usb.contains(&"adb") {
        return Some(
            "The phone says it is connected over USB with debugging on, but adb does not see it. Try another USB \
             port (straight on the computer, not a hub), or restart adb (adb kill-server)."
                .into(),
        );
    }
    if connected {
        return Some(
            "The phone is connected over USB but USB debugging is off. Turn it on in the phone's Developer options."
                .into(),
        );
    }
    if r.plugged && r.source.as_deref() != Some("wireless") {
        return Some(
            "The phone is charging but has no USB data connection: the cable or the port only carries power. Try \
             another cable or port; until then the panel runs over Wi-Fi."
                .into(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000_000;

    fn r(
        plugged: bool,
        usb: Option<&str>,
        age_ms: i64,
        source: Option<&str>,
    ) -> PhoneBatteryReport {
        PhoneBatteryReport {
            level: 60,
            plugged,
            temp: None,
            at: NOW - age_ms,
            source: source.map(String::from),
            usb: usb.map(String::from),
        }
    }

    fn says(d: Option<String>, text: &str) -> bool {
        d.is_some_and(|d| d.contains(text))
    }

    #[test]
    fn charging_without_data_blames_the_cable() {
        assert!(says(
            diagnose(Some(&r(true, Some("none"), 0, Some("ac"))), false, NOW),
            "only carries power"
        ));
    }

    #[test]
    fn wireless_charging_without_usb_does_not_blame_a_cable() {
        assert!(says(
            diagnose(Some(&r(true, Some("none"), 0, Some("usb"))), false, NOW),
            "only carries power"
        ));
        assert_eq!(
            diagnose(
                Some(&r(true, Some("none"), 0, Some("wireless"))),
                false,
                NOW
            ),
            None
        );
        assert!(says(
            diagnose(
                Some(&r(true, Some("connected"), 0, Some("wireless"))),
                false,
                NOW
            ),
            "USB debugging is off"
        ));
    }

    #[test]
    fn battery_reports_from_before_a_clock_rollback_do_not_raise_a_diagnosis() {
        for (age_ms, expected) in [(0, true), (-1, false), (-300_000, false)] {
            let actual =
                diagnose(Some(&r(true, Some("none"), age_ms, Some("ac"))), false, NOW).is_some();
            assert_eq!(actual, expected, "age {age_ms}");
        }
    }

    #[test]
    fn connected_with_adb_but_unseen_blames_the_port() {
        assert!(says(
            diagnose(
                Some(&r(true, Some("connected,configured,adb"), 0, None)),
                false,
                NOW
            ),
            "adb does not see it"
        ));
    }

    #[test]
    fn connected_without_adb_asks_for_debugging() {
        assert!(says(
            diagnose(
                Some(&r(true, Some("connected,configured"), 0, None)),
                false,
                NOW
            ),
            "USB debugging is off"
        ));
    }

    #[test]
    fn stays_quiet() {
        for (plugged, usb, age_ms, adb_sees) in [
            (true, Some("none"), 0, true), // adb sees the phone: nothing to explain
            (false, Some("none"), 0, false), // unplugged
            (true, None, 0, false),        // an older app without the USB report
            (true, Some("none"), 3 * 60_000, false), // stale report
        ] {
            assert_eq!(
                diagnose(Some(&r(plugged, usb, age_ms, None)), adb_sees, NOW),
                None,
                "{plugged} {usb:?} {age_ms}"
            );
        }
    }

    #[test]
    fn no_report_no_diagnosis() {
        assert_eq!(diagnose(None, false, NOW), None);
    }
}
