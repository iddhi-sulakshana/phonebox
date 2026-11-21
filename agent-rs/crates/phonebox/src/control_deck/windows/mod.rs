//! The deck's native side on Windows (`ControlDeck.Native.cs`, `ControlDeck.Windows.cs`): Core Audio devices and
//! sessions, the media sessions Windows lists, playback, monitors and power, MIDI and loopback listening. The pure
//! helpers here build and are tested on every target; the rest is Windows only, checked by the Windows build.

#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod hardware;
#[cfg(windows)]
mod media;
#[cfg(windows)]
mod native;
#[cfg(windows)]
mod playback;

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

/// A media session's player name: Spotify by any of its app ids, a desktop app's file name, else the app id.
pub fn media_name(aumid: &str) -> String {
    if aumid.to_lowercase().contains("spotify") {
        return "Spotify".into();
    }
    let n = aumid.len();
    if n >= 4 && aumid.is_char_boundary(n - 4) && aumid[n - 4..].eq_ignore_ascii_case(".exe") {
        return process_stem(aumid).to_string();
    }
    aumid.to_string()
}

/// `Path.GetFileNameWithoutExtension` with Windows separators: the last part of the path, without what follows its
/// last dot (empty for a folder).
pub fn process_stem(process: &str) -> &str {
    let file = process.rsplit(['\\', '/']).next().unwrap_or(process);
    file.rfind('.').map_or(file, |dot| &file[..dot])
}

/// `powercfg /list`'s plans: a GUID, the name in parentheses, then `*` on the active one.
pub fn parse_power_plans(output: &str) -> Vec<super::models::DeckItem> {
    static PLAN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?<id>[a-fA-F0-9]{8}-(?:[a-fA-F0-9]{4}-){3}[a-fA-F0-9]{12})\s+\((?<name>[^\r\n]+)\)(?<active>\s*\*)?",
        )
        .expect("the power plan pattern")
    });
    PLAN.captures_iter(output)
        .map(|m| super::models::DeckItem {
            id: m["id"].to_string(),
            name: m["name"].to_string(),
            kind: "power-plan".into(),
            state: Some(
                if m.name("active").is_some() {
                    "Active"
                } else {
                    "Available"
                }
                .into(),
            ),
            available: true,
            rev: None,
        })
        .collect()
}

/// A monitor's DDC/CI brightness as 0–100 (rounded half to even, as .NET), when its range is known.
pub fn brightness_percent(min: u32, current: u32, max: u32) -> Option<i32> {
    (max > min).then(|| {
        (f64::from(current.wrapping_sub(min)) * 100.0 / f64::from(max - min)).round_ties_even()
            as i32
    })
}

/// The DDC/CI level for 0–100 in a monitor's range.
pub fn brightness_level(min: u32, max: u32, value: i32) -> u32 {
    min.wrapping_add(
        (f64::from(max.wrapping_sub(min)) * f64::from(value) / 100.0).round_ties_even() as u32,
    )
}

/// A service's state (`SERVICE_STATUS.dwCurrentState`) as the deck shows it.
pub fn service_state_name(state: u32) -> &'static str {
    match state {
        1 => "Stopped",
        2 => "Starting",
        3 => "Stopping",
        4 => "Running",
        5 => "Resuming",
        6 => "Pausing",
        7 => "Paused",
        _ => "Service unavailable",
    }
}

/// The key a presentation command presses.
pub fn presentation_key(argument: Option<&str>) -> Option<&'static str> {
    match argument? {
        "start" => Some("F5"),
        "next" => Some("Right"),
        "prev" => Some("Left"),
        "end" => Some("Esc"),
        "blank" => Some("B"),
        _ => None,
    }
}

/// Where a window goes in its screen's working area (x, y, width, height): the left or right half, or the middle
/// three quarters. "max" maximizes instead; anything else is not a layout.
pub fn layout_rect(
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    layout: &str,
) -> Option<(i32, i32, i32, i32)> {
    match layout {
        "left" => Some((left, top, width / 2, height)),
        "right" => Some((left + width / 2, top, width / 2, height)),
        "center" => Some((
            left + width / 8,
            top + height / 8,
            width * 3 / 4,
            height * 3 / 4,
        )),
        _ => None,
    }
}

/// A display profile's mode: the size, a refresh rate (0 keeps it) and a position when both x and y are given.
#[derive(Debug, PartialEq)]
pub struct DisplayProfile {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
    pub position: Option<(i32, i32)>,
}

