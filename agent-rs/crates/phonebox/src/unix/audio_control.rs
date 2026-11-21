//! Port of `Unix/AudioControl.cs`: the volume cache behind `audio` in the state (never waits for a helper; refreshed in
//! the background) and the Linux (`wpctl` / `pactl`) and macOS (CoreAudio, `osascript`, `m1ddc`) reads and writes. The
//! cache also serves fake readers in tests on every OS.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crate::audio_control::{AudioSnapshot, ReadFn, SetFn, ToggleFn, muted_after_set};

const MAX_AGE_MS: i64 = 900;
/// A busy computer can need seconds; giving up kills the helper and nothing is set.
const SET_TIMEOUT_MS: u64 = 8000;

#[derive(Default)]
struct Inner {
    cached: Option<Arc<AudioSnapshot>>,
    /// When `cached` was read; 0 = never read, or invalidated.
    at: i64,
    /// Bumped by remember / invalidate, so a read from before a volume change cannot land after it.
    generation: u64,
    /// Read tickets: a reading lands only if no later-started read has landed.
    seq: u64,
    written: u64,
}

struct Shared {
    inner: Mutex<Inner>,
    refreshing: AtomicBool,
    epoch: Instant,
    read: ReadFn,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> i64 {
        self.epoch.elapsed().as_millis() as i64 + 1
    }

    fn begin_read(&self) -> (u64, u64) {
        let mut i = self.lock();
        i.seq += 1;
        (i.seq, i.generation)
    }

    fn store(&self, ticket: u64, generation: u64, s: Option<AudioSnapshot>) {
        let changed = {
            let mut i = self.lock();
            if i.generation != generation || ticket <= i.written {
                return;
            }
            // the volume moved from outside (the keyboard's keys, another app): the phone should see it now
            let changed = matches!((&i.cached, &s), (Some(old), Some(new)) if old.volume != new.volume || old.muted != new.muted);
            i.written = ticket;
            i.cached = s.map(Arc::new);
            i.at = self.now();
            changed
        };
        if changed {
            crate::state_signal::poke();
        }
    }
}

pub struct Cache {
    shared: Arc<Shared>,
    set: Option<SetFn>,
    toggle: Option<ToggleFn>,
    /// Control actions run one at a time, each with its cache update inside.
    control_gate: Mutex<()>,
}

impl Cache {
    pub fn new(read: ReadFn, set: Option<SetFn>, toggle: Option<ToggleFn>) -> Cache {
        Cache {
            shared: Arc::new(Shared {
                inner: Mutex::default(),
                refreshing: AtomicBool::new(false),
                epoch: Instant::now(),
                read,
            }),
            set,
            toggle,
            control_gate: Mutex::new(()),
        }
    }

    /// The system's reader, setter and mute toggle.
    pub fn system() -> Cache {
        Cache::new(Arc::new(system_read), None, None)
    }

    pub fn refreshing(&self) -> bool {
        self.shared.refreshing.load(Ordering::Acquire)
    }

