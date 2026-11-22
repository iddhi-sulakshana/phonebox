//! `SpectrumAnalyzer`: the live audio spectrum of what the computer plays, for the "Spectrum bars" widget. Windows only
//! (WASAPI loopback of the default output device). Capture runs only while a panel shows the widget: every `want`
//! keeps it going for a few seconds, and only when an enabled page has the widget. The audio never leaves this module;
//! the phone gets 48 band levels per frame. Elsewhere it stays inactive.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::store::ConfigStore;

pub const WIDGET_TYPE: &str = "spectrum";
/// Bands per frame; the widget merges them into its own number of bars.
pub const BANDS: usize = 48;
/// Frames per second sent on the live stream.
pub const FPS: i64 = 30;
/// Samples per analysis (a power of two): about 43 ms at 48 kHz, 23 Hz per bin.
pub const FFT_SIZE: usize = 2048;
/// Most boost [`loopback_gain`] gives, so a nearly silent slider does not turn noise into bars.
pub const MAX_BOOST_DB: f64 = 40.0;
const KEEP_MS: i64 = 5000;
const MIN_HZ: f64 = 40.0;
const MAX_HZ: f64 = 16000.0;
/// The shown range after the slope below.
const FLOOR_DB: f64 = -66.0;
const TOP_DB: f64 = -6.0;
/// Music falls off towards the treble; lift it so the bars look even.
const SLOPE_DB_PER_OCTAVE: f64 = 3.0;

pub struct SpectrumAnalyzer {
    cfg: Arc<ConfigStore>,
    wanted_until: AtomicI64,
    frame: Mutex<Option<Vec<i32>>>,
    /// The capture thread runs.
    running: std::sync::atomic::AtomicBool,
    /// The last start failed (warned once).
    failing: std::sync::atomic::AtomicBool,
    start_lock: Mutex<()>,
    /// Tests: capture counts as running, with this frame.
    #[cfg(test)]
    pretend: std::sync::atomic::AtomicBool,
}

/// Monotonic milliseconds (`Environment.TickCount64`).
fn now_ms() -> i64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as i64
}

const CATEGORY: &str = "PhoneBox.Agent.SpectrumAnalyzer";

impl SpectrumAnalyzer {
    pub fn new(cfg: Arc<ConfigStore>) -> Arc<SpectrumAnalyzer> {
        Arc::new(SpectrumAnalyzer {
            cfg,
            wanted_until: AtomicI64::new(0),
            frame: Mutex::new(None),
            running: Default::default(),
            failing: Default::default(),
            start_lock: Mutex::new(()),
            #[cfg(test)]
            pretend: Default::default(),
        })
    }

    fn running(&self) -> bool {
        #[cfg(test)]
        if self.pretend.load(Ordering::SeqCst) {
            return true;
        }
        self.running.load(Ordering::SeqCst)
    }

    /// True while a panel wants frames and capture is running.
    pub fn active(&self) -> bool {
        now_ms() < self.wanted_until.load(Ordering::SeqCst) && self.running()
    }

