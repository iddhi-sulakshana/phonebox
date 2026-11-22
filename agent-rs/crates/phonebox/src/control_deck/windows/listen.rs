//! Loopback listening (`ControlDeck.Native.cs:394-420`): what the default output plays, captured through WASAPI in its
//! mix format on a thread of its own, packet by packet (silent ones skipped) into a 12-packet queue that drops the
//! oldest. The stream that reads it stops the capture when it is dropped.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use base64::Engine;
use futures_util::stream::BoxStream;
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eMultimedia, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree,
};
use windows::core::GUID;

use super::super::models::DeckAudioPacket;
use super::{ListenFormat, PacketQueue, audio, listen_stream, wave_encoding};

const IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
/// The capture buffer (NAudio's default, 100 ms in 100 ns units).
const BUFFER: i64 = 1_000_000;

/// Stops the capture thread when the stream goes away.
struct Stop(Arc<AtomicBool>);

impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The packets, or None when the default output cannot be captured.
pub(super) fn start() -> Option<BoxStream<'static, DeckAudioPacket>> {
    let queue = Arc::new(PacketQueue::new(12));
    let stop = Arc::new(AtomicBool::new(false));
    let (ready, opened) = std::sync::mpsc::sync_channel(1);
    let (q, s) = (queue.clone(), stop.clone());
    std::thread::Builder::new()
        .name("deck-listen".into())
        .spawn(move || capture(q, s, ready))
        .ok()?;
    let stop = Stop(stop);
    let format = opened.recv_timeout(Duration::from_secs(5)).ok().flatten()?;
    Some(listen_stream(queue, format, stop))
}

/// The mix format as the phone is told it.
///
/// SAFETY: `mix` is a WAVEFORMATEX from GetMixFormat, extended when its tag says so.
unsafe fn format_of(mix: *const WAVEFORMATEX) -> ListenFormat {
    // SAFETY: as the function's contract says
    unsafe {
        let format = *mix;
        // packed structs: the sub-format is copied out before it is compared
        let float = format.wFormatTag == 0xFFFE && {
            let extensible = std::ptr::read_unaligned(mix as *const WAVEFORMATEXTENSIBLE);
            let sub_format = extensible.SubFormat;
            sub_format == IEEE_FLOAT
        };
        ListenFormat {
            rate: format.nSamplesPerSec as i32,
            channels: i32::from(format.nChannels),
            bits: i32::from(format.wBitsPerSample),
            encoding: wave_encoding(format.wFormatTag, float),
        }
    }
}

fn capture(
    queue: Arc<PacketQueue>,
    stop: Arc<AtomicBool>,
    ready: SyncSender<Option<ListenFormat>>,
) {
    // SAFETY: COM calls on this thread's objects; the mix format is freed once, after Initialize read it
    let opened = unsafe {
        (|| -> windows::core::Result<(IAudioClient, IAudioCaptureClient, ListenFormat, usize)> {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator =
                audio::enumerator().ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let mix = client.GetMixFormat()?;
            let format = format_of(mix);
            let block = usize::from((*mix).nBlockAlign);
            let initialized = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER,
                0,
                mix,
                None,
            );
            CoTaskMemFree(Some(mix as *const core::ffi::c_void));
            initialized?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok((client, capture, format, block))
        })()
    };
    let Ok((client, capture, format, block)) = opened else {
        let _ = ready.send(None);
        return;
    };
    let _ = ready.send(Some(format.clone()));
    let silent = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
        loop {
            // SAFETY: the capture client started above; each buffer is released after it is copied
            unsafe {
                if !matches!(capture.GetNextPacketSize(), Ok(n) if n > 0) {
                    break;
                }
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                if capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .is_err()
                {
                    break;
                }
                if flags & silent == 0 && frames > 0 && !data.is_null() {
                    let bytes = std::slice::from_raw_parts(data, frames as usize * block);
                    queue.push(DeckAudioPacket {
                        rate: format.rate,
                        channels: format.channels,
                        bits: format.bits,
                        encoding: format.encoding.clone(),
                        data: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
                    });
                }
                let _ = capture.ReleaseBuffer(frames);
            }
        }
    }
    // SAFETY: the client started above
    let _ = unsafe { client.Stop() };
}
