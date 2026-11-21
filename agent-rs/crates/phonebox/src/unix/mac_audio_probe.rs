//! Port of `Unix/MacAudioProbe.cs`: the selected macOS output read and written in-process through CoreAudio, with DDC
//! readback (`m1ddc`) for a display that is the selected output. Elsewhere every call answers "nothing".

/// Whether the output ends up unmuted: not muted already, or muted and cleared. A mute that cannot be read is only fine
/// when the output has no mute property at all (`exists` false); otherwise the state is unknown and the caller falls
/// back to osascript.
pub fn unmuted(muted: Option<u32>, exists: Option<bool>, clear: impl FnOnce() -> bool) -> bool {
    match muted {
        Some(m) => m == 0 || clear(),
        None => exists == Some(false),
    }
}

/// The levels to write to channels at `current` so their average becomes `target`: each moves by the same factor (the
/// balance stays); a channel that would pass 1 stops there and the rest goes to the others.
pub fn scale_channels(current: &[f32], target: f32) -> Vec<f32> {
    if current.is_empty() {
        return vec![];
    }
    let average = current.iter().sum::<f32>() / current.len() as f32;
    if average <= 0.0 {
        return vec![target; current.len()]; // silent: no balance to keep
    }
    let factor = target / average;
    let mut levels: Vec<f32> = current
        .iter()
        .map(|c| (c * factor).clamp(0.0, 1.0))
        .collect();
    for _ in 0..levels.len() {
        let missing = target * levels.len() as f32 - levels.iter().sum::<f32>();
        let open = levels.iter().filter(|l| **l < 1.0).count();
        if missing <= 1e-6 || open == 0 {
            break;
        }
        for l in levels.iter_mut().filter(|l| **l < 1.0) {
            *l = (*l + missing / open as f32).min(1.0);
        }
    }
    levels
}

/// The level an output reports for these channel values: their average, in percent.
pub fn effective_level(written: &[f32]) -> i32 {
    if written.is_empty() {
        return 0;
    }
    let average = (written.iter().sum::<f32>() / written.len() as f32).clamp(0.0, 1.0);
    f64::from(average * 100.0).round_ties_even() as i32
}

/// Channel writes in order (None: the channel does not exist): done when at least one exists and all were written.
pub fn all_written(results: &[Option<bool>]) -> bool {
    let existing: Vec<bool> = results.iter().flatten().copied().collect();
    !existing.is_empty() && existing.iter().all(|r| *r)
}

/// The selected output: (device id, name, volume 0-100, muted), each part as far as it can be read.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub id: u32,
    pub name: String,
    pub volume: Option<i32>,
    pub muted: Option<bool>,
}

pub use os::{can_control, display_volume, selected_output, set_volume, toggle_mute};

#[cfg(target_os = "macos")]
mod os {
    use std::ffi::{c_char, c_void};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::{Output, all_written, effective_level, scale_channels, unmuted};
    use crate::unix::sh;

    const GLOBAL: u32 = 0x676c_6f62; // 'glob'
    const OUTPUT: u32 = 0x6f75_7470; // 'outp'
    const DEFAULT_OUTPUT: u32 = 0x644f_7574; // 'dOut'
    const NAME: u32 = 0x6c6e_616d; // 'lnam'
    const VOLUME: u32 = 0x766f_6c6d; // 'volm'
    const MUTE: u32 = 0x6d75_7465; // 'mute'
    const VIRTUAL_MAIN: u32 = 0x766d_7663; // 'vmvc': the volume System Settings and AppleScript's "output volume" show