/// `{"width", "height", "hz"?, "x"?, "y"?}` as whole 32-bit numbers: 320–16384 wide, 200–16384 high, 0–1000 Hz, the
/// position clamped to ±32768. None for anything else (C# threw or refused).
pub fn parse_display_profile(json: &str) -> Option<DisplayProfile> {
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    let root = root.as_object()?;
    let int = |key: &str| -> Option<Option<i32>> {
        match root.get(key) {
            None => Some(None),
            Some(v) => v.as_i64().and_then(|v| i32::try_from(v).ok()).map(Some),
        }
    };
    let width = int("width")??;
    let height = int("height")??;
    let hz = int("hz")?.unwrap_or(0);
    if !(320..=16384).contains(&width)
        || !(200..=16384).contains(&height)
        || !(0..=1000).contains(&hz)
    {
        return None;
    }
    let position = match (root.contains_key("x"), root.contains_key("y")) {
        (true, true) => Some((
            int("x")??.clamp(-32768, 32768),
            int("y")??.clamp(-32768, 32768),
        )),
        _ => None,
    };
    Some(DisplayProfile {
        width: width as u32,
        height: height as u32,
        hz: hz as u32,
        position,
    })
}

/// A screen's name: its device name, and " (primary)" on the primary one.
pub fn monitor_name(id: &str, primary: bool) -> String {
    if primary {
        format!("{id} (primary)")
    } else {
        id.to_string()
    }
}

/// (position, duration) in ms from a session's timeline (100 ns ticks; `updated` and `now` as file times): a playing
/// session's position moves on since its last update, clamped to the duration.
pub fn media_timeline(
    start: i64,
    end: i64,
    position: i64,
    updated: i64,
    now: i64,
    playing: bool,
) -> (i64, i64) {
    let duration = ((end - start) / 10_000).max(0);
    let mut at = (position - start) / 10_000;
    if playing {
        at += ((now - updated) / 10_000).max(0);
    }
    (at.clamp(0, duration), duration)
}

/// `MediaPlaybackAutoRepeatMode` as its name ("None" when there is none).
pub fn repeat_name(mode: Option<i32>) -> String {
    match mode {
        Some(1) => "Track".into(),
        Some(2) => "List".into(),
        Some(0) | None => "None".into(),
        Some(other) => other.to_string(),
    }
}

/// What a sound's target is: an http(s) address ending in `.m3u8` (HLS), another http(s) address (a stream), or a
/// file path.
#[derive(Debug, PartialEq)]
pub enum PlayTarget {
    Hls(reqwest::Url),
    Stream(reqwest::Url),
    File(std::path::PathBuf),
}

pub fn play_target(target: &str) -> PlayTarget {
    match reqwest::Url::parse(target) {
        Ok(url) if super::hls::is_hls_url(&url) => PlayTarget::Hls(url),
        Ok(url) if matches!(url.scheme(), "http" | "https") => PlayTarget::Stream(url),
        _ => PlayTarget::File(target.into()),
    }
}