    /// The latest bands, 0–100 each, or None before the first one.
    pub fn frame(&self) -> Option<Vec<i32>> {
        self.frame.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[cfg(any(windows, test))]
    fn set_frame(&self, frame: Option<Vec<i32>>) {
        *self.frame.lock().unwrap_or_else(|e| e.into_inner()) = frame;
    }

    pub fn shown(&self) -> bool {
        self.cfg
            .get()
            .widgets(true)
            .iter()
            .any(|w| w.r#type == WIDGET_TYPE)
    }

    /// Tests: act as if capture ran and produced `frame`.
    #[cfg(test)]
    pub(crate) fn pretend_running(&self, frame: Vec<i32>) {
        self.pretend.store(true, Ordering::SeqCst);
        self.set_frame(Some(frame));
    }

    /// A panel shows a visualizer: start capture (if needed) and keep it going for a few seconds. Refused (false) when
    /// no enabled page has the widget, so removing or hiding the widget turns capture off.
    pub fn want(self: &Arc<Self>) -> bool {
        if !self.shown() {
            return false;
        }
        self.wanted_until
            .store(now_ms() + KEEP_MS, Ordering::SeqCst);
        self.start();
        true
    }

    #[cfg(not(windows))]
    fn start(self: &Arc<Self>) {
        let _ = (&self.start_lock, &self.failing);
        crate::log::debug(
            CATEGORY,
            "spectrum: capturing the output device needs Windows",
        );
    }

    #[cfg(windows)]
    fn start(self: &Arc<Self>) {
        let _gate = self.start_lock.lock().unwrap_or_else(|e| e.into_inner());
        if self.running.load(Ordering::SeqCst) {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("spectrum".into())
            .spawn(move || win::run(me, tx));
        let opened = match spawned {
            Ok(_) => rx
                .recv()
                .unwrap_or_else(|_| Err("the capture thread ended".into())),
            Err(e) => Err(e.to_string()),
        };
        match opened {
            Ok(started) => {
                self.running.store(true, Ordering::SeqCst);
                self.failing.store(false, Ordering::SeqCst);
                crate::log::info(CATEGORY, &started);
            }
            Err(e) => {
                // a panel asks every second while the widget shows, and with no output device every ask fails the same
                // way: warn once, then quietly until capture works
                let message = format!("spectrum: cannot capture the output device: {e}");
                if self.failing.swap(true, Ordering::SeqCst) {
                    crate::log::debug(CATEGORY, &message);
                } else {
                    crate::log::warn(CATEGORY, &message);
                }
            }
        }
    }
}

/// The Hann window over [`FFT_SIZE`] samples.
fn window() -> &'static [f64] {
    static W: std::sync::OnceLock<Vec<f64>> = std::sync::OnceLock::new();
    W.get_or_init(|| {
        let n = FFT_SIZE;
        (0..n)
            .map(|i| 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos()))
            .collect()
    })
}

/// In-place radix-2 FFT; the length must be a power of two.
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        for i in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f64, 0.0f64);
            for k in 0..len / 2 {
                let (a, b) = (i + k, i + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
    }
}

/// The band levels (0–100) of the last [`FFT_SIZE`] mono samples: Hann window, FFT, the loudest bin in each of
/// [`BANDS`] log-spaced bands from 40 Hz to 16 kHz, in dB with a treble lift, mapped to 0–100.
pub fn analyze(samples: &[f32], sample_rate: u32) -> Result<Vec<i32>, String> {
    if samples.len() != FFT_SIZE {
        return Err(format!("needs {FFT_SIZE} samples"));
    }
    let w = window();
    let mut re: Vec<f64> = samples
        .iter()
        .zip(w)
        .map(|(s, w)| {
            if s.is_finite() {
                f64::from(*s) * w
            } else {
                0.0
            }
        })
        .collect();
    let mut im = vec![0.0; FFT_SIZE];
    fft(&mut re, &mut im);
    let bin_hz = f64::from(sample_rate) / FFT_SIZE as f64;
    let top = MAX_HZ.min(f64::from(sample_rate) / 2.0 * 0.95);
    let mut bands = vec![0; BANDS];
    for (b, band) in bands.iter_mut().enumerate() {
        let lo = MIN_HZ * (top / MIN_HZ).powf(b as f64 / BANDS as f64);
        let hi = MIN_HZ * (top / MIN_HZ).powf((b + 1) as f64 / BANDS as f64);
        let i0 = ((lo / bin_hz).floor() as i64).max(1);
        let i1 = i0.max(((FFT_SIZE / 2 - 1) as i64).min((hi / bin_hz).ceil() as i64 - 1));
        let peak = (i0..=i1)
            .map(|i| {
                let i = i as usize;
                (re[i] * re[i] + im[i] * im[i]).sqrt()
            })
            .fold(0.0f64, f64::max);
        // amplitude of a sine in this band: |X| * 2 / sum(window), and sum(Hann) = N / 2
        let amp = peak * 4.0 / FFT_SIZE as f64;
        let db =
            20.0 * (amp + 1e-9).log10() + SLOPE_DB_PER_OCTAVE * ((lo * hi).sqrt() / 1000.0).log2();
        *band = (((db - FLOOR_DB) / (TOP_DB - FLOOR_DB)).clamp(0.0, 1.0) * 100.0).round_ties_even()
            as i32;
    }
    Ok(bands)
}

