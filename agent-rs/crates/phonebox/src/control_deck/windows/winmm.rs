//! MIDI out through WinMM (`ControlDeck.Native.cs:378-392`): the ports by number and name, and one message per action
//! on a port opened for it. A note-on is followed by its note-off 120 ms later, so a pad never leaves a note sounding
//! after the phone's connection drops.

use std::time::Duration;

use windows::Win32::Media::Audio::{
    CALLBACK_NULL, HMIDIOUT, MIDIOUTCAPSW, midiOutClose, midiOutGetDevCapsW, midiOutGetNumDevs,
    midiOutOpen, midiOutShortMsg,
};

use super::super::midi;
use super::super::models::DeckItem;
use super::{mm_result_name, note_off};

const PORT_OUT_OF_RANGE: &str =
    "Specified argument was out of the range of valid values. (Parameter 'port')";

fn wide(text: &[u16]) -> String {
    let end = text.iter().position(|c| *c == 0).unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

/// NAudio's `MmException` text.
fn failed(code: u32, function: &str) -> String {
    format!("{} calling {function}", mm_result_name(code))
}

/// Every MIDI output by its number (none when one cannot be read, as C#'s enumeration threw).
pub(super) fn ports() -> Vec<DeckItem> {
    // SAFETY: no arguments
    let count = unsafe { midiOutGetNumDevs() };
    let mut ports = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut caps = MIDIOUTCAPSW::default();
        // SAFETY: a caps struct of the size given
        let read = unsafe {
            midiOutGetDevCapsW(
                i as usize,
                &mut caps,
                std::mem::size_of::<MIDIOUTCAPSW>() as u32,
            )
        };
        if read != 0 {
            return vec![];
        }
        ports.push(DeckItem {
            id: i.to_string(),
            name: {
                // a packed struct: the name is copied out first
                let name = caps.szPname;
                wide(&name)
            },
            kind: "midi".into(),
            state: None,
            available: true,
            rev: None,
        });
    }
    ports
}

fn short(handle: HMIDIOUT, packed: i32) -> Result<(), String> {
    // SAFETY: an open MIDI out handle
    match unsafe { midiOutShortMsg(handle, packed as u32) } {
        0 => Ok(()),
        code => Err(failed(code, "midiOutShortMsg")),
    }
}

/// Sends `message` (wire-order hex, `{value}` for the value) to output `port`. Blocks 120 ms after a note-on.
pub(super) fn send(port: i32, message: &str, value: i32) -> Result<(), String> {
    // SAFETY: no arguments
    let count = unsafe { midiOutGetNumDevs() };
    if port < 0 || port as u32 >= count {
        return Err(PORT_OUT_OF_RANGE.into());
    }
    let mut handle = HMIDIOUT::default();
    // SAFETY: the handle is written by the call; no callback
    let opened = unsafe { midiOutOpen(&mut handle, port as u32, None, None, CALLBACK_NULL) };
    if opened != 0 {
        return Err(failed(opened, "midiOutOpen"));
    }
    let sent = midi::parse(message, value)
        .map_err(|e| e.to_string())
        .and_then(|packed| {
            short(handle, packed)?;
            if let Some(off) = note_off(packed) {
                std::thread::sleep(Duration::from_millis(120));
                short(handle, off)?;
            }
            Ok(())
        });
    // SAFETY: the handle opened above, closed once
    let _ = unsafe { midiOutClose(handle) };
    sent
}