    /// The last reading, refreshed in the background: the live stream asks every second for every phone, and under
    /// load the helpers can take seconds each.
    pub fn snapshot(&self) -> Option<AudioSnapshot> {
        let (at, cached) = {
            let i = self.shared.lock();
            (i.at, i.cached.clone())
        };
        // never read (at 0) counts as stale too: C# compares against the system's uptime, always far past 0
        if (at == 0 || self.shared.now() - at >= MAX_AGE_MS)
            && self
                .shared
                .refreshing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let shared = self.shared.clone();
            std::thread::spawn(move || {
                let (ticket, generation) = shared.begin_read();
                let s = (shared.read)();
                shared.store(ticket, generation, s);
                shared.refreshing.store(false, Ordering::Release);
            });
        }
        cached.map(|c| c.as_ref().clone())
    }

    /// A reading for a control action: the cached one while fresh, otherwise read now.
    pub fn current(&self) -> Option<AudioSnapshot> {
        {
            let i = self.shared.lock();
            if i.at != 0 && self.shared.now() - i.at < MAX_AGE_MS {
                return i.cached.as_ref().map(|c| c.as_ref().clone());
            }
        }
        let (ticket, generation) = self.shared.begin_read();
        let s = (self.shared.read)();
        self.shared.store(ticket, generation, s.clone());
        s
    }

    /// The last reading, or None when there is none or it was invalidated.
    pub fn known_reading(&self) -> Option<AudioSnapshot> {
        self.known_arc().map(|c| c.as_ref().clone())
    }

    fn known_arc(&self) -> Option<Arc<AudioSnapshot>> {
        let i = self.shared.lock();
        if i.at != 0 { i.cached.clone() } else { None }
    }

    fn remember_locked(&self, i: &mut Inner, s: AudioSnapshot) {
        i.cached = Some(Arc::new(s));
        i.at = self.shared.now();
        i.generation += 1;
    }

    fn invalidate_locked(i: &mut Inner) {
        i.at = 0;
        i.generation += 1;
    }

    /// Whether the output can be controlled, from the last reading however old; only a missing or negative reading is
    /// read again.
    fn can_control_now(&self) -> bool {
        let known = self.shared.lock().cached.clone();
        known.is_some_and(|k| k.can_control) || self.current().is_some_and(|c| c.can_control)
    }

    pub fn set_volume(&self, level: i32) -> bool {
        if cfg!(target_os = "macos") && !self.can_control_now() {
            return false;
        }
        let level = level.clamp(0, 100);
        let _gate = self.control_gate.lock().unwrap_or_else(|e| e.into_inner());
        // the level the write reached: the request, unless the output could not get there
        let reached = match &self.set {
            Some(set) => set(level),
            None => system_set(level),
        };
        let before = self.known_reading();
        let mut i = self.shared.lock();
        match (
            reached,
            muted_after_set(self.set.is_some() || clears_mute(level), before.as_ref()),
        ) {
            (Some(now), Some(muted)) => {
                self.remember_locked(&mut i, AudioSnapshot::new(now, muted, true))
            }
            // a write that leaves mute alone with no earlier reading says nothing about mute: read it next time
            _ => Self::invalidate_locked(&mut i),
        }
        reached.is_some()
    }

    pub fn step(&self, delta: i32) -> bool {
        self.current()
            .is_some_and(|s| self.set_volume(s.volume + delta))
    }

    pub fn toggle_mute(&self) -> bool {
        if cfg!(target_os = "macos") && !self.current().is_some_and(|c| c.can_control) {
            return false;
        }
        let _gate = self.control_gate.lock().unwrap_or_else(|e| e.into_inner());
        // the reading the flip starts from is taken before the toggle: a read that lands meanwhile may include it
        let before = self.known_arc();
        let ok = match &self.toggle {
            Some(toggle) => toggle(),
            None => system_toggle(),
        };
        let mut i = self.shared.lock();
        match before {
            Some(b) if ok && i.at != 0 && i.cached.as_ref().is_some_and(|c| Arc::ptr_eq(c, &b)) => {
                let flipped = AudioSnapshot::new(b.volume, !b.muted, b.can_control);
                self.remember_locked(&mut i, flipped);
            }
            _ => Self::invalidate_locked(&mut i),
        }
        ok
    }
}

/// Whether the write `system_set` makes for this level also unmutes: macOS always does, wpctl above 0.
#[cfg_attr(windows, allow(dead_code))]
fn clears_mute(level: i32) -> bool {
    cfg!(target_os = "macos") || (crate::unix::sh::which("wpctl").is_some() && level > 0)
}

#[cfg(not(windows))]
use crate::unix::{mac_audio_probe, sh};

/// Sets the volume on this system; the level reached, or None when it failed.
fn system_set(level: i32) -> Option<i32> {
    #[cfg(not(windows))]
    {
        if cfg!(target_os = "macos") {
            // in-process; osascript only for an output CoreAudio cannot control
            if let Some(reached) = mac_audio_probe::set_volume(level) {
                return Some(reached);
            }
            let script = format!("set volume output volume {level} without output muted");
            return sh::run_with_timeout(SET_TIMEOUT_MS, "osascript", &["-e", &script])
                .map(|_| level);
        }
        let ok = if sh::which("wpctl").is_some() {
            let value = format!("{:.2}", f64::from(level) / 100.0);
            sh::run_with_timeout(
                SET_TIMEOUT_MS,
                "wpctl",
                &["set-volume", "@DEFAULT_AUDIO_SINK@", &value],
            )
            .is_some()
                && (level == 0
                    || sh::run_with_timeout(
                        SET_TIMEOUT_MS,
                        "wpctl",
                        &["set-mute", "@DEFAULT_AUDIO_SINK@", "0"],
                    )
                    .is_some())
        } else {
            let value = format!("{level}%");
            sh::run_with_timeout(
                SET_TIMEOUT_MS,
                "pactl",
                &["set-sink-volume", "@DEFAULT_SINK@", &value],
            )
            .is_some()
        };
        ok.then_some(level)
    }
    #[cfg(windows)]
    {
        let _ = (level, SET_TIMEOUT_MS);
        None
    }
}