/// `AdaptiveMediaSourceCreationStatus` as .NET prints it (its number when it has no name).
pub fn adaptive_status_name(status: i32) -> String {
    match status {
        0 => "Success".into(),
        1 => "ManifestDownloadFailure".into(),
        2 => "ManifestParseFailure".into(),
        3 => "UnsupportedManifestContentType".into(),
        4 => "UnsupportedManifestVersion".into(),
        5 => "UnsupportedManifestProfile".into(),
        6 => "UnknownFailure".into(),
        other => other.to_string(),
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

    #[test]
    fn media_names_timelines_and_repeat_modes() {
        assert_eq!(
            media_name("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"),
            "Spotify"
        );
        assert_eq!(media_name("C:\\Program Files\\VLC\\vlc.EXE"), "vlc");
        assert_eq!(media_name("Chrome"), "Chrome");
        // 10 s into a 100 s track, updated 2 s ago and playing: 12 s
        assert_eq!(
            media_timeline(0, 1_000_000_000, 100_000_000, 0, 20_000_000, true),
            (12_000, 100_000)
        );
        assert_eq!(
            media_timeline(0, 1_000_000_000, 100_000_000, 0, 20_000_000, false),
            (10_000, 100_000)
        );
        assert_eq!(
            media_timeline(0, 10_000_000, 100_000_000, 0, 0, false),
            (1_000, 1_000)
        );
        assert_eq!(media_timeline(5, 0, 0, 0, 0, false), (0, 0));
        assert_eq!(repeat_name(Some(2)), "List");
        assert_eq!(repeat_name(None), "None");
    }

    #[test]
    fn play_targets_and_adaptive_statuses() {
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        assert_eq!(
            play_target("https://radio.example/live/playlist.M3U8?token=1"),
            PlayTarget::Hls(url("https://radio.example/live/playlist.M3U8?token=1"))
        );
        assert_eq!(
            play_target("http://radio.example/stream.mp3"),
            PlayTarget::Stream(url("http://radio.example/stream.mp3"))
        );
        for file in [
            r"C:\Sounds\ding.wav",
            "file:///tmp/test.m3u8",
            "ftp://host/a.mp3",
            "ding.wav",
        ] {
            assert_eq!(play_target(file), PlayTarget::File(file.into()), "{file}");
        }
        assert_eq!(adaptive_status_name(1), "ManifestDownloadFailure");
        assert_eq!(adaptive_status_name(6), "UnknownFailure");
        assert_eq!(adaptive_status_name(9), "9");
    }

    #[test]
    fn power_plans_from_powercfg_output() {
        let output = "Existing Power Schemes (* Active)\r\n-----------------------------------\r\n\
            Power Scheme GUID: 381b4222-f694-41f0-9685-ff5bb260df2e  (Balanced) *\r\n\
            Power Scheme GUID: 8C5E7FDA-E8BF-4A96-9A85-A6E23A8C635C  (High performance)\r\n";
        let plans = parse_power_plans(output);
        let summary: Vec<_> = plans
            .iter()
            .map(|p| {
                (
                    p.id.as_str(),
                    p.name.as_str(),
                    p.kind.as_str(),
                    p.state.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "381b4222-f694-41f0-9685-ff5bb260df2e",
                    "Balanced",
                    "power-plan",
                    Some("Active")
                ),
                (
                    "8C5E7FDA-E8BF-4A96-9A85-A6E23A8C635C",
                    "High performance",
                    "power-plan",
                    Some("Available")
                ),
            ]
        );
        assert!(parse_power_plans("Access is denied.").is_empty());
    }

    #[test]
    fn brightness_service_states_and_presentation_keys() {
        assert_eq!(brightness_percent(0, 50, 100), Some(50));
        assert_eq!(brightness_percent(10, 20, 30), Some(50));
        assert_eq!(brightness_percent(0, 1, 8), Some(12)); // 12.5, half to even
        assert_eq!(brightness_percent(5, 5, 5), None);
        assert_eq!(brightness_level(0, 100, 50), 50);
        assert_eq!(brightness_level(0, 5, 50), 2); // 2.5, half to even
        assert_eq!(brightness_level(10, 30, 50), 20);
        assert_eq!(service_state_name(4), "Running");
        assert_eq!(service_state_name(7), "Paused");
        assert_eq!(service_state_name(0), "Service unavailable");
        assert_eq!(presentation_key(Some("start")), Some("F5"));
        assert_eq!(presentation_key(Some("blank")), Some("B"));
        assert_eq!(presentation_key(Some("jump")), None);
        assert_eq!(presentation_key(None), None);
    }

    #[test]
    fn window_layouts_display_profiles_and_names() {
        assert_eq!(
            layout_rect(0, 0, 1920, 1040, "left"),
            Some((0, 0, 960, 1040))
        );
        assert_eq!(
            layout_rect(0, 0, 1920, 1040, "right"),
            Some((960, 0, 960, 1040))
        );
        assert_eq!(
            layout_rect(-1920, 0, 1920, 1040, "center"),
            Some((-1680, 130, 1440, 780))
        );
        assert_eq!(layout_rect(0, 0, 1920, 1040, "max"), None);
        let profile = |width, height, hz, position| {
            Some(DisplayProfile {
                width,
                height,
                hz,
                position,
            })
        };
        assert_eq!(
            parse_display_profile(r#"{"width":1920,"height":1080}"#),
            profile(1920, 1080, 0, None)
        );
        assert_eq!(
            parse_display_profile(r#"{"width":2560,"height":1440,"hz":144,"x":40000,"y":-5}"#),
            profile(2560, 1440, 144, Some((32768, -5)))
        );
        assert_eq!(
            parse_display_profile(r#"{"width":1920,"height":1080,"x":5}"#),
            profile(1920, 1080, 0, None)
        );
        for bad in [
            r#"{"width":100,"height":1080}"#,
            r#"{"height":1080}"#,
            r#"{"width":1920,"height":1080,"hz":"60"}"#,
            r#"{"width":1920.5,"height":1080}"#,
            r#"{"width":1920,"height":1080,"hz":1001}"#,
            "[]",
            "not json",
        ] {
            assert_eq!(parse_display_profile(bad), None, "{bad}");
        }
        assert_eq!(
            process_stem(r"C:\Program Files\Microsoft Office\POWERPNT.EXE"),
            "POWERPNT"
        );
        assert_eq!(process_stem("soffice"), "soffice");
        assert_eq!(process_stem(r"C:\Tools\"), "");
        assert_eq!(process_stem("my.app.exe"), "my.app");
        assert_eq!(
            monitor_name(r"\\.\DISPLAY1", true),
            r"\\.\DISPLAY1 (primary)"
        );
        assert_eq!(monitor_name(r"\\.\DISPLAY2", false), r"\\.\DISPLAY2");
    }
}
