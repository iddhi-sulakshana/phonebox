//! Port of `AudioControl.cs` (Windows Core Audio) and the facade over `Unix/AudioControl.cs`: the master volume of
//! the default output, as `audio` in the state and for `/api/volume`.

use std::sync::Arc;

use serde::Serialize;

/// `AudioSnapshot`: `canControl` false when the output's volume cannot be set from here (a display on macOS).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSnapshot {
    pub volume: i32,
    pub muted: bool,
    pub can_control: bool,
}

impl AudioSnapshot {
    pub fn new(volume: i32, muted: bool, can_control: bool) -> AudioSnapshot {
        AudioSnapshot {
            volume,
            muted,
            can_control,
        }
    }
}

pub type ReadFn = Arc<dyn Fn() -> Option<AudioSnapshot> + Send + Sync>;
pub type SetFn = Arc<dyn Fn(i32) -> Option<i32> + Send + Sync>;
pub type ToggleFn = Arc<dyn Fn() -> bool + Send + Sync>;

pub struct AudioControl {
    backend: Backend,
}

enum Backend {
    /// Linux and macOS (and fakes in tests): the background-refreshed cache of `Unix/AudioControl.cs`.
    Cached(crate::unix::audio_control::Cache),
    /// Windows: Core Audio reads are in-process and fast, so there is no cache.
    #[cfg(windows)]
    Endpoint,
}

impl AudioControl {
    pub fn new() -> Arc<AudioControl> {
        #[cfg(windows)]
        let backend = Backend::Endpoint;
        #[cfg(not(windows))]
        let backend = Backend::Cached(crate::unix::audio_control::Cache::system());
        Arc::new(AudioControl { backend })
    }

    /// For tests: a fake reader (and optionally setter and mute toggle) instead of the system's.
    pub fn with(read: ReadFn, set: Option<SetFn>, toggle: Option<ToggleFn>) -> Arc<AudioControl> {
        Arc::new(AudioControl {
            backend: Backend::Cached(crate::unix::audio_control::Cache::new(read, set, toggle)),
        })
    }

    /// The reading for the state: never waits for a helper (Linux, macOS).
    pub fn snapshot(&self) -> Option<AudioSnapshot> {
        match &self.backend {
            Backend::Cached(c) => c.snapshot(),
            #[cfg(windows)]
            Backend::Endpoint => win::snapshot(),
        }
    }

    /// A reading for a control action (may wait for the helper on Linux and macOS).
    pub fn current(&self) -> Option<AudioSnapshot> {
        match &self.backend {
            Backend::Cached(c) => c.current(),
            #[cfg(windows)]
            Backend::Endpoint => win::snapshot(),
        }
    }

    pub fn set_volume(&self, level: i32) -> bool {
        match &self.backend {
            Backend::Cached(c) => c.set_volume(level),
            #[cfg(windows)]
            Backend::Endpoint => win::set_volume(level),
        }
    }

    pub fn step(&self, delta: i32) -> bool {
        match &self.backend {
            Backend::Cached(c) => c.step(delta),
            #[cfg(windows)]
            Backend::Endpoint => win::snapshot().is_some_and(|s| win::set_volume(s.volume + delta)),
        }
    }

    pub fn toggle_mute(&self) -> bool {
        match &self.backend {
            Backend::Cached(c) => c.toggle_mute(),
            #[cfg(windows)]
            Backend::Endpoint => win::toggle_mute(),
        }
    }

    /// The last reading, or None when there is none or it was invalidated (the cache only).
    pub fn known_reading(&self) -> Option<AudioSnapshot> {
        match &self.backend {
            Backend::Cached(c) => c.known_reading(),
            #[cfg(windows)]
            Backend::Endpoint => win::snapshot(),
        }
    }

    /// Whether a background read is running (the cache only).
    pub fn refreshing(&self) -> bool {
        match &self.backend {
            Backend::Cached(c) => c.refreshing(),
            #[cfg(windows)]
            Backend::Endpoint => false,
        }
    }
}

impl crate::state::StateSource for AudioControl {
    fn snapshot(&self) -> serde_json::Value {
        serde_json::to_value(AudioControl::snapshot(self)).unwrap_or(serde_json::Value::Null)
    }
}