/// The gain that brings loopback back to the level of what is playing: loopback captures after the Windows volume on
/// outputs that apply it in software (HDMI and DisplayPort monitors, S/PDIF…); outputs with a hardware volume capture at
/// full level already.
pub fn loopback_gain(hardware_volume: bool, volume_db: f32) -> f32 {
    if hardware_volume || !volume_db.is_finite() {
        1.0
    } else {
        10f64.powf((-f64::from(volume_db)).clamp(0.0, MAX_BOOST_DB) / 20.0) as f32
    }
}

/// Writes interleaved frames (32-bit float, or 16/32-bit PCM) to `ring` as mono from `pos`, times `gain`, and returns
/// the next position. The gain belongs to the samples as they are captured.
#[allow(clippy::too_many_arguments)]
pub fn downmix(
    buf: &[u8],
    channels: usize,
    bits: u32,
    is_float: bool,
    frame_bytes: usize,
    gain: f32,
    ring: &mut [f32],
    mut pos: usize,
) -> usize {
    let ch = channels.max(1);
    let sample_bytes = (bits / 8) as usize;
    if frame_bytes == 0 || ring.is_empty() || ch * sample_bytes > frame_bytes {
        return pos;
    }
    // stereo sound on a surround output fills only the front pair: dividing by every channel would lower it 6–12 dB,
    // so the sum is scaled like stereo
    let scale = gain / ch.min(2) as f32;
    for frame in buf.chunks_exact(frame_bytes) {
        let mut sum = 0.0f64;
        for c in 0..ch {
            let s = &frame[c * sample_bytes..];
            sum += if is_float {
                f64::from(f32::from_le_bytes([s[0], s[1], s[2], s[3]]))
            } else if bits == 32 {
                f64::from(i32::from_le_bytes([s[0], s[1], s[2], s[3]])) / 2147483648.0
            } else {
                f64::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0
            };
        }
        ring[pos] = (sum * f64::from(scale)) as f32;
        pos = (pos + 1) % ring.len();
    }
    pos
}

