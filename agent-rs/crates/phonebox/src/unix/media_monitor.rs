//! `Unix/MediaMonitor.cs`: now playing on Linux (MPRIS through playerctl) and macOS (system media through
//! nowplaying-cli, with a Spotify and Music AppleScript fallback). Same API as the Windows monitor.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::media_monitor::{AlbumArt, MediaSnapshot};

pub const MAX_ART_BYTES: usize = 5 * 1024 * 1024;
pub const ART_DEADLINE: Duration = Duration::from_secs(5);
/// How often Spotify's or Music's own position is asked for while macOS reports none.
pub const ANCHOR_EVERY: chrono::TimeDelta = chrono::TimeDelta::seconds(5);

pub type Time = chrono::DateTime<chrono::Utc>;
type AppPosition = Arc<dyn Fn(&str) -> Option<i64> + Send + Sync>;

/// The macOS now-playing bookkeeping: where the position sequence and the cover stand.
#[derive(Default)]
struct Work {
    art_url: String,
    art_key: String,
    art_encoded: String,
    position_key: String,
    position_ms: i64,
    last_raw_position_ms: i64,
    position_at: Option<Time>,
    anchor_key: String,
    anchor_playing: bool,
    /// None: ask the player at the next reading.
    anchor_at: Option<Time>,
    source_key: String,
    source: String,
    source_at: Option<Time>,
}

pub struct MediaMonitor {
    snap: Mutex<Option<MediaSnapshot>>,
    art: Mutex<Option<Arc<AlbumArt>>>,
    art_id: AtomicI32,
    warned: AtomicBool,
    mac_system_media: AtomicBool,
    /// Held for a whole reading (the C# poll gate).
    work: Mutex<Work>,
    /// When the last now-playing position was computed.
    last_sample: Mutex<Option<Instant>>,
    app_position: Mutex<AppPosition>,
}

