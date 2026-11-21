//! The deck's native side on Windows (`ControlDeck.Native.cs`, `ControlDeck.Windows.cs`): Core Audio devices and
//! sessions, the media sessions Windows lists, playback, monitors and power, MIDI and loopback listening. The pure
//! helpers here build and are tested on every target; the rest is Windows only, checked by the Windows build.

#[cfg(windows)]
mod audio;
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
        // Path.GetFileNameWithoutExtension with Windows separators
        let file = aumid.rsplit(['\\', '/']).next().unwrap_or(aumid);
        return file.rfind('.').map_or(file, |dot| &file[..dot]).to_string();
    }
    aumid.to_string()
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
}