/// Windows: WASAPI loopback of the default output device, on its own thread, which also ticks the frames (33 ms) and
/// ends capture when no panel asked for a while, the widget left the enabled pages, or the default output changed.
#[cfg(windows)]
mod win {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
        IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
        WAVEFORMATEX, eMultimedia, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    };

    use super::{
        BANDS, CATEGORY, FFT_SIZE, FPS, SpectrumAnalyzer, analyze, downmix, loopback_gain, now_ms,
    };

    const ENDPOINT_HARDWARE_SUPPORT_VOLUME: u32 = 1;
    const WAVE_FORMAT_PCM: u16 = 1;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

    struct Opened {
        client: IAudioClient,
        capture: IAudioCaptureClient,
        volume: IAudioEndpointVolume,
        device_id: String,
        hardware_volume: bool,
        rate: u32,
        channels: usize,
        bits: u32,
        block_align: usize,
        /// 32-bit float or 16/32-bit PCM; other formats are not read.
        readable: bool,
        is_float: bool,
    }

    fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
        // SAFETY: COM is initialized on this thread (run)
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
    }

    fn id_of(device: &IMMDevice) -> windows::core::Result<String> {
        // SAFETY: the id string is freed after it is copied
        unsafe {
            let id = device.GetId()?;
            let text = id.to_string().unwrap_or_default();
            CoTaskMemFree(Some(id.0 as *const _));
            Ok(text)
        }
    }

    /// The id of the default output device (the one loopback captures), or None when there is none.
    fn default_output_id() -> Option<String> {
        // SAFETY: plain COM calls
        let device = unsafe {
            enumerator()
                .ok()?
                .GetDefaultAudioEndpoint(eRender, eMultimedia)
                .ok()?
        };
        id_of(&device).ok()
    }

    fn open() -> windows::core::Result<Opened> {
        // SAFETY: COM calls on live interfaces; the mix format is read unaligned (WAVEFORMATEX is packed) and freed
        // after Initialize copied it
        unsafe {
            let device = enumerator()?.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
            let device_id = id_of(&device)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let fmt_ptr = client.GetMixFormat()?;
            let fmt: WAVEFORMATEX = std::ptr::read_unaligned(fmt_ptr);
            let tag = fmt.wFormatTag;
            let sub = if tag == WAVE_FORMAT_EXTENSIBLE && fmt.cbSize >= 22 {
                // WAVEFORMATEXTENSIBLE: the SubFormat GUID follows at byte 24; its first field is the format tag
                std::ptr::read_unaligned((fmt_ptr as *const u8).add(24) as *const u32) as u16
            } else {
                tag
            };
            let initialized = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                1_000_000, // 100 ms, as NAudio's capture
                0,
                fmt_ptr,
                None,
            );
            CoTaskMemFree(Some(fmt_ptr as *const _));
            initialized?;
            let capture: IAudioCaptureClient = client.GetService()?;
            let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None)?;
            let hardware_volume = volume
                .QueryHardwareSupport()
                .map(|s| s & ENDPOINT_HARDWARE_SUPPORT_VOLUME != 0)
                .unwrap_or(false);
            let bits = u32::from(fmt.wBitsPerSample);
            let is_float = sub == WAVE_FORMAT_IEEE_FLOAT;
            let readable = fmt.nBlockAlign > 0
                && if is_float {
                    bits == 32
                } else {
                    sub == WAVE_FORMAT_PCM && (bits == 16 || bits == 32)
                };
            client.Start()?;
            Ok(Opened {
                client,
                capture,
                volume,
                device_id,
                hardware_volume,
                rate: fmt.nSamplesPerSec,
                channels: usize::from(fmt.nChannels),
                bits,
                block_align: usize::from(fmt.nBlockAlign),
                readable,
                is_float,
            })
        }
    }

    /// "-19.3", "-20": the C# `{0:0.#}` form.
    fn one_decimal(v: f32) -> String {
        let r = (f64::from(v) * 10.0).round() / 10.0;
        if r.fract() == 0.0 {
            format!("{r:.0}")
        } else {
            format!("{r:.1}")
        }
    }

    enum End {
        NotWanted,
        DeviceChanged,
        Failed(String),
    }

    pub(super) fn run(
        me: Arc<SpectrumAnalyzer>,
        opened_tx: std::sync::mpsc::Sender<Result<String, String>>,
    ) {
        // SAFETY: initializes COM for this thread, which owns every interface below
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let cap = match open() {
            Ok(cap) => cap,
            Err(e) => {
                let _ = opened_tx.send(Err(e.message()));
                return;
            }
        };
        // SAFETY: a call on a live endpoint
        let master_db = unsafe { cap.volume.GetMasterVolumeLevel() }.unwrap_or(0.0);
        let mut gain = loopback_gain(cap.hardware_volume, master_db);
        let _ = opened_tx.send(Ok(format!(
            "spectrum: capture started ({} Hz, {}-bit, {} ch, {} volume)",
            cap.rate,
            cap.bits,
            cap.channels,
            if cap.hardware_volume {
                "hardware".to_string()
            } else {
                format!("software {} dB", one_decimal(master_db))
            }
        )));

        let mut ring = vec![0f32; FFT_SIZE * 2];
        let mut last = vec![0f32; FFT_SIZE];
        let mut ring_pos = 0usize;
        let mut last_audio = 0i64;
        let mut tick_count = 0u64;
        let tick_every = Duration::from_millis((1000 / FPS) as u64);
        let mut next_tick = Instant::now();
        let mut next_volume = Instant::now();
        let end = loop {
            // drain what the device delivered
            let drained: windows::core::Result<()> = (|| {
                // SAFETY: each buffer is read within its GetBuffer / ReleaseBuffer pair, frames * block_align bytes
                unsafe {
                    while cap.capture.GetNextPacketSize()? > 0 {
                        let mut data = std::ptr::null_mut();
                        let (mut frames, mut flags) = (0u32, 0u32);
                        cap.capture
                            .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                        let bytes = frames as usize * cap.block_align;
                        if cap.readable {
                            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                            let zeros;
                            let buf: &[u8] = if silent || data.is_null() {
                                zeros = vec![0u8; bytes];
                                &zeros
                            } else {
                                std::slice::from_raw_parts(data, bytes)
                            };
                            ring_pos = downmix(
                                buf,
                                cap.channels,
                                cap.bits,
                                cap.is_float,
                                cap.block_align,
                                gain,
                                &mut ring,
                                ring_pos,
                            );
                            if bytes > 0 {
                                last_audio = now_ms();
                            }
                        }
                        cap.capture.ReleaseBuffer(frames)?;
                    }
                }
                Ok(())
            })();
            if let Err(e) = drained {
                break End::Failed(e.message());
            }
            // the volume of a software-volume output, for the samples that arrive from now on
            if !cap.hardware_volume && Instant::now() >= next_volume {
                // SAFETY: a call on a live endpoint
                if let Ok(db) = unsafe { cap.volume.GetMasterVolumeLevel() } {
                    gain = loopback_gain(false, db);
                }
                next_volume = Instant::now() + Duration::from_millis(100);
            }
            if Instant::now() >= next_tick {
                next_tick += tick_every;
                if next_tick < Instant::now() {
                    next_tick = Instant::now() + tick_every; // a late tick is skipped, not caught up
                }
                tick_count += 1;
                // stop when no panel asked for a while, or the widget was removed or its page turned off since (the
                // config walk once a second is enough for that)
                let every_second = tick_count.is_multiple_of(FPS as u64);
                if now_ms() >= me.wanted_until.load(Ordering::SeqCst)
                    || (every_second && !me.shown())
                {
                    break End::NotWanted;
                }
                // loopback stays on the device it was opened on: follow the user to a new default output
                if every_second && default_output_id().is_some_and(|id| id != cap.device_id) {
                    break End::DeviceChanged;
                }
                // WASAPI loopback delivers nothing while the computer is silent: show silence, not the last sound
                if now_ms() - last_audio > 250 {
                    me.set_frame(Some(vec![0; BANDS]));
                } else {
                    let start = (ring_pos + ring.len() - FFT_SIZE) % ring.len();
                    for (i, s) in last.iter_mut().enumerate() {
                        *s = ring[(start + i) % ring.len()];
                    }
                    match analyze(&last, cap.rate) {
                        Ok(bands) => me.set_frame(Some(bands)),
                        Err(e) => crate::log::debug(CATEGORY, &format!("spectrum: {e}")),
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        // SAFETY: a call on a live client
        let _ = unsafe { cap.client.Stop() };
        drop(cap);
        me.set_frame(None);
        me.running.store(false, Ordering::SeqCst);
        match end {
            End::NotWanted => crate::log::info(
                CATEGORY,
                "spectrum: capture stopped (no visualizer on screen)",
            ),
            End::DeviceChanged => {
                crate::log::info(
                    CATEGORY,
                    "spectrum: capture stopped (default output changed)",
                );
                me.start();
            }
            End::Failed(e) => {
                crate::log::info(CATEGORY, &format!("spectrum: capture stopped: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f64, amplitude: f64, rate: u32) -> Vec<f32> {
        (0..FFT_SIZE)
            .map(|i| {
                (amplitude * (2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64).sin())
                    as f32
            })
            .collect()
    }

    fn max(v: &[i32]) -> i32 {
        *v.iter().max().unwrap()
    }

    fn loudest(v: &[i32]) -> usize {
        let m = max(v);
        v.iter().position(|b| *b == m).unwrap()
    }

    #[test]
    fn silence_is_all_zero() {
        assert!(
            analyze(&vec![0.0; FFT_SIZE], 48000)
                .unwrap()
                .iter()
                .all(|b| *b == 0)
        );
    }

    #[test]
    fn a_corrupt_sample_does_not_blank_the_rest_of_the_window() {
        for corrupt in [0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut clean = sine(1000.0, 0.5, 48000);
            clean[512] = 0.0;
            let expected = analyze(&clean, 48000).unwrap();
            assert!(max(&expected) > 0);
            clean[512] = corrupt;
            assert_eq!(analyze(&clean, 48000).unwrap(), expected, "{corrupt}");
        }
    }

    #[test]
    fn bass_lands_in_the_bass_bars() {
        let bands = analyze(&sine(60.0, 0.5, 48000), 48000).unwrap();
        assert!(loudest(&bands) <= 8);
        assert!(bands.iter().skip(16).all(|b| *b < max(&bands) / 2));
    }

    #[test]
    fn a_tone_lights_its_own_band() {
        for (hz, rate) in [(250.0, 48000), (1000.0, 48000), (8000.0, 44100)] {
            let bands = analyze(&sine(hz, 0.5, rate), rate).unwrap();
            assert_eq!(bands.len(), BANDS);
            assert!(bands.iter().all(|b| (0..=100).contains(b)));
            let expected =
                ((hz / 40.0).ln() / (16000.0f64 / 40.0).ln() * BANDS as f64).floor() as i64;
            let top = loudest(&bands);
            assert!(
                (expected - 1..=expected + 1).contains(&(top as i64)),
                "{hz}: band {top}"
            );
            assert!(bands[top] >= 60, "band {top} = {}", bands[top]);
            // far away from the tone, the window keeps the leakage low
            assert!(
                bands
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| i.abs_diff(top) > 6)
                    .all(|(_, b)| *b < bands[top] / 2)
            );
        }
    }

    #[test]
    fn louder_is_taller() {
        let quiet = max(&analyze(&sine(1000.0, 0.02, 48000), 48000).unwrap());
        let loud = max(&analyze(&sine(1000.0, 0.5, 48000), 48000).unwrap());
        assert!(loud > quiet, "{loud} <= {quiet}");
    }

    #[test]
    fn loopback_gain_undoes_software_volume() {
        for (hardware, db, gain) in [
            (true, -19.3f32, 1f32), // hardware volume: loopback is at full level already
            (false, 0.0, 1.0),      // software volume at 100 %
            (false, -20.0, 10.0),   // software volume at -20 dB: undo it
            (false, -65.0, 100.0),  // nearly silent slider: at most +40 dB
            (false, 3.0, 1.0),      // never turned down
            (false, f32::NAN, 1.0),
        ] {
            let got = loopback_gain(hardware, db);
            assert!(
                (got - gain).abs() < 0.0005 * gain.max(1.0),
                "{hardware} {db}: {got}"
            );
        }
    }

    #[test]
    fn software_volume_no_longer_flattens_the_bars() {
        // a monitor at 28 % (-19.3 dB) hands over music 19 dB quieter: with the gain the bars match full volume
        let full = max(&analyze(&sine(1000.0, 0.05, 48000), 48000).unwrap());
        let turned_down = sine(1000.0, 0.05 * 10f64.powf(-19.3 / 20.0), 48000);
        let gain = loopback_gain(false, -19.3);
        let restored: Vec<f32> = turned_down.iter().map(|s| s * gain).collect();
        assert!(max(&analyze(&turned_down, 48000).unwrap()) < full - 20);
        let restored = max(&analyze(&restored, 48000).unwrap());
        assert!(
            (full - 1..=full + 1).contains(&restored),
            "{restored} vs {full}"
        );
    }

    fn float_frames(frames: usize, frame: &[f32]) -> Vec<u8> {
        (0..frames)
            .flat_map(|_| frame.iter().flat_map(|f| f.to_le_bytes()))
            .collect()
    }

    #[test]
    fn a_volume_change_only_scales_audio_captured_after_it() {
        let mut ring = [0f32; 8];
        let pos = downmix(
            &float_frames(4, &[0.001, 0.001]),
            2,
            32,
            true,
            8,
            loopback_gain(false, -40.0),
            &mut ring,
            0,
        );
        let pos = downmix(
            &float_frames(4, &[0.1, 0.1]),
            2,
            32,
            true,
            8,
            loopback_gain(false, 0.0),
            &mut ring,
            pos,
        );
        assert_eq!(pos, 0);
        assert!(ring.iter().all(|s| (s - 0.1).abs() < 1e-4), "{ring:?}");
    }

    #[test]
    fn stereo_on_a_surround_output_keeps_its_level() {
        for channels in [2usize, 4, 8] {
            let mut frame = vec![0f32; channels];
            frame[0] = 0.5;
            frame[1] = 0.5;
            let mut ring = [0f32; 4];
            downmix(
                &float_frames(4, &frame),
                channels,
                32,
                true,
                channels * 4,
                1.0,
                &mut ring,
                0,
            );
            assert!(
                ring.iter().all(|s| (s - 0.5).abs() < 1e-4),
                "{channels}: {ring:?}"
            );
        }
    }

    #[test]
    fn pcm_is_downmixed_too() {
        let mut ring = [0f32; 2];
        let pcm16: Vec<u8> = [16384i16, 16384, -16384, -16384]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        downmix(&pcm16, 2, 16, false, 4, 1.0, &mut ring, 0);
        assert_eq!(ring, [0.5, -0.5]);
        let mut ring = [0f32; 1];
        let pcm32: Vec<u8> = [1i32 << 30].iter().flat_map(|s| s.to_le_bytes()).collect();
        downmix(&pcm32, 1, 32, false, 4, 1.0, &mut ring, 0);
        assert_eq!(ring, [0.5]);
    }

    #[test]
    fn only_full_windows_are_analyzed() {
        assert!(analyze(&[0.0; 100], 48000).is_err());
    }

    fn analyzer(
        dir: &tempfile::TempDir,
        widget: bool,
        page_enabled: bool,
    ) -> Arc<SpectrumAnalyzer> {
        let path = dir.path().join("config.json");
        let widgets = if widget {
            serde_json::json!([{"id": "s", "type": "spectrum", "x": 0, "y": 0, "w": 6, "h": 3}])
        } else {
            serde_json::json!([])
        };
        let json = serde_json::json!({"pages": [
            {"id": "p", "title": "P", "enabled": page_enabled, "widgets": widgets},
            {"id": "q", "title": "Q", "widgets": [{"id": "c", "type": "clock", "x": 0, "y": 0, "w": 6, "h": 4}]}
        ]});
        std::fs::write(&path, json.to_string()).unwrap();
        SpectrumAnalyzer::new(Arc::new(ConfigStore::new(path)))
    }

    #[test]
    fn capture_is_refused_without_the_widget_on_an_enabled_page() {
        let dir = tempfile::tempdir().unwrap();
        let none = analyzer(&dir, false, true);
        assert!(!none.want());
        assert!(!none.active());
        assert!(none.frame().is_none());
        let dir = tempfile::tempdir().unwrap();
        let hidden = analyzer(&dir, true, false);
        assert!(!hidden.want());
        assert!(!hidden.active());
    }

    #[test]
    fn the_widget_on_an_enabled_page_is_shown() {
        let dir = tempfile::tempdir().unwrap();
        assert!(analyzer(&dir, true, true).shown());
    }

    #[cfg(not(windows))]
    #[test]
    fn elsewhere_a_wanted_widget_stays_inactive() {
        let dir = tempfile::tempdir().unwrap();
        let shown = analyzer(&dir, true, true);
        assert!(shown.want());
        assert!(!shown.active());
        assert!(shown.frame().is_none());
    }
}