/// The mute state a volume write leaves behind: a write that also clears mute leaves it unmuted; one that only sets
/// the volume leaves it as it was, unknown (None) when there was no earlier reading.
pub fn muted_after_set(clears_mute: bool, before: Option<&AudioSnapshot>) -> Option<bool> {
    if clears_mute {
        Some(false)
    } else {
        before.map(|b| b.muted)
    }
}

/// Windows Core Audio: the default render endpoint's `IAudioEndpointVolume`, acquired for each call.
#[cfg(windows)]
mod win {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{
        IMMDeviceEnumerator, MMDeviceEnumerator, eMultimedia, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };

    use super::AudioSnapshot;

    fn endpoint() -> Option<IAudioEndpointVolume> {
        // SAFETY: COM is initialized for this thread (multithreaded, as .NET's thread pool) before the calls; the
        // interfaces are released when dropped
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eMultimedia)
                .ok()?;
            device
                .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
                .ok()
        }
    }

    pub fn snapshot() -> Option<AudioSnapshot> {
        let ep = endpoint()?;
        // SAFETY: calls on a live endpoint
        unsafe {
            let v = ep.GetMasterVolumeLevelScalar().ok()?;
            let m = ep.GetMute().ok()?.as_bool();
            Some(AudioSnapshot::new(
                (f64::from(v) * 100.0).round_ties_even() as i32,
                m,
                true,
            ))
        }
    }

    /// As C#: the results of the writes are not checked once the endpoint was reached.
    pub fn set_volume(level: i32) -> bool {
        let Some(ep) = endpoint() else { return false };
        // SAFETY: calls on a live endpoint; a null event context
        unsafe {
            let _ =
                ep.SetMasterVolumeLevelScalar(level.clamp(0, 100) as f32 / 100.0, std::ptr::null());
            if level > 0 {
                let _ = ep.SetMute(false, std::ptr::null());
            }
        }
        true
    }

    pub fn toggle_mute() -> bool {
        let Some(ep) = endpoint() else { return false };
        // SAFETY: calls on a live endpoint; a null event context
        unsafe {
            let muted = ep.GetMute().map(|m| m.as_bool()).unwrap_or(false);
            let _ = ep.SetMute(!muted, std::ptr::null());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    fn fixed(volume: i32, muted: bool) -> ReadFn {
        Arc::new(move || Some(AudioSnapshot::new(volume, muted, true)))
    }
    fn echo() -> Option<SetFn> {
        Some(Arc::new(Some))
    }
    fn wait_until(what: &str, check: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !check() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    /// A gate a fake reader waits on (at most 10 s, as the C# tests do).
    #[derive(Clone, Default)]
    struct Gate(Arc<(Mutex<bool>, std::sync::Condvar)>);
    impl Gate {
        fn open(&self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
        fn wait(&self) -> bool {
            let guard = self.0.0.lock().unwrap();
            let (guard, _) = self
                .0
                .1
                .wait_timeout_while(guard, Duration::from_secs(10), |open| !*open)
                .unwrap();
            *guard
        }
    }

    #[test]
    fn a_successful_set_shows_the_new_volume_at_once() {
        let audio = AudioControl::with(fixed(50, false), echo(), None);
        assert_eq!(audio.current().map(|s| s.volume), Some(50));
        assert!(audio.set_volume(80));
        let shown = audio.snapshot().unwrap();
        assert_eq!(
            (shown.volume, shown.muted, shown.can_control),
            (80, false, true)
        );
    }

    #[test]
    fn a_slow_reader_does_not_hold_up_a_set_when_an_older_reading_exists() {
        let gate = Gate::default();
        let reads = Arc::new(AtomicUsize::new(0));
        let (g, r) = (gate.clone(), reads.clone());
        let audio = AudioControl::with(
            Arc::new(move || {
                if r.fetch_add(1, Ordering::SeqCst) >= 1 {
                    g.wait(); // the busy computer
                }
                Some(AudioSnapshot::new(50, false, true))
            }),
            echo(),
            None,
        );
        audio.current();
        std::thread::sleep(Duration::from_millis(1000)); // older than the cache allows (900 ms)
        let clock = Instant::now();
        assert!(audio.set_volume(70));
        assert!(
            clock.elapsed() < Duration::from_millis(1000),
            "the set waited {:?} for a read",
            clock.elapsed()
        );
        gate.open();
    }

    #[test]
    fn a_failed_set_keeps_the_reading_and_reports_failure() {
        let audio = AudioControl::with(fixed(50, false), Some(Arc::new(|_| None)), None);
        audio.current();
        assert!(!audio.set_volume(90));
        assert_eq!(audio.snapshot().map(|s| s.volume), Some(50));
    }

    #[test]
    fn two_sets_at_once_are_written_and_remembered_in_order() {
        let first_inside = Gate::default();
        let release_first = Gate::default();
        let running = Arc::new(Mutex::new((0usize, 0usize))); // (running, max)
        let calls = Arc::new(AtomicUsize::new(0));
        let (fi, rf, run, c) = (
            first_inside.clone(),
            release_first.clone(),
            running.clone(),
            calls.clone(),
        );
        let audio = AudioControl::with(
            fixed(50, false),
            Some(Arc::new(move |level| {
                {
                    let mut r = run.lock().unwrap();
                    r.0 += 1;
                    r.1 = r.1.max(r.0);
                }
                if c.fetch_add(1, Ordering::SeqCst) == 0 {
                    fi.open();
                    rf.wait();
                }
                run.lock().unwrap().0 -= 1;
                Some(level)
            })),
            None,
        );
        audio.current();
        let a1 = audio.clone();
        let first = std::thread::spawn(move || a1.set_volume(20));
        assert!(first_inside.wait());
        let a2 = audio.clone();
        let second = std::thread::spawn(move || a2.set_volume(80)); // arrives while the first write is still going
        std::thread::sleep(Duration::from_millis(200));
        release_first.open();
        assert!(first.join().unwrap() && second.join().unwrap());
        assert_eq!(running.lock().unwrap().1, 1);
        assert_eq!(audio.snapshot().map(|s| s.volume), Some(80));
    }

    #[test]
    fn a_set_that_does_not_clear_mute_keeps_the_mute_state() {
        for (clears, was_muted, expected) in [
            (true, true, false),
            (false, true, true),
            (false, false, false),
        ] {
            let before = AudioSnapshot::new(50, was_muted, true);
            assert_eq!(
                muted_after_set(clears, Some(&before)),
                Some(expected),
                "{clears} {was_muted}"
            );
        }
    }

    #[test]
    fn with_no_earlier_reading_a_volume_only_write_leaves_the_mute_state_unknown() {
        assert_eq!(muted_after_set(false, None), None);
        assert_eq!(muted_after_set(true, None), Some(false));
    }

    #[test]
    fn a_toggle_flips_the_remembered_mute_at_once() {
        let toggles = Arc::new(AtomicUsize::new(0));
        let t = toggles.clone();
        let audio = AudioControl::with(
            fixed(50, false),
            echo(),
            Some(Arc::new(move || {
                t.fetch_add(1, Ordering::SeqCst);
                true
            })),
        );
        audio.current();
        assert!(audio.toggle_mute());
        assert_eq!(toggles.load(Ordering::SeqCst), 1);
        let shown = audio.snapshot().unwrap();
        assert!(shown.muted);
        assert_eq!(shown.volume, 50);
        assert!(audio.toggle_mute());
        assert!(!audio.snapshot().unwrap().muted);
    }

    #[test]
    fn a_failed_toggle_keeps_nothing_stale() {
        let audio = AudioControl::with(fixed(50, false), echo(), Some(Arc::new(|| false)));
        audio.current();
        assert!(!audio.toggle_mute());
        assert!(!audio.snapshot().unwrap().muted);
    }

    #[test]
    fn a_write_that_could_not_reach_the_level_is_remembered_at_the_level_it_reached() {
        let audio = AudioControl::with(
            fixed(50, false),
            Some(Arc::new(|level| Some(level.min(68)))),
            None,
        );
        audio.current();
        assert!(audio.set_volume(90));
        assert_eq!(audio.snapshot().map(|s| s.volume), Some(68));
        assert!(audio.set_volume(40));
        assert_eq!(audio.snapshot().map(|s| s.volume), Some(40));
    }

    #[test]
    fn a_read_that_lands_during_a_toggle_is_not_flipped_again() {
        let reading = Arc::new(Mutex::new(AudioSnapshot::new(50, false, true)));
        let me: Arc<OnceLock<Arc<AudioControl>>> = Arc::new(OnceLock::new());
        let (r1, r2, m) = (reading.clone(), reading.clone(), me.clone());
        let audio = AudioControl::with(
            Arc::new(move || Some(r1.lock().unwrap().clone())),
            echo(),
            Some(Arc::new(move || {
                std::thread::sleep(Duration::from_millis(1000)); // the earlier reading is now too old (900 ms)
                *r2.lock().unwrap() = AudioSnapshot::new(50, true, true); // the device has toggled
                m.get().unwrap().current(); // a read lands while the toggle is still in progress
                true
            })),
        );
        let _ = me.set(audio.clone());
        audio.current(); // unmuted
        assert!(audio.toggle_mute());
        assert!(audio.snapshot().unwrap().muted);
    }

    #[test]
    fn a_failed_action_makes_the_next_one_start_from_a_fresh_read() {
        let muted = Arc::new(Mutex::new(false));
        let (m1, m2) = (muted.clone(), muted.clone());
        let audio = AudioControl::with(
            Arc::new(move || Some(AudioSnapshot::new(50, *m1.lock().unwrap(), true))),
            Some(Arc::new(|_| None)),
            Some(Arc::new(move || {
                let mut m = m2.lock().unwrap();
                *m = !*m;
                true
            })),
        );
        assert!(!audio.current().unwrap().muted);
        assert!(!audio.set_volume(60)); // fails: the reading is invalidated
        *muted.lock().unwrap() = true; // muted from outside
        assert!(audio.toggle_mute()); // the sink is now unmuted
        assert!(!audio.current().unwrap().muted);
    }

    #[test]
    fn an_invalidated_reading_is_unknown_not_a_baseline() {
        let audio = AudioControl::with(
            fixed(50, false),
            Some(Arc::new(
                |level| if level == 60 { None } else { Some(level) },
            )),
            None,
        );
        assert_eq!(audio.known_reading(), None);
        audio.current();
        assert!(audio.known_reading().is_some());
        assert!(!audio.set_volume(60));
        assert_eq!(audio.known_reading(), None);
        assert!(audio.set_volume(40));
        assert_eq!(audio.known_reading().map(|s| s.volume), Some(40));
    }

    #[test]
    fn snapshot_returns_at_once_and_fills_in_the_background() {
        let gate = Gate::default();
        let g = gate.clone();
        let audio = AudioControl::with(
            Arc::new(move || {
                g.wait();
                Some(AudioSnapshot::new(42, false, true))
            }),
            None,
            None,
        );
        assert_eq!(audio.snapshot(), None); // nothing read yet, and the call did not wait
        gate.open();
        wait_until("the background read", || !audio.refreshing());
        assert_eq!(audio.snapshot().map(|s| s.volume), Some(42));
    }

    #[test]
    fn an_older_background_read_cannot_replace_a_newer_current_reading() {
        let started = Gate::default();
        let release = Gate::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let (s, r, c) = (started.clone(), release.clone(), calls.clone());
        let audio = AudioControl::with(
            Arc::new(move || {
                if c.fetch_add(1, Ordering::SeqCst) == 0 {
                    s.open(); // the background read starts first and is slow: it saw the old volume
                    r.wait();
                    return Some(AudioSnapshot::new(20, false, true));
                }
                Some(AudioSnapshot::new(60, false, true))
            }),
            None,
            None,
        );
        audio.snapshot();
        assert!(started.wait());
        assert_eq!(audio.current().map(|s| s.volume), Some(60));
        release.open();
        wait_until("the background read", || !audio.refreshing());
        assert_eq!(audio.snapshot().map(|s| s.volume), Some(60));
        assert_eq!(audio.current().map(|s| s.volume), Some(60));
    }

    // Review Focus 2: no wpctl, no pactl (the server, a CI runner): nothing to read or set, quickly, never a panic
    #[cfg(target_os = "linux")]
    #[test]
    fn a_machine_without_audio_tools_answers_without_hanging() {
        if crate::unix::sh::which("wpctl").is_some() || crate::unix::sh::which("pactl").is_some() {
            return; // a desktop: this test is for machines without the tools
        }
        let audio = AudioControl::new();
        let clock = Instant::now();
        assert_eq!(audio.current(), None);
        assert!(!audio.set_volume(50));
        assert!(!audio.step(5));
        assert!(!audio.toggle_mute());
        assert_eq!(audio.snapshot(), None);
        assert!(
            clock.elapsed() < Duration::from_secs(5),
            "{:?}",
            clock.elapsed()
        );
    }
}