    #[repr(C)]
    struct Address {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyData(
            id: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> i32;
        fn AudioObjectSetPropertyData(
            id: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: u32,
            data: *const c_void,
        ) -> i32;
        fn AudioObjectIsPropertySettable(
            id: u32,
            address: *const Address,
            settable: *mut u8,
        ) -> i32;
        fn AudioObjectHasProperty(id: u32, address: *const Address) -> u8;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringGetCString(
            string: *const c_void,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(cf: *const c_void);
    }

    fn get<T: Default>(id: u32, selector: u32, scope: u32, element: u32) -> Option<T> {
        let address = Address {
            selector,
            scope,
            element,
        };
        let mut size = size_of::<T>() as u32;
        let mut value = T::default();
        // SAFETY: `value` is a T and `size` says so; CoreAudio writes at most `size` bytes
        let status = unsafe {
            AudioObjectGetPropertyData(
                id,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                (&mut value as *mut T).cast(),
            )
        };
        (status == 0).then_some(value)
    }

    fn uint(id: u32, selector: u32, scope: u32, element: u32) -> Option<u32> {
        get::<u32>(id, selector, scope, element)
    }

    fn float(id: u32, selector: u32, scope: u32, element: u32) -> Option<f32> {
        get::<f32>(id, selector, scope, element).filter(|v| v.is_finite())
    }

    fn write<T>(device: u32, selector: u32, element: u32, value: T) -> bool {
        let address = Address {
            selector,
            scope: OUTPUT,
            element,
        };
        // SAFETY: a T and its size
        unsafe {
            AudioObjectSetPropertyData(
                device,
                &address,
                0,
                std::ptr::null(),
                size_of::<T>() as u32,
                (&value as *const T).cast(),
            ) == 0
        }
    }

    fn exists(device: u32, selector: u32, element: u32) -> bool {
        let address = Address {
            selector,
            scope: OUTPUT,
            element,
        };
        // SAFETY: a plain query
        unsafe { AudioObjectHasProperty(device, &address) != 0 }
    }

    fn default_output() -> Option<u32> {
        uint(1, DEFAULT_OUTPUT, GLOBAL, 0).filter(|d| *d != 0)
    }

    /// Sets the selected output's volume (0-100) and unmutes it, in-process; the level the output ended on (None when
    /// it has no controllable volume: the caller falls back to osascript).
    pub fn set_volume(level: i32) -> Option<i32> {
        let device = default_output()?;
        let level = level.clamp(0, 100);
        let value = level as f32 / 100.0;
        // the virtual main volume first (what the system moves), then the main element, then the channels
        let mut ok = write(device, VIRTUAL_MAIN, 0, value) || write(device, VOLUME, 0, value);
        let mut reached = level;
        if !ok {
            // per-channel volumes: each moves by the same factor; a channel that does not exist is skipped, one that
            // exists and refuses makes the whole set fail
            let current = [
                float(device, VOLUME, OUTPUT, 1),
                float(device, VOLUME, OUTPUT, 2),
            ];
            let present: Vec<f32> = current.iter().flatten().copied().collect();
            let targets = scale_channels(&present, value);
            let mut next = 0;
            let mut channel = |element: u32| -> Option<bool> {
                current[element as usize - 1]?;
                let target = targets[next];
                next += 1;
                Some(write(device, VOLUME, element, target))
            };
            let results = [channel(1), channel(2)];
            ok = all_written(&results);
            reached = effective_level(&targets);
        }
        if ok {
            ok = unmuted(
                uint(device, MUTE, OUTPUT, 0),
                Some(exists(device, MUTE, 0)),
                || write(device, MUTE, 0, 0u32),
            );
        }
        ok.then_some(reached)
    }

    /// Flips the selected output's mute in-process; false when it cannot be read or written.
    pub fn toggle_mute() -> bool {
        let Some(device) = default_output() else {
            return false;
        };
        let Some(muted) = uint(device, MUTE, OUTPUT, 0) else {
            return false;
        };
        write(device, MUTE, 0, u32::from(muted == 0))
    }

    pub fn can_control(device: u32) -> bool {
        let writable = |selector: u32, element: u32| {
            let address = Address {
                selector,
                scope: OUTPUT,
                element,
            };
            let mut settable = 0u8;
            // SAFETY: a plain query with an out byte
            unsafe {
                AudioObjectIsPropertySettable(device, &address, &mut settable) == 0 && settable != 0
            }
        };
        (writable(VOLUME, 0) || writable(VOLUME, 1) || writable(VOLUME, 2)) && writable(MUTE, 0)
    }

    fn device_name(device: u32) -> Option<String> {
        let cf = get::<usize>(device, NAME, GLOBAL, 0).filter(|p| *p != 0)? as *const c_void;
        let mut buffer = [0 as c_char; 512];
        // SAFETY: a CFString the property handed over (released once below) and a 512-byte buffer
        let ok = unsafe {
            CFStringGetCString(cf, buffer.as_mut_ptr(), buffer.len() as isize, 0x0800_0100) != 0
        };
        // SAFETY: the property returns a retained CFString
        unsafe { CFRelease(cf) };
        if !ok {
            return None;
        }
        let bytes: Vec<u8> = buffer
            .iter()
            .take_while(|b| **b != 0)
            .map(|b| *b as u8)
            .collect();
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn selected_output() -> Option<Output> {
        let device = default_output()?;
        let mut scalar =
            float(device, VIRTUAL_MAIN, OUTPUT, 0).or_else(|| float(device, VOLUME, OUTPUT, 0));
        if scalar.is_none() {
            let channels: Vec<f32> = [
                float(device, VOLUME, OUTPUT, 1),
                float(device, VOLUME, OUTPUT, 2),
            ]
            .into_iter()
            .flatten()
            .collect();
            if !channels.is_empty() {
                scalar = Some(channels.iter().sum::<f32>() / channels.len() as f32);
            }
        }
        Some(Output {
            id: device,
            name: device_name(device).unwrap_or_default(),
            volume: scalar.map(|s| f64::from(s.clamp(0.0, 1.0) * 100.0).round_ties_even() as i32),
            muted: uint(device, MUTE, OUTPUT, 0).map(|m| m != 0),
        })
    }

    struct Display {
        device: u32,
        name: String,
        index: Option<i32>,
        at: Option<Instant>,
    }
    static DISPLAY: Mutex<Display> = Mutex::new(Display {
        device: 0,
        name: String::new(),
        index: None,
        at: None,
    });

    /// A display's own volume over DDC (`m1ddc`), when the selected output is that display.
    pub fn display_volume(device: u32, name: &str) -> Option<i32> {
        if name.trim().is_empty() || sh::which("m1ddc").is_none() {
            return None;
        }
        let index = {
            let mut d = DISPLAY.lock().unwrap_or_else(|e| e.into_inner());
            if d.device != device
                || d.name != name
                || d.at.is_none_or(|at| at.elapsed() > Duration::from_secs(30))
            {
                d.device = device;
                d.name = name.to_string();
                d.at = Some(Instant::now());
                d.index = None;
                let re = regex::Regex::new(r"^\[(\d+)\] (.+?) \(").expect("regex");
                if let Some(list) = sh::run_with_timeout(5000, "m1ddc", &["display", "list"]) {
                    d.index = list.split('\n').find_map(|line| {
                        let c = re.captures(line)?;
                        (c[2].trim().to_lowercase() == name.trim().to_lowercase())
                            .then(|| c[1].parse().ok())?
                    });
                }
            }
            d.index
        }?;
        let n = index.to_string();
        let value: i32 = sh::run_with_timeout(3000, "m1ddc", &["display", &n, "get", "volume"])?
            .trim()
            .parse()
            .ok()?;
        let max: i32 = sh::run_with_timeout(3000, "m1ddc", &["display", &n, "max", "volume"])?
            .trim()
            .parse()
            .ok()?;
        (max > 0)
            .then(|| (f64::from(value) / f64::from(max)).clamp(0.0, 1.0) * 100.0)
            .map(|v| v.round_ties_even() as i32)
    }
}

#[cfg(not(target_os = "macos"))]
mod os {
    use super::Output;

    pub fn set_volume(_level: i32) -> Option<i32> {
        None
    }
    pub fn toggle_mute() -> bool {
        false
    }
    pub fn can_control(_device: u32) -> bool {
        false
    }
    pub fn selected_output() -> Option<Output> {
        None
    }
    pub fn display_volume(_device: u32, _name: &str) -> Option<i32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_needs_both_channels_written() {
        assert!(all_written(&[Some(true), Some(true)]));
        assert!(!all_written(&[Some(true), Some(false)]));
        assert!(!all_written(&[Some(false), Some(false)]));
    }

    #[test]
    fn a_mono_output_needs_only_its_one_channel_and_no_channels_is_failure() {
        assert!(all_written(&[Some(true), None]));
        assert!(!all_written(&[None, None]));
    }

    #[test]
    fn a_mute_that_cannot_be_read_is_only_fine_when_the_output_has_no_mute_property() {
        assert!(unmuted(Some(0), Some(true), || panic!("must not write")));
        assert!(unmuted(Some(1), Some(true), || true));
        assert!(!unmuted(Some(1), Some(true), || false));
        assert!(!unmuted(None, Some(true), || true));
        assert!(!unmuted(None, None, || true));
        assert!(unmuted(None, Some(false), || panic!("must not write")));
    }

    #[test]
    fn channels_keep_their_balance_when_the_volume_changes() {
        for (current, target, expected) in [
            (vec![0.2f32, 0.8], 0.25f32, vec![0.1f32, 0.4]),
            (vec![0.2, 0.8], 0.5, vec![0.2, 0.8]),
            (vec![0.2, 0.8], 0.9, vec![0.8, 1.0]),
            (vec![0.5, 1.0], 1.0, vec![1.0, 1.0]),
            (vec![0.9, 0.9], 1.0, vec![1.0, 1.0]),
            (vec![0.0, 0.0], 0.5, vec![0.5, 0.5]),
            (vec![0.6], 0.3, vec![0.3]),
        ] {
            let got = scale_channels(&current, target);
            assert_eq!(got.len(), expected.len(), "{current:?} → {target}");
            for (g, e) in got.iter().zip(&expected) {
                assert!((g - e).abs() < 0.0005, "{current:?} → {target}: {got:?}");
            }
        }
    }

    #[test]
    fn the_effective_level_is_the_average_of_what_was_written() {
        assert_eq!(effective_level(&[0.36, 1.0]), 68);
        assert_eq!(effective_level(&[0.1, 0.4]), 25);
        assert_eq!(effective_level(&[0.3]), 30);
        assert_eq!(effective_level(&[]), 0);
    }
}
