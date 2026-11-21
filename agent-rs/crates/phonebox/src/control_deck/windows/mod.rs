//! The deck's native side on Windows (`ControlDeck.Native.cs`, `ControlDeck.Windows.cs`): Core Audio devices and
//! sessions, the media sessions Windows lists, playback, monitors and power, MIDI and loopback listening. The pure
//! helpers here build and are tested on every target; the rest is Windows only, checked by the Windows build.

#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod native;

#[cfg(windows)]
pub use native::WindowsDeckNative;

/// A name worth showing instead of the process name: not blank, not an unresolved `ms-resource:` string, and not
/// just the process name (with or without `.exe`), in any case.
pub fn useful_name(value: Option<&str>, process: &str) -> bool {
    let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
        return false;
    };
    if value.len() >= 12 && value[..12].eq_ignore_ascii_case("ms-resource:") {
        return false;
    }
    let candidate = value.trim().to_uppercase();
    candidate != process.to_uppercase() && candidate != format!("{process}.exe").to_uppercase()
}

/// A session's name: the system's sounds for pid 0, else its display name unless it is blank or an `@` resource,
/// else the process's friendly name.
pub fn session_display(pid: u32, display: Option<&str>, friendly: &str) -> String {
    if pid == 0 {
        return "System sounds".into();
    }
    match display.filter(|d| !d.trim().is_empty() && !d.starts_with('@')) {
        Some(display) => display.to_string(),
        None => friendly.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_worth_showing_and_session_names() {
        assert!(useful_name(Some("MSI Center"), "DCv2"));
        assert!(!useful_name(Some("dcv2.EXE"), "DCv2"));
        assert!(!useful_name(Some(" DCv2 "), "DCv2"));
        assert!(!useful_name(Some("ms-resource:AppName"), "x"));
        assert!(!useful_name(Some("  "), "x"));
        assert!(!useful_name(None, "x"));
        assert_eq!(session_display(0, Some("x"), "y"), "System sounds");
        assert_eq!(
            session_display(7, Some("@%SystemRoot%\\x.dll,-1"), "Friendly"),
            "Friendly"
        );
        assert_eq!(session_display(7, Some(" "), "Friendly"), "Friendly");
        assert_eq!(session_display(7, Some("Chrome"), "Friendly"), "Chrome");
    }
}