const CATEGORY: &str = "PhoneBox.Agent.MediaMonitor";
const PREFIX: &str = "kMRMediaRemoteNowPlayingInfo";

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl MediaMonitor {
    pub fn new() -> Arc<MediaMonitor> {
        Arc::new(MediaMonitor {
            snap: Mutex::new(None),
            art: Mutex::new(None),
            art_id: AtomicI32::new(0),
            warned: AtomicBool::new(false),
            mac_system_media: AtomicBool::new(false),
            work: Mutex::new(Work::default()),
            last_sample: Mutex::new(None),
            app_position: Mutex::new(Arc::new(read_app_position)),
        })
    }

    pub fn snapshot(&self) -> Option<MediaSnapshot> {
        locked(&self.snap).clone()
    }

    pub fn art(&self) -> Option<Arc<AlbumArt>> {
        locked(&self.art).clone()
    }

    /// For tests: where the player says playback is, instead of asking Spotify or Music.
    pub fn set_app_position(&self, f: impl Fn(&str) -> Option<i64> + Send + Sync + 'static) {
        *locked(&self.app_position) = Arc::new(f);
    }

    pub(crate) fn last_sample(&self) -> Option<Instant> {
        *locked(&self.last_sample)
    }

    /// Reads what plays every second; a change that matters pokes the phone. Between reads the thread waits once
    /// (one wake per second when idle, as C#'s `Task.Delay(1000, ct)`), and the agent's stop ends the wait at once.
    pub fn start(self: &Arc<Self>, stop: crate::server::Stop) -> std::thread::JoinHandle<()> {
        let me = self.clone();
        std::thread::Builder::new()
            .name("media".into())
            .spawn(move || {
                while !stop.is_stopped() {
                    me.poll();
                    if stop.sleep(Duration::from_secs(1)) {
                        break;
                    }
                }
            })
            .expect("media thread")
    }

    fn set_art(&self, art: Option<Arc<AlbumArt>>) {
        *locked(&self.art) = art;
    }

    /// Takes a new cover: the id changes only when the bytes or the type do.
    fn publish_art(&self, next: AlbumArt) {
        let mut art = locked(&self.art);
        if art.as_deref() != Some(&next) {
            *art = Some(Arc::new(next));
            self.art_id.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn current_art_id(&self) -> i32 {
        if self.art().is_some() {
            self.art_id.load(Ordering::SeqCst)
        } else {
            0
        }
    }

    fn poll(&self) {
        let before = self.snapshot();
        {
            let mut work = locked(&self.work);
            let (snap, art_url) = if cfg!(target_os = "macos") {
                self.read_mac(&mut work)
            } else {
                self.read_linux()
            };
            // when the position was computed: for the now-playing info that is inside read_mac_system_media, before
            // the artwork is decoded (a TIFF is converted with sips, which can take seconds); otherwise just now
            let system = self.mac_system_media.load(Ordering::SeqCst);
            let sampled_at = match self.last_sample() {
                Some(t) if cfg!(target_os = "macos") && system => t,
                _ => Instant::now(),
            };
            match snap {
                None => {
                    *locked(&self.snap) = None;
                    self.set_art(None);
                    work.art_key.clear();
                    work.art_encoded.clear();
                    work.position_key.clear();
                    work.art_url.clear();
                }
                Some(snap) => {
                    if !system {
                        if !work.art_key.is_empty() {
                            work.art_key.clear();
                            work.art_encoded.clear();
                            work.art_url = "\0".into();
                        }
                        if art_url != work.art_url {
                            work.art_url = art_url.clone();
                            match self.load_art(&art_url) {
                                None => self.set_art(None),
                                Some(next) => self.publish_art(next),
                            }
                        }
                    }
                    *locked(&self.snap) = Some(MediaSnapshot {
                        art_id: self.current_art_id(),
                        sampled_at: Some(sampled_at),
                        ..snap
                    });
                }
            }
        }
        // a new track, a pause or a seek reaches the phone now, not at its next whole-second state
        if crate::state_signal::media_changed(before.as_ref(), self.snapshot().as_ref()) {
            crate::state_signal::poke();
        }
    }

    // ------------------------------------------------------------ Linux: playerctl (MPRIS)

    fn read_linux(&self) -> (Option<MediaSnapshot>, String) {
        if crate::unix::sh::which("playerctl").is_none() {
            if !self.warned.swap(true, Ordering::SeqCst) {
                crate::log::info(
                    CATEGORY,
                    "media: install playerctl to show and control what is playing (sudo apt install playerctl)",
                );
            }
            return (None, String::new());
        }
        let line = crate::unix::sh::run(
            "playerctl",
            &[
                "metadata",
                "--format",
                "{{playerName}}\t{{status}}\t{{title}}\t{{artist}}\t{{album}}\t{{position}}\t{{mpris:length}}\t{{mpris:artUrl}}",
            ],
        );
        let Some(line) = line.filter(|l| !l.trim().is_empty()) else {
            return (None, String::new());
        };
        let f: Vec<&str> = line.trim_end_matches('\n').split('\t').collect();
        if f.len() < 8 {
            return (None, String::new());
        }
        let us = |s: &str| s.trim().parse::<i64>().map(|v| v / 1000).unwrap_or(0);
        let app = title_case(f[0].split('.').next().unwrap_or(""));
        let length = us(f[6]);
        (
            Some(MediaSnapshot::new(
                &app,
                f[2],
                f[3],
                f[4],
                f[1] == "Playing",
                us(f[5]),
                length,
                0,
                length > 0,
            )),
            f[7].to_string(),
        )
    }

    // ------------------------------------------------------------ macOS: system media, then AppleScript

    fn read_mac(&self, work: &mut Work) -> (Option<MediaSnapshot>, String) {
        let system = self.read_mac_system_media_locked(work, None, None);
        self.mac_system_media
            .store(system.0.is_some(), Ordering::SeqCst);
        if system.0.is_some() {
            system
        } else {
            read_mac_apple_script()
        }
    }

    /// The macOS system media from `nowplaying-cli get-raw` (or the JSON given), as of `sampled_at` (or now).
    pub fn read_mac_system_media(
        &self,
        supplied_json: Option<&str>,
        sampled_at: Option<Time>,
    ) -> (Option<MediaSnapshot>, String) {
        let mut work = locked(&self.work);
        self.read_mac_system_media_locked(&mut work, supplied_json, sampled_at)
    }

    fn read_mac_system_media_locked(
        &self,
        w: &mut Work,
        supplied_json: Option<&str>,
        sampled_at: Option<Time>,
    ) -> (Option<MediaSnapshot>, String) {
        let none = |w: &mut Work| {
            w.position_key.clear();
            (None, String::new())
        };
        let json = match supplied_json {
            Some(j) => Some(j.to_string()),
            None => crate::unix::sh::run("nowplaying-cli", &["get-raw"]),
        };
        let Some(json) = json.filter(|j| !j.trim().is_empty()) else {
            return none(w);
        };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&json) else {
            return none(w);
        };
        if !data.is_object() {
            return none(w);
        }
        let text = |key: &str| {
            data.get(format!("{PREFIX}{key}"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let num = |key: &str| {
            data.get(format!("{PREFIX}{key}"))
                .and_then(|v| v.as_f64())
                .filter(|n| n.is_finite())
                .unwrap_or(0.0)
        };
        let title = text("Title");
        if title.trim().is_empty() {
            return none(w);
        }
        let bundle = text("ClientBundleIdentifier");
        let mut app = match bundle.as_str() {
            "com.apple.WebKit.GPU" | "com.apple.Safari" => "Safari".to_string(),
            "com.google.Chrome" => "Chrome".into(),
            "org.mozilla.firefox" => "Firefox".into(),
            "com.apple.Music" => "Music".into(),
            "com.spotify.client" => "Spotify".into(),
            "com.github.th-ch.youtube-music" => "YouTube Music".into(),
            "" => "Mac".into(),
            other => other.rsplit('.').next().unwrap_or(other).to_string(),
        };
        let source_key = format!("{bundle}\n{title}");
        let real_now = chrono::Utc::now();
        if w.source_key != source_key
            || w.source_at
                .is_none_or(|at| real_now - at > chrono::TimeDelta::seconds(10))
        {
            w.source_key = source_key.clone();
            w.source_at = Some(real_now);
            w.source = read_youtube_source(&bundle, &title);
        }
        if !w.source.is_empty() {
            app = w.source.clone();
        }

        let duration = num("Duration").max(0.0);
        let rate = num("PlaybackRate");
        let playing = rate > 0.0;
        let raw_position = (num("ElapsedTime").max(0.0) * 1000.0) as i64;
        let position_key = format!("{source_key}\n{}", num("UniqueIdentifier"));
        let now = sampled_at.unwrap_or(real_now);
        // the position is as of now, whatever the artwork below takes
        *locked(&self.last_sample) = Some(Instant::now());
        if position_key != w.position_key {
            w.position_key = position_key.clone();
            w.position_ms = raw_position;
            w.last_raw_position_ms = raw_position;
            // a new position sequence (or the same track back after a gap): ask the player
            w.anchor_at = None;
        } else {
            let since = w.position_at.map_or(0, |at| (now - at).num_milliseconds());
            let predicted = w.position_ms + if playing { since } else { 0 };
            w.position_ms = if raw_position != w.last_raw_position_ms {
                raw_position
            } else {
                predicted
            };
            w.last_raw_position_ms = raw_position;
        }
        // ElapsedTime is only valid as of the Timestamp the source published it with (browsers publish it on play,
        // pause and seek, not while playing), so the real position is that plus the time since, at the playback rate
        if let Some(published) = mac_published_at(&data, &format!("{PREFIX}Timestamp"), now) {
            let since_ms = (now - published)
                .num_nanoseconds()
                .map_or(f64::MAX, |n| n as f64 / 1e6)
                .max(0.0);
            w.position_ms = raw_position + if playing { (since_ms * rate) as i64 } else { 0 };
            w.last_raw_position_ms = raw_position;
        } else if bundle == "com.spotify.client" || bundle == "com.apple.Music" {
            // Since macOS 15.4 these often report ElapsedTime 0 for the whole track and no Timestamp, so nothing here
            // says where playback is. The player does: ask it when the track or play/pause changes and every few
            // seconds (a seek), and count from that in between.
            if position_key != w.anchor_key
                || playing != w.anchor_playing
                || w.anchor_at.is_none_or(|at| now - at >= ANCHOR_EVERY)
            {
                w.anchor_key = position_key.clone();
                w.anchor_playing = playing;
                w.anchor_at = Some(now); // also after a refusal: it is not asked again every second
                let ask = locked(&self.app_position).clone();
                if let Some(from_player) = ask(&bundle) {
                    w.position_ms = from_player;
                    w.last_raw_position_ms = raw_position;
                }
            }
        }
        w.position_at = Some(now);
        let duration_ms = (duration * 1000.0) as i64;
        if duration_ms > 0 {
            w.position_ms = w.position_ms.min(duration_ms);
        }

        if position_key != w.art_key {
            w.art_key = position_key;
            w.art_encoded.clear();
            self.set_art(None);
        }
        let encoded = data
            .get(format!("{PREFIX}ArtworkData"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if encoded.is_empty() {
            w.art_encoded.clear();
            self.set_art(None);
        } else if encoded != w.art_encoded {
            match decode_mac_art(encoded) {
                None => {
                    w.art_encoded.clear();
                    self.set_art(None);
                }
                Some(next) => {
                    w.art_encoded = encoded.to_string();
                    self.publish_art(next);
                }
            }
        }
        (
            Some(MediaSnapshot::new(
                &app,
                &title,
                &text("Artist"),
                &text("Album"),
                playing,
                w.position_ms,
                duration_ms,
                self.current_art_id(),
                duration > 0.0,
            )),
            String::new(),
        )
    }

    /// The next reading asks the player for its position instead of counting on from the last answer.
    pub fn forget_player_position(&self) {
        locked(&self.work).anchor_at = None;
    }

    pub fn control(&self, action: &str, ms: Option<i64>) -> bool {
        let seconds = |m: i64| format!("{}", m as f64 / 1000.0);
        let ok = if cfg!(target_os = "macos") {
            if self.mac_system_media.load(Ordering::SeqCst) {
                let args: Option<Vec<String>> = match (action, ms) {
                    ("playpause", _) => Some(vec!["togglePlayPause".into()]),
                    ("next", _) => Some(vec!["next".into()]),
                    ("prev", _) => Some(vec!["previous".into()]),
                    ("seek", Some(m)) => Some(vec!["seek".into(), seconds(m)]),
                    _ => None,
                };
                args.is_some_and(|a| {
                    let a: Vec<&str> = a.iter().map(String::as_str).collect();
                    crate::unix::sh::run("nowplaying-cli", &a).is_some()
                })
            } else {
                let app = self.snapshot().map_or("Spotify".to_string(), |s| s.app);
                let cmd = match (action, ms) {
                    ("playpause", _) => Some("playpause".to_string()),
                    ("next", _) => Some("next track".into()),
                    ("prev", _) => Some("previous track".into()),
                    ("seek", Some(m)) => Some(format!("set player position to {}", seconds(m))),
                    _ => None,
                };
                cmd.is_some_and(|c| {
                    let script = format!("tell application \"{app}\" to {c}");
                    crate::unix::sh::run("osascript", &["-e", &script]).is_some()
                })
            }
        } else {
            let args: Option<Vec<String>> = match (action, ms) {
                ("playpause", _) => Some(vec!["play-pause".into()]),
                ("next", _) => Some(vec!["next".into()]),
                ("prev", _) => Some(vec!["previous".into()]),
                ("seek", Some(m)) => Some(vec!["position".into(), seconds(m)]),
                _ => None,
            };
            args.is_some_and(|a| {
                let a: Vec<&str> = a.iter().map(String::as_str).collect();
                crate::unix::sh::run("playerctl", &a).is_some()
            })
        };
        // after a seek the position is no longer where the last check put it: the poll below asks the player again
        if action == "seek" {
            self.forget_player_position();
        }
        self.poll();
        ok
    }

    pub fn pause(&self) -> bool {
        let Some(snap) = self.snapshot().filter(|s| s.playing) else {
            return false;
        };
        let ok = if cfg!(target_os = "macos") {
            if self.mac_system_media.load(Ordering::SeqCst) {
                crate::unix::sh::run("nowplaying-cli", &["pause"]).is_some()
            } else if snap.app == "Spotify" || snap.app == "Music" {
                let script = format!("tell application \"{}\" to pause", snap.app);
                crate::unix::sh::run("osascript", &["-e", &script]).is_some()
            } else {
                return false;
            }
        } else {
            crate::unix::sh::run("playerctl", &["pause"]).is_some()
        };
        self.poll();
        ok
    }

    /// A cover from the player's art URL: a local image file, or an http(s) one from a public address.
    pub fn load_art(&self, url: &str) -> Option<AlbumArt> {
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("file://") {
            let parsed = reqwest::Url::parse(url).ok()?;
            if parsed
                .host_str()
                .is_some_and(|h| !h.is_empty() && h != "localhost")
            {
                return None; // a network share
            }
            let path = parsed.to_file_path().ok()?;
            if !has_allowed_art_extension(&path) || !path.is_file() {
                return None;
            }
            let len = std::fs::metadata(&path).ok()?.len();
            if len == 0 || len > MAX_ART_BYTES as u64 {
                return None;
            }
            let bytes = std::fs::read(&path).ok()?;
            let mime = sniff_image_mime(&bytes).filter(|_| bytes.len() <= MAX_ART_BYTES)?;
            return Some(AlbumArt {
                bytes,
                mime: mime.into(),
            });
        }
        if lower.starts_with("https://") || lower.starts_with("http://") {
            let result = art_runtime().block_on(fetch_public_art(url));
            return match result {
                Ok(Some(bytes)) => sniff_image_mime(&bytes).map(|mime| AlbumArt {
                    bytes,
                    mime: mime.into(),
                }),
                Ok(None) => None,
                Err(e) => {
                    crate::log::debug(CATEGORY, &format!("cover art: {e}"));
                    None
                }
            };
        }
        None
    }
}

/// `TextInfo.ToTitleCase`: each word's first letter upper case and the rest lower case; an all-capitals word stays.
fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        if word.chars().any(char::is_lowercase) || !word.chars().any(char::is_uppercase) {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.push_str(&chars.as_str().to_lowercase());
            }
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in s.chars() {
        if c.is_alphanumeric() || c == '\'' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// The YouTube or YouTube Music tab whose title holds the track's, so a browser's media is named after the site.
fn read_youtube_source(bundle: &str, title: &str) -> String {
    let script = match bundle {
        "com.apple.WebKit.GPU" | "com.apple.Safari" => SAFARI_TABS,
        "com.google.Chrome" => CHROME_TABS,
        _ => return String::new(),
    };
    let Some(out) = crate::unix::sh::run("osascript", &["-e", script]) else {
        return String::new();
    };
    let wanted = title.to_lowercase();
    out.split('\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.trim_end_matches('\r').split_once('\t'))
        .find(|(_, name)| name.to_lowercase().contains(&wanted))
        .map(|(source, _)| source.to_string())
        .unwrap_or_default()
}

const SAFARI_TABS: &str = r#"if application "Safari" is not running then return ""
set matches to ""
tell application "Safari"
  repeat with w in windows
    repeat with t in tabs of w
      try
        set u to URL of t
        if u contains "youtube.com/" then
          set sourceName to "YouTube"
          if u contains "music.youtube.com/" then set sourceName to "YouTube Music"
          set matches to matches & sourceName & (ASCII character 9) & (name of t) & linefeed
        end if
      end try
    end repeat
  end repeat
end tell
return matches"#;

const CHROME_TABS: &str = r#"if application "Google Chrome" is not running then return ""
set matches to ""
tell application "Google Chrome"
  repeat with w in windows
    repeat with t in tabs of w
      try
        set u to URL of t
        if u contains "youtube.com/" then
          set sourceName to "YouTube"
          if u contains "music.youtube.com/" then set sourceName to "YouTube Music"
          set matches to matches & sourceName & (ASCII character 9) & (title of t) & linefeed
        end if
      end try
    end repeat
  end repeat
end tell
return matches"#;

/// The now-playing artwork: JPEG, PNG or WEBP as it is; anything else (Safari publishes TIFF, which the phone cannot
/// show) converted to PNG with sips.
fn decode_mac_art(encoded: &str) -> Option<AlbumArt> {
    use base64::Engine;
    if encoded.is_empty() || encoded.len() > 8_000_000 {
        return None;
    }
    // Convert.FromBase64String skips white space
    let compact: String = encoded
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\r' | '\n'))
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(compact)
        .ok()?;
    if let Some(mime) = sniff_image_mime(&bytes).filter(|m| *m != "image/gif") {
        return Some(AlbumArt {
            bytes,
            mime: mime.into(),
        });
    }
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let input = std::env::temp_dir().join(format!(
        "phonebox-art-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let output = crate::app_paths::with_suffix(&input, ".png");
    let converted = std::fs::write(&input, &bytes).ok().and_then(|()| {
        let (i, o) = (input.to_string_lossy(), output.to_string_lossy());
        crate::unix::sh::run_with_timeout(5000, "sips", &["-s", "format", "png", &i, "--out", &o])?;
        std::fs::read(&output).ok()
    });
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    converted.map(|bytes| AlbumArt {
        bytes,
        mime: "image/png".into(),
    })
}

/// Spotify's or Music's own "player position" (about 150 ms; never launches the app).
fn read_app_position(bundle: &str) -> Option<i64> {
    let app = if bundle == "com.spotify.client" {
        "Spotify"
    } else {
        "Music"
    };
    let script = format!(
        "if application \"{app}\" is running then tell application \"{app}\" to return (player position as string)"
    );
    let text = crate::unix::sh::run("osascript", &["-e", &script])?;
    // the decimal separator follows the Mac's language ("16,32")
    parse_seconds(&text)
        .filter(|s| *s >= 0.0)
        .map(|s| (s * 1000.0) as i64)
}

/// A number of seconds as AppleScript prints it, with either decimal separator.
fn parse_seconds(text: &str) -> Option<f64> {
    let t = text.trim().replace(',', ".");
    if t.is_empty()
        || !t
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'))
    {
        return None;
    }
    t.parse::<f64>().ok().filter(|s| s.is_finite())
}

fn read_mac_apple_script() -> (Option<MediaSnapshot>, String) {
    const SCRIPT: &str = r#"if application "Spotify" is running then
  tell application "Spotify"
    if player state is stopped then return ""
    set t to current track
    return "Spotify" & tab & (player state as string) & tab & (name of t) & tab & (artist of t) & tab & (album of t) & tab & (player position as string) & tab & ((duration of t) / 1000 as string) & tab & (artwork url of t)
  end tell
else if application "Music" is running then
  tell application "Music"
    if player state is stopped then return ""
    set t to current track
    return "Music" & tab & (player state as string) & tab & (name of t) & tab & (artist of t) & tab & (album of t) & tab & (player position as string) & tab & ((duration of t) as string) & tab & ""
  end tell
end if
return """#;
    let Some(line) = crate::unix::sh::run("osascript", &["-e", SCRIPT]) else {
        return (None, String::new());
    };
    let line = line.trim();
    if line.is_empty() {
        return (None, String::new());
    }
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() < 8 {
        return (None, String::new());
    }
    let sec = |s: &str| parse_seconds(s).map_or(0, |v| (v * 1000.0) as i64);
    (
        Some(MediaSnapshot::new(
            f[0],
            f[2],
            f[3],
            f[4],
            f[1] == "playing",
            sec(f[5]),
            sec(f[6]),
            0,
            true,
        )),
        f[7].to_string(),
    )
}

/// When the source published the position, from the now-playing "Timestamp": a date string or seconds since 1970;
/// None when it is missing, unreadable or implausible (over 5 s ahead or 12 h back).
pub fn mac_published_at(data: &serde_json::Value, key: &str, now: Time) -> Option<Time> {
    let at = match data.get(key)? {
        serde_json::Value::String(s) => parse_date(s)?,
        serde_json::Value::Number(n) => {
            let seconds = n
                .as_f64()
                .filter(|s| s.is_finite() && *s > 1e9 && *s < 1e11)?;
            chrono::DateTime::from_timestamp_millis((seconds * 1000.0) as i64)?
        }
        _ => return None,
    };
    // up to a few seconds ahead is clock noise (the date string is cut to whole seconds); further, or over half a day
    // back, means this is not a time we can rely on
    let age = now - at;
    (age >= chrono::TimeDelta::seconds(-5) && age <= chrono::TimeDelta::hours(12)).then_some(at)
}

/// The date forms `DateTimeOffset.TryParse` reads from now-playing info: ISO 8601 with or without a zone (UTC when
/// none), and NSDate's "2026-11-19 10:00:00 +0000".
fn parse_date(s: &str) -> Option<Time> {
    let s = s.trim();
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&chrono::Utc));
    }
    for f in [
        "%Y-%m-%d %H:%M:%S%.f %z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
    ] {
        if let Ok(t) = chrono::DateTime::parse_from_str(s, f) {
            return Some(t.with_timezone(&chrono::Utc));
        }
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, f) {
            return Some(t.and_utc());
        }
    }
    None
}

pub fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        Some("image/png")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

fn has_allowed_art_extension(path: &std::path::Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif"))
}

/// The body of a cover art URL; Ok(None) when the server refuses it or it is over [`MAX_ART_BYTES`]. One deadline
/// covers the request and every body read, so a server that stops mid-body cannot hold the poll.
pub async fn download_art(
    http: &reqwest::Client,
    url: &str,
    deadline: Duration,
) -> Result<Option<Vec<u8>>, String> {
    use futures_util::StreamExt;
    let work = async {
        let r = http.get(url).send().await.map_err(|e| e.to_string())?;
        if !r.status().is_success() || r.content_length().is_some_and(|n| n > MAX_ART_BYTES as u64)
        {
            return Ok(None);
        }
        let mut body = r.bytes_stream();
        let mut out = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            if out.len() + chunk.len() > MAX_ART_BYTES {
                return Ok(None);
            }
            out.extend_from_slice(&chunk);
        }
        Ok(Some(out))
    };
    tokio::time::timeout(deadline, work)
        .await
        .map_err(|_| "the cover art download timed out".to_string())?
}

/// The runtime cover art downloads run on (the monitor polls on a plain thread). Never dropped.
fn art_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("cover art runtime")
    })
}

/// Downloads cover art from public addresses only: an address-literal host must be public, and a name must resolve
/// to one (IPv4 first); no redirects, no proxy, 4 s to connect, 5 s in all.
async fn fetch_public_art(url: &str) -> Result<Option<Vec<u8>>, String> {
    static HTTP: std::sync::OnceLock<Result<reqwest::Client, String>> = std::sync::OnceLock::new();
    let parsed = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    // an address-literal host never reaches the resolver
    let literal = parsed
        .host_str()
        .map(|h| h.trim_start_matches('[').trim_end_matches(']'))
        .and_then(|h| h.parse::<std::net::IpAddr>().ok());
    if literal.is_some_and(|ip| !crate::widget_feed_proxy::is_public_address(ip)) {
        return Err("Refused non-public cover art host".into());
    }
    let http = HTTP
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(Duration::from_secs(4))
                .timeout(Duration::from_secs(5))
                .dns_resolver(Arc::new(PublicResolver))
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(Clone::clone)?;
    download_art(http, url, ART_DEADLINE).await
}

/// Resolves a cover art host to its public addresses only, IPv4 first.
struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let mut public: Vec<std::net::SocketAddr> = found
                .filter(|a| crate::widget_feed_proxy::is_public_address(a.ip()))
                .collect();
            public.sort_by_key(|a| if a.is_ipv4() { 0 } else { 1 });
            if public.is_empty() {
                return Err("Refused non-public cover art host".into());
            }
            let addrs: reqwest::dns::Addrs = Box::new(public.into_iter());
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, Utc};

    /// The "media" threads of this process by id (Linux), with their voluntary context switches: one per wake from a
    /// sleep.
    #[cfg(target_os = "linux")]
    fn media_threads() -> std::collections::HashMap<String, u64> {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .flatten()
            .filter(|t| {
                std::fs::read_to_string(t.path().join("comm")).is_ok_and(|c| c.trim() == "media")
            })
            .filter_map(|t| {
                let status = std::fs::read_to_string(t.path().join("status")).ok()?;
                let n = status
                    .lines()
                    .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))?;
                Some((
                    t.file_name().to_string_lossy().into_owned(),
                    n.trim().parse().ok()?,
                ))
            })
            .collect()
    }

    // Between polls the thread waits once (C#'s Task.Delay(1000, ct)); it used to wake ten times a second to look for
    // the stop, which made the idle agent wake three times as often as C# on the MacBook resource run.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_media_thread_wakes_once_a_second_between_polls() {
        let before: std::collections::HashSet<String> = media_threads().into_keys().collect();
        let stop = crate::server::Stop::default();
        let thread = MediaMonitor::new().start(stop.clone());
        std::thread::sleep(Duration::from_millis(300)); // past the first poll
        let mine = |all: std::collections::HashMap<String, u64>| -> u64 {
            all.into_iter()
                .filter(|(id, _)| !before.contains(id))
                .map(|(_, n)| n)
                .sum()
        };
        let start = mine(media_threads());
        std::thread::sleep(Duration::from_secs(2));
        let woke = mine(media_threads()) - start;
        stop.stop();
        let stopped_at = Instant::now();
        thread.join().unwrap();
        assert!(
            stopped_at.elapsed() < Duration::from_secs(2),
            "the stop ends the wait at once"
        );
        assert!(woke <= 8, "the media thread woke {woke} times in 2 s");
    }

    fn monitor() -> Arc<MediaMonitor> {
        let m = MediaMonitor::new();
        m.set_app_position(|_| None); // never the Spotify that may be playing here
        m
    }

    fn pos(m: &MediaMonitor, json: &str, at: Time) -> i64 {
        m.read_mac_system_media(Some(json), Some(at))
            .0
            .unwrap()
            .pos_ms
    }

    const TRACK: &str = r#"{"kMRMediaRemoteNowPlayingInfoTitle":"Track","kMRMediaRemoteNowPlayingInfoClientBundleIdentifier":"test.player","kMRMediaRemoteNowPlayingInfoDuration":120,"kMRMediaRemoteNowPlayingInfoElapsedTime":10,"kMRMediaRemoteNowPlayingInfoPlaybackRate":1}"#;

    fn stamped(elapsed: f64, rate: i32, stamp: &str) -> String {
        format!(
            r#"{{"kMRMediaRemoteNowPlayingInfoTitle":"Track","kMRMediaRemoteNowPlayingInfoClientBundleIdentifier":"test.player","kMRMediaRemoteNowPlayingInfoDuration":600,"kMRMediaRemoteNowPlayingInfoElapsedTime":{elapsed},"kMRMediaRemoteNowPlayingInfoPlaybackRate":{rate},"kMRMediaRemoteNowPlayingInfoTimestamp":{stamp}}}"#
        )
    }

    fn iso(t: Time) -> String {
        format!("\"{}\"", t.format("%Y-%m-%dT%H:%M:%SZ"))
    }

    fn spotify(duration: i32, elapsed: f64) -> String {
        format!(
            r#"{{"kMRMediaRemoteNowPlayingInfoTitle":"Track","kMRMediaRemoteNowPlayingInfoClientBundleIdentifier":"com.spotify.client","kMRMediaRemoteNowPlayingInfoDuration":{duration},"kMRMediaRemoteNowPlayingInfoElapsedTime":{elapsed},"kMRMediaRemoteNowPlayingInfoPlaybackRate":1}}"#
        )
    }

    fn paused(json: &str) -> String {
        let out = json.replace("PlaybackRate\":1", "PlaybackRate\":0");
        assert_ne!(out, json);
        out
    }

    #[test]
    fn system_metadata_advances_from_the_reported_position_and_stops_when_paused() {
        let m = monitor();
        let now = Utc::now();
        assert_eq!(pos(&m, TRACK, now), 10000);
        assert_eq!(pos(&m, TRACK, now + TimeDelta::seconds(2)), 12000);
        let p = m
            .read_mac_system_media(Some(&paused(TRACK)), Some(now + TimeDelta::seconds(4)))
            .0
            .unwrap();
        assert!(!p.playing);
        assert_eq!(p.pos_ms, 12000);
    }

    #[test]
    fn position_is_extrapolated_from_when_the_source_published_it() {
        // a browser published 0:00 when playback began and never refreshed it: the phone must show 0:15, not 0:00
        let now = Utc::now();
        let published = now - TimeDelta::seconds(15);
        let epoch = format!("{}", published.timestamp_millis() as f64 / 1000.0);
        for stamp in [iso(published), epoch] {
            let p = pos(&monitor(), &stamped(0.0, 1, &stamp), now);
            assert!((14000..=16000).contains(&p), "{stamp} {p}"); // the ISO form is whole seconds
        }
    }

    #[test]
    fn the_published_position_is_kept_while_paused_or_when_the_timestamp_cannot_be_trusted() {
        let now = Utc::now();
        let old = iso(now - TimeDelta::seconds(15));
        assert_eq!(pos(&monitor(), &stamped(7.0, 0, &old), now), 7000);
        // a timestamp in the future, or one that is not a date at all, is ignored
        let future = iso(now + TimeDelta::minutes(10));
        for stamp in [future.as_str(), "\"soon\"", "12"] {
            assert_eq!(
                pos(&monitor(), &stamped(7.0, 1, stamp), now),
                7000,
                "{stamp}"
            );
        }
    }

    #[test]
    fn a_sample_reports_its_age_to_the_phone_without_exposing_the_clock_reading() {
        let fresh = MediaSnapshot::new("App", "T", "A", "", true, 5000, 60000, 0, true);
        assert_eq!(fresh.age_ms(), 0); // radio and other undated snapshots
        let aged = MediaSnapshot {
            sampled_at: Some(Instant::now() - Duration::from_millis(1500)),
            ..fresh
        };
        assert!((1500..=1700).contains(&aged.age_ms()));
        let json = serde_json::to_string(&aged).unwrap();
        assert!(json.contains("\"ageMs\":"));
        assert!(!json.to_lowercase().contains("sampledat"));
    }

    #[test]
    fn spotify_and_music_anchor_on_the_players_own_position_when_mac_os_reports_none() {
        // macOS 26: Spotify reports ElapsedTime 0 for the whole track and no Timestamp; the player knows
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let answer = Arc::new(std::sync::atomic::AtomicI64::new(15_000));
        let m = MediaMonitor::new();
        {
            let (asked, answer) = (asked.clone(), answer.clone());
            m.set_app_position(move |b| {
                asked.lock().unwrap().push(b.to_string());
                Some(answer.load(std::sync::atomic::Ordering::SeqCst))
            });
        }
        let json = spotify(200, 0.0);
        let t0 = Utc::now();
        assert_eq!(pos(&m, &json, t0), 15_000);
        assert_eq!(pos(&m, &json, t0 + TimeDelta::seconds(2)), 17_000); // counted, not asked again
        assert_eq!(asked.lock().unwrap().len(), 1);
        answer.store(31_000, std::sync::atomic::Ordering::SeqCst); // a seek, noticed at the next check
        assert_eq!(pos(&m, &json, t0 + TimeDelta::seconds(5)), 31_000);
        assert_eq!(asked.lock().unwrap().len(), 2);
        // pausing is noticed at once and the position holds
        let p = m
            .read_mac_system_media(Some(&paused(&json)), Some(t0 + TimeDelta::seconds(6)))
            .0
            .unwrap();
        assert!(!p.playing);
        assert_eq!(p.pos_ms, 31_000);
        assert_eq!(asked.lock().unwrap().len(), 3);
        assert_eq!(asked.lock().unwrap()[0], "com.spotify.client");
    }

    fn counting(
        answer: Arc<std::sync::atomic::AtomicI64>,
    ) -> (Arc<MediaMonitor>, Arc<std::sync::atomic::AtomicUsize>) {
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let m = MediaMonitor::new();
        let counter = asked.clone();
        m.set_app_position(move |_| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(answer.load(std::sync::atomic::Ordering::SeqCst))
        });
        (m, asked)
    }

    #[test]
    fn after_a_seek_the_player_is_asked_again_at_once_not_at_the_next_check() {
        use std::sync::atomic::Ordering::SeqCst;
        let answer = Arc::new(std::sync::atomic::AtomicI64::new(20_000));
        let (m, asked) = counting(answer.clone());
        let json = spotify(300, 0.0);
        let t0 = Utc::now();
        assert_eq!(pos(&m, &json, t0), 20_000);
        assert_eq!(pos(&m, &json, t0 + TimeDelta::seconds(1)), 21_000);
        assert_eq!(asked.load(SeqCst), 1);
        answer.store(150_000, SeqCst); // the phone seeked to 2:30
        m.forget_player_position();
        assert_eq!(pos(&m, &json, t0 + TimeDelta::seconds(2)), 150_000);
        assert_eq!(asked.load(SeqCst), 2);
    }

    #[test]
    fn a_track_that_comes_back_after_a_gap_is_asked_about_again_at_once() {
        use std::sync::atomic::Ordering::SeqCst;
        let answer = Arc::new(std::sync::atomic::AtomicI64::new(40_000));
        let (m, asked) = counting(answer.clone());
        let json = spotify(300, 0.0);
        let t0 = Utc::now();
        assert_eq!(pos(&m, &json, t0), 40_000);
        assert!(
            m.read_mac_system_media(Some("{}"), Some(t0 + TimeDelta::seconds(1)))
                .0
                .is_none()
        );
        answer.store(44_000, SeqCst);
        assert_eq!(pos(&m, &json, t0 + TimeDelta::seconds(2)), 44_000); // not 0
        assert_eq!(asked.load(SeqCst), 2);
    }

    #[test]
    fn the_position_is_dated_when_it_is_computed_not_after_the_artwork_is_decoded() {
        let m = monitor();
        let before = Instant::now();
        m.read_mac_system_media(Some(TRACK), None);
        let after = Instant::now();
        let at = m.last_sample().unwrap();
        assert!(before <= at && at <= after);
    }

    #[test]
    fn when_the_player_cannot_be_asked_the_position_still_advances_and_other_players_are_never_asked()
     {
        use std::sync::atomic::Ordering::SeqCst;
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let m = MediaMonitor::new();
        let counter = asked.clone();
        m.set_app_position(move |_| {
            counter.fetch_add(1, SeqCst);
            None
        });
        let json = spotify(200, 4.0);
        let t0 = Utc::now();
        assert_eq!(pos(&m, &json, t0), 4000);
        assert_eq!(pos(&m, &json, t0 + TimeDelta::seconds(3)), 7000);
        assert_eq!(asked.load(SeqCst), 1); // a refusal is not retried every second

        let other = MediaMonitor::new();
        let counter = asked.clone();
        other.set_app_position(move |_| {
            counter.fetch_add(1, SeqCst);
            Some(99_000)
        });
        assert_eq!(
            pos(
                &other,
                &json.replace("com.spotify.client", "com.google.Chrome"),
                t0
            ),
            4000
        );
        assert_eq!(asked.load(SeqCst), 1);
    }

    #[test]
    fn extrapolated_position_never_passes_the_duration() {
        let now = Utc::now();
        let stamp = iso(now - TimeDelta::minutes(30));
        assert_eq!(pos(&monitor(), &stamped(5.0, 1, &stamp), now), 600000);
    }

    #[test]
    fn missing_or_malformed_metadata_is_unavailable() {
        for json in [
            "[]",
            "{\"kMRMediaRemoteNowPlayingInfoTitle\":42}",
            "not json",
            "",
            "  ",
        ] {
            assert!(
                monitor()
                    .read_mac_system_media(Some(json), None)
                    .0
                    .is_none(),
                "{json}"
            );
        }
    }

    #[test]
    fn known_players_are_named_after_their_bundle() {
        for (bundle, expected) in [
            ("com.spotify.client", "Spotify"),
            ("com.github.th-ch.youtube-music", "YouTube Music"),
            ("org.example.player", "player"),
            ("com.apple.WebKit.GPU", "Safari"),
            ("", "Mac"),
        ] {
            let json = format!(
                r#"{{"kMRMediaRemoteNowPlayingInfoTitle":"Track","kMRMediaRemoteNowPlayingInfoClientBundleIdentifier":"{bundle}","kMRMediaRemoteNowPlayingInfoPlaybackRate":1}}"#
            );
            if bundle == "com.apple.WebKit.GPU" && cfg!(target_os = "macos") {
                continue; // that looks through Safari's tabs on a Mac
            }
            assert_eq!(
                monitor()
                    .read_mac_system_media(Some(&json), None)
                    .0
                    .unwrap()
                    .app,
                expected
            );
        }
    }

    #[test]
    fn delayed_mac_artwork_refresh_updates_art_id_and_clears_when_missing() {
        use base64::Engine;
        let m = monitor();
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let png_one = b64(&[0x89, 0x50, 0x4e, 0x47, 1, 2, 3, 4]);
        let png_two = b64(&[0x89, 0x50, 0x4e, 0x47, 5, 6, 7, 8]);
        let with = |art: &str| {
            let extra = if art.is_empty() {
                String::new()
            } else {
                format!(r#" ,"kMRMediaRemoteNowPlayingInfoArtworkData":"{art}" "#)
            };
            TRACK.replace("PlaybackRate\":1}", &format!("PlaybackRate\":1{extra}}}"))
        };
        let read = |json: String| m.read_mac_system_media(Some(&json), None).0.unwrap();

        assert_eq!(read(with("")).art_id, 0);
        assert!(m.art().is_none());
        assert_eq!(read(with(&png_one)).art_id, 1);
        assert_eq!(m.art().unwrap().mime, "image/png");
        assert_eq!(read(with(&png_two)).art_id, 2);
        assert_eq!(read(with("not-valid-base64!!!")).art_id, 0);
        assert!(m.art().is_none());
        assert_eq!(read(with(&png_two)).art_id, 3);
        assert_eq!(read(with("")).art_id, 0);
        assert!(m.art().is_none());
    }

    #[test]
    fn load_art_rejects_non_images_and_loopback_and_system_media_honors_small_seeks() {
        let dir = tempfile::tempdir().unwrap();
        let loopback = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        loopback.set_nonblocking(true).unwrap();
        let port = loopback.local_addr().unwrap().port();
        let m = monitor();
        let file_url = |p: &std::path::Path| reqwest::Url::from_file_path(p).unwrap().to_string();

        let valid_png = dir.path().join("cover.png");
        std::fs::write(&valid_png, [0x89, 0x50, 0x4e, 0x47, 1, 2, 3, 4]).unwrap();
        assert_eq!(m.load_art(&file_url(&valid_png)).unwrap().mime, "image/png");

        let secret = dir.path().join("config.json");
        std::fs::write(&secret, "{\"token\":\"secret\"}").unwrap();
        assert!(m.load_art(&file_url(&secret)).is_none());

        let disguised = dir.path().join("fake.png");
        std::fs::write(&disguised, "{\"not\":\"an-image\"}").unwrap();
        assert!(m.load_art(&file_url(&disguised)).is_none());

        // nothing accepts on the listener, so a connection that got through stays queued for accept to see
        for url in [
            format!("http://127.0.0.1:{port}/art.png"),
            format!("http://[::1]:{port}/art.png"),
            format!("http://localhost:{port}/art.png"),
        ] {
            assert!(m.load_art(&url).is_none(), "{url}");
        }
        assert!(
            loopback.accept().is_err(),
            "a loopback connection got through"
        );

        let t0 = Utc::now();
        let json10 = spotify(120, 10.0);
        let json102 = spotify(120, 10.2);
        assert_eq!(pos(&m, &json10, t0), 10000);
        assert_eq!(pos(&m, &json10, t0 + TimeDelta::seconds(30)), 40000);
        assert_eq!(pos(&m, &json102, t0 + TimeDelta::seconds(31)), 10200);
        assert!(
            m.read_mac_system_media(Some(""), Some(t0 + TimeDelta::seconds(32)))
                .0
                .is_none()
        );
        assert_eq!(pos(&m, &json102, t0 + TimeDelta::seconds(52)), 10200);
    }

    #[tokio::test]
    async fn artwork_download_deadline_covers_a_stalled_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let serving = tokio::spawn(async move {
            let (mut c, _) = server.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            if c.read(&mut buf).await.unwrap_or(0) == 0 {
                return;
            }
            // the headers and the first bytes of a PNG, then nothing more until the test ends
            c.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 4096\r\n\r\n",
            )
            .await
            .unwrap();
            c.write_all(&[0x89, 0x50, 0x4e, 0x47]).await.unwrap();
            let _ = release_rx.await;
        });
        let http = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/art.png");
        let download = download_art(&http, &url, Duration::from_millis(300));
        let result = tokio::time::timeout(Duration::from_secs(10), download)
            .await
            .expect("the deadline ended the download");
        assert!(result.is_err());
        let _ = release_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), serving).await;
    }

    #[tokio::test]
    async fn a_download_answers_the_body_up_to_the_cap() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(async move {
            for body in [&b"\x89PNG"[..], b""] {
                let (mut c, _) = server.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf).await;
                let head = if body.is_empty() {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
                } else {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                };
                c.write_all(head.as_bytes()).await.unwrap();
                c.write_all(body).await.unwrap();
            }
        });
        let http = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/art.png");
        assert_eq!(
            download_art(&http, &url, ART_DEADLINE).await,
            Ok(Some(b"\x89PNG".to_vec()))
        );
        assert_eq!(download_art(&http, &url, ART_DEADLINE).await, Ok(None));
    }

    #[test]
    fn image_types_are_sniffed_from_their_first_bytes() {
        assert_eq!(sniff_image_mime(&[0xff, 0xd8, 0xff, 0]), Some("image/jpeg"));
        assert_eq!(
            sniff_image_mime(&[0x89, 0x50, 0x4e, 0x47]),
            Some("image/png")
        );
        assert_eq!(sniff_image_mime(b"RIFF\0\0\0\0WEBP"), Some("image/webp"));
        assert_eq!(sniff_image_mime(b"GIF89a.."), Some("image/gif"));
        assert_eq!(sniff_image_mime(b"GIF87a"), Some("image/gif"));
        assert_eq!(sniff_image_mime(b"<svg>"), None);
        assert_eq!(sniff_image_mime(&[]), None);
    }

    #[test]
    fn published_times_are_dates_or_unix_seconds_within_reason() {
        let now = Utc::now();
        let at = |v: serde_json::Value| mac_published_at(&serde_json::json!({ "t": v }), "t", now);
        let ten_ago = now - TimeDelta::seconds(10);
        assert_eq!(
            at(serde_json::json!(
                ten_ago.format("%Y-%m-%dT%H:%M:%SZ").to_string()
            ))
            .map(|t| t.timestamp()),
            Some(ten_ago.timestamp())
        );
        assert_eq!(
            at(serde_json::json!(
                ten_ago.format("%Y-%m-%d %H:%M:%S +0000").to_string()
            ))
            .map(|t| t.timestamp()),
            Some(ten_ago.timestamp())
        );
        assert_eq!(
            at(serde_json::json!(
                ten_ago.timestamp_millis() as f64 / 1000.0
            ))
            .map(|t| t.timestamp_millis()),
            Some(ten_ago.timestamp_millis())
        );
        assert!(at(serde_json::json!(ten_ago.timestamp_millis())).is_none()); // milliseconds are not guessed
        assert!(at(serde_json::json!((now - TimeDelta::hours(13)).timestamp())).is_none());
        assert!(at(serde_json::json!(true)).is_none());
        assert!(mac_published_at(&serde_json::json!({}), "t", now).is_none());
    }
}
