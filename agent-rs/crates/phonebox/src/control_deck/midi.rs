//! `DeckMidi.Parse`: a MIDI channel message written as wire-order hex bytes, `{value}` standing for the control's value.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MidiError {
    /// The value is outside 0..127 (`ArgumentOutOfRangeException`).
    ValueOutOfRange,
    /// The message is not a channel message (`ArgumentException`, or a byte that does not parse).
    Invalid(&'static str),
}

/// The message packed as WinMM's `midiOutShortMsg` wants it: `b0 | b1 << 8 | b2 << 16`.
pub fn parse(message: &str, value: i32) -> Result<i32, MidiError> {
    if !(0..=127).contains(&value) {
        return Err(MidiError::ValueOutOfRange);
    }
    let pieces: Vec<&str> = message
        .split([' ', ',', ':'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if !(2..=3).contains(&pieces.len()) {
        return Err(MidiError::Invalid(
            "Use MIDI wire-order bytes, e.g. B0 07 {value}",
        ));
    }
    let mut bytes = Vec::with_capacity(3);
    for piece in pieces {
        let b = if piece == "{value}" {
            value as u8
        } else {
            // byte.Parse(hex): hex digits only (no 0x), at most 0xFF
            u8::from_str_radix(piece, 16)
                .ok()
                .filter(|_| piece.bytes().all(|c| c.is_ascii_hexdigit()))
                .ok_or(MidiError::Invalid("Invalid MIDI byte"))?
        };
        bytes.push(b);
    }
    if !(0x80..=0xEF).contains(&bytes[0]) || bytes[1..].iter().any(|b| *b > 127) {
        return Err(MidiError::Invalid("Invalid MIDI channel message"));
    }
    let want = if matches!(bytes[0] & 0xF0, 0xC0 | 0xD0) {
        2
    } else {
        3
    };
    if bytes.len() != want {
        return Err(MidiError::Invalid("Wrong MIDI message length"));
    }
    Ok(bytes
        .iter()
        .enumerate()
        .fold(0, |acc, (i, b)| acc | (i32::from(*b) << (8 * i))))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ControlDeckTests
    #[test]
    fn midi_bytes_use_wire_order_and_reject_system_or_out_of_range_messages() {
        assert_eq!(parse("B0 07 {value}", 100), Ok(0x6407B0));
        assert_eq!(parse("C0 3C", 0), Ok(0x003CC0));
        assert!(matches!(parse("F0 00 00", 0), Err(MidiError::Invalid(_))));
        assert!(matches!(parse("90 FF 10", 0), Err(MidiError::Invalid(_))));
        assert!(matches!(parse("90 3C", 0), Err(MidiError::Invalid(_))));
        assert_eq!(parse("B0 07 {value}", 128), Err(MidiError::ValueOutOfRange));
        assert_eq!(parse("B0,07:{value}", 1), Ok(0x0107B0));
        assert!(matches!(parse("B0 0x07 1", 0), Err(MidiError::Invalid(_))));
        assert!(matches!(parse("B0", 0), Err(MidiError::Invalid(_))));
    }
}