fn system_toggle() -> bool {
    #[cfg(not(windows))]
    {
        if cfg!(target_os = "macos") {
            return mac_audio_probe::toggle_mute()
                || sh::run_with_timeout(
                    SET_TIMEOUT_MS,
                    "osascript",
                    &[
                        "-e",
                        "set volume output muted (not (output muted of (get volume settings)))",
                    ],
                )
                .is_some();
        }
        if sh::which("wpctl").is_some() {
            sh::run("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]).is_some()
        } else {
            sh::run("pactl", &["set-sink-mute", "@DEFAULT_SINK@", "toggle"]).is_some()
        }
    }
    #[cfg(windows)]
    {
        false
    }
}

/// The volume and mute state of the default output, or None when none can be read.
fn system_read() -> Option<AudioSnapshot> {
    #[cfg(not(windows))]
    {
        if cfg!(target_os = "macos") {
            let output = mac_audio_probe::selected_output();
            // a controllable output is read in-process; osascript is for the rest (a display's own volume…)
            if let Some(o) = &output
                && let (Some(v), Some(m)) = (o.volume, o.muted)
                && mac_audio_probe::can_control(o.id)
            {
                return Some(AudioSnapshot::new(v, m, true));
            }
            let script = "set v to get volume settings\nreturn ((output volume of v) as string) & \",\" & ((output muted of v) as string)";
            if let Some(text) = sh::run("osascript", &["-e", script])
                && let Some((v, m)) = parse_osascript_volume(text.trim())
            {
                let can = output
                    .as_ref()
                    .is_some_and(|o| mac_audio_probe::can_control(o.id));
                return Some(AudioSnapshot::new(v, m, can));
            }
            let o = output?;
            let level = o
                .volume
                .or_else(|| mac_audio_probe::display_volume(o.id, &o.name))?;
            return Some(AudioSnapshot::new(level, o.muted.unwrap_or(false), false));
        }
        if sh::which("wpctl").is_some()
            && let Some(s) = parse_wpctl(
                &sh::run("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]).unwrap_or_default(),
            )
        {
            return Some(s);
        }
        let volume = parse_pactl_volume(
            &sh::run("pactl", &["get-sink-volume", "@DEFAULT_SINK@"]).unwrap_or_default(),
        )?;
        let muted = sh::run("pactl", &["get-sink-mute", "@DEFAULT_SINK@"])
            .unwrap_or_default()
            .contains("yes");
        Some(AudioSnapshot::new(volume, muted, true))
    }
    #[cfg(windows)]
    {
        None
    }
}

/// "Volume: 0.45 [MUTED]" from `wpctl get-volume`.
#[cfg_attr(windows, allow(dead_code))]
fn parse_wpctl(text: &str) -> Option<AudioSnapshot> {
    let re = regex::Regex::new(r"Volume:\s*([\d.]+)(\s*\[MUTED\])?").expect("regex");
    let c = re.captures(text)?;
    let v: f64 = c[1].parse().ok()?;
    Some(AudioSnapshot::new(
        (v * 100.0).round_ties_even() as i32,
        c.get(2).is_some(),
        true,
    ))
}

/// The first "N%" of `pactl get-sink-volume`.
#[cfg_attr(windows, allow(dead_code))]
fn parse_pactl_volume(text: &str) -> Option<i32> {
    let re = regex::Regex::new(r"(\d+)%").expect("regex");
    re.captures(text)?[1].parse().ok()
}

/// "45,false" from AppleScript's volume settings (`int.TryParse`, `bool.TryParse`).
#[cfg_attr(windows, allow(dead_code))]
fn parse_osascript_volume(text: &str) -> Option<(i32, bool)> {
    let parts: Vec<&str> = text.split(',').collect();
    let [v, m] = parts[..] else { return None };
    let v = v.trim().parse().ok()?;
    let m = match m.trim().to_lowercase().as_str() {
        "true" => true,
        "false" => false,
        _ => return None,
    };
    Some((v, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_output_is_read_as_the_csharp_code_reads_it() {
        assert_eq!(
            parse_wpctl("Volume: 0.45 [MUTED]\n"),
            Some(AudioSnapshot::new(45, true, true))
        );
        assert_eq!(
            parse_wpctl("Volume: 0.125"),
            Some(AudioSnapshot::new(12, false, true))
        ); // half to even
        assert_eq!(parse_wpctl("nothing"), None);
        assert_eq!(
            parse_pactl_volume(
                "Volume: front-left: 32768 /  50% / -18.06 dB,   front-right: 32768 /  51%"
            ),
            Some(50)
        );
        assert_eq!(parse_pactl_volume(""), None);
        assert_eq!(parse_osascript_volume("45,false"), Some((45, false)));
        assert_eq!(parse_osascript_volume("45, True"), Some((45, true)));
        assert_eq!(parse_osascript_volume("missing value,false"), None);
    }
}
