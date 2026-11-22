//! Port of `Idle` (`ClaudeHook.cs`): how long the PC's user has not touched the keyboard or mouse. Blocking (macOS
//! runs `ioreg`, Linux `xprintidle`, both bounded at 2 s): call it off the async workers.

/// Seconds since the last keyboard or mouse input; 0 when unknown (Wayland, a missing tool).
pub fn seconds() -> f64 {
    #[cfg(windows)]
    {
        use windows::Win32::System::SystemInformation::GetTickCount;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        // Environment.TickCount and dwTime both wrap; the difference does not
        if unsafe { GetLastInputInfo(&mut info) }.as_bool() {
            (unsafe { GetTickCount() } as i32).wrapping_sub(info.dwTime as i32) as f64 / 1000.0
        } else {
            0.0
        }
    }
    #[cfg(target_os = "macos")]
    {
        parse_ioreg(
            &crate::unix::sh::run("ioreg", &["-c", "IOHIDSystem", "-d", "4"]).unwrap_or_default(),
        )
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // X11 only; on Wayland this reports 0, i.e. "at the PC"
        parse_xprintidle(crate::unix::sh::run("xprintidle", &[]).as_deref())
    }
}

/// `"HIDIdleTime" = <nanoseconds>` in `ioreg -c IOHIDSystem -d 4`, as seconds; 0 without it.
pub fn parse_ioreg(text: &str) -> f64 {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#""HIDIdleTime" = (\d+)"#).expect("valid"));
    RE.captures(text)
        .and_then(|c| c[1].parse::<i64>().ok())
        .map_or(0.0, |ns| ns as f64 / 1e9)
}

/// `xprintidle`'s milliseconds as seconds; 0 when it gave nothing that parses.
pub fn parse_xprintidle(text: Option<&str>) -> f64 {
    text.and_then(|t| t.trim().parse::<f64>().ok())
        .filter(|ms| ms.is_finite())
        .map_or(0.0, |ms| ms / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_parses_ioreg_and_xprintidle() {
        let ioreg =
            "  | |   \"HIDIdleTime\" = 2500000000\n  | |   \"HIDPointerAcceleration\" = 45056";
        assert_eq!(parse_ioreg(ioreg), 2.5);
        assert_eq!(parse_ioreg("nothing here"), 0.0);
        assert_eq!(
            parse_ioreg("\"HIDIdleTime\" = 99999999999999999999999"),
            0.0
        );
        assert_eq!(parse_xprintidle(Some("61234\n")), 61.234);
        assert_eq!(parse_xprintidle(Some("")), 0.0);
        assert_eq!(parse_xprintidle(Some("n/a")), 0.0);
        assert_eq!(parse_xprintidle(None), 0.0);
    }
}
