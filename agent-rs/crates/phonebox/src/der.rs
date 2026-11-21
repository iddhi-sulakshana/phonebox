//! A small DER reader and writer: what PKCS#12 and the Wi-Fi certificate need (single-byte tags, definite and
//! indefinite lengths on read, constructed OCTET STRINGs).

pub const BOOLEAN: u8 = 0x01;
pub const INTEGER: u8 = 0x02;
pub const BIT_STRING: u8 = 0x03;
pub const OCTET_STRING: u8 = 0x04;
pub const NULL: u8 = 0x05;
pub const OID: u8 = 0x06;
pub const UTF8_STRING: u8 = 0x0C;
pub const PRINTABLE_STRING: u8 = 0x13;
pub const UTC_TIME: u8 = 0x17;
pub const GENERALIZED_TIME: u8 = 0x18;
pub const SEQUENCE: u8 = 0x30;
pub const SET: u8 = 0x31;

/// A context-specific tag `[n]`.
pub const fn context(n: u8, constructed: bool) -> u8 {
    0x80 | if constructed { 0x20 } else { 0 } | n
}

/// One element: its tag, its content and the whole encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tlv<'a> {
    pub tag: u8,
    pub value: &'a [u8],
    pub raw: &'a [u8],
}

/// Reads elements one after another from a slice.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn peek_tag(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    pub fn take(&mut self) -> Result<Tlv<'a>, String> {
        let (tlv, used) = parse(&self.data[self.pos.min(self.data.len())..], 0)?;
        self.pos += used;
        Ok(tlv)
    }

    /// The next element's content, which must have `tag`.
    pub fn expect(&mut self, tag: u8) -> Result<&'a [u8], String> {
        let t = self.take()?;
        if t.tag != tag {
            return Err(format!("expected tag {tag:#04x}, found {:#04x}", t.tag));
        }
        Ok(t.value)
    }

    /// The next element's content when it has `tag`; nothing is read otherwise.
    pub fn optional(&mut self, tag: u8) -> Result<Option<&'a [u8]>, String> {
        if self.peek_tag() == Some(tag) {
            self.expect(tag).map(Some)
        } else {
            Ok(None)
        }
    }
}

/// Nesting deeper than this is refused (a hostile file cannot exhaust the stack).
const MAX_DEPTH: usize = 32;

/// One element at the start of `data`, and how many bytes it used.
fn parse(data: &[u8], depth: usize) -> Result<(Tlv<'_>, usize), String> {
    if depth > MAX_DEPTH {
        return Err("too deeply nested".into());
    }
    let tag = *data.first().ok_or("unexpected end of data")?;
    if tag & 0x1f == 0x1f {
        return Err("multi-byte tags are not supported".into());
    }
    let first = *data.get(1).ok_or("unexpected end of data")?;
    let (len, header) = match first {
        0x80 => {
            // indefinite length (BER): constructed only, ends at the 00 00 that closes it
            if tag & 0x20 == 0 {
                return Err("indefinite length on a primitive element".into());
            }
            let mut pos = 2;
            loop {
                if data.get(pos..pos + 2) == Some(&[0, 0]) {
                    let value = &data[2..pos];
                    return Ok((
                        Tlv {
                            tag,
                            value,
                            raw: &data[..pos + 2],
                        },
                        pos + 2,
                    ));
                }
                let (_, used) = parse(&data[pos.min(data.len())..], depth + 1)?;
                pos += used;
            }
        }
        n if n < 0x80 => (n as usize, 2),
        n => {
            let count = (n & 0x7f) as usize;
            if count > 4 {
                return Err("length too large".into());
            }
            let bytes = data.get(2..2 + count).ok_or("unexpected end of data")?;
            let len = bytes.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize);
            (len, 2 + count)
        }
    };
    let end = header.checked_add(len).ok_or("length too large")?;
    let value = data.get(header..end).ok_or("unexpected end of data")?;
    Ok((
        Tlv {
            tag,
            value,
            raw: &data[..end],
        },
        end,
    ))
}

/// The bytes of an OCTET STRING, primitive or constructed (BER chunks), from its whole encoding.
pub fn octets(tlv: &Tlv) -> Result<Vec<u8>, String> {
    collect_octets(tlv, 0)
}

fn collect_octets(tlv: &Tlv, depth: usize) -> Result<Vec<u8>, String> {
    if depth > MAX_DEPTH {
        return Err("too deeply nested".into());
    }
    if tlv.tag & 0x20 == 0 {
        return Ok(tlv.value.to_vec());
    }
    let mut out = Vec::new();
    let mut r = Reader::new(tlv.value);
    while !r.is_empty() {
        out.extend(collect_octets(&r.take()?, depth + 1)?);
    }
    Ok(out)
}

/// An unsigned INTEGER's value (at most 8 bytes, leading zero allowed).
pub fn uint(value: &[u8]) -> Result<u64, String> {
    let trimmed = match value {
        [0, rest @ ..] if !rest.is_empty() => rest,
        v => v,
    };
    if trimmed.is_empty()
        || trimmed.len() > 8
        || (value.len() == trimmed.len() && trimmed[0] & 0x80 != 0)
    {
        return Err("not a small non-negative integer".into());
    }
    Ok(trimmed
        .iter()
        .fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
}

/// The encoding of a dotted OBJECT IDENTIFIER's content ("1.2.840.113549.1.7.1").
pub fn oid(dotted: &str) -> Vec<u8> {
    let arcs: Vec<u64> = dotted
        .split('.')
        .map(|a| a.parse().expect("an OID arc"))
        .collect();
    let mut out = Vec::new();
    let mut push = |mut v: u64| {
        let mut chunk = vec![(v & 0x7f) as u8];
        v >>= 7;
        while v > 0 {
            chunk.push(0x80 | (v & 0x7f) as u8);
            v >>= 7;
        }
        chunk.reverse();
        out.extend(chunk);
    };
    push(arcs[0] * 40 + arcs[1]);
    for a in &arcs[2..] {
        push(*a);
    }
    out
}

// ---------------------------------------------------------------- writing

pub fn tlv(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = value.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend(&bytes[skip..]);
    }
    out.extend(value);
    out
}

/// A SEQUENCE (or SET, or context tag) of already encoded parts.
pub fn constructed(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    tlv(tag, &parts.concat())
}

pub fn seq(parts: &[&[u8]]) -> Vec<u8> {
    constructed(SEQUENCE, parts)
}

pub fn set(parts: &[&[u8]]) -> Vec<u8> {
    constructed(SET, parts)
}

pub fn write_oid(dotted: &str) -> Vec<u8> {
    tlv(OID, &oid(dotted))
}

/// A non-negative INTEGER from big-endian bytes (leading zeros dropped, a zero added before a high bit).
pub fn write_uint_bytes(be: &[u8]) -> Vec<u8> {
    let skip = be.iter().take_while(|b| **b == 0).count();
    let mut value = be[skip..].to_vec();
    if value.is_empty() || value[0] & 0x80 != 0 {
        value.insert(0, 0);
    }
    tlv(INTEGER, &value)
}

pub fn write_uint(v: u64) -> Vec<u8> {
    write_uint_bytes(&v.to_be_bytes())
}

pub fn write_octets(bytes: &[u8]) -> Vec<u8> {
    tlv(OCTET_STRING, bytes)
}

/// UTCTime before 2050, GeneralizedTime from 2050 (RFC 5280), to the second, in UTC.
pub fn write_time(t: chrono::DateTime<chrono::Utc>) -> Vec<u8> {
    use chrono::Datelike;
    if t.year() < 2050 {
        tlv(UTC_TIME, t.format("%y%m%d%H%M%SZ").to_string().as_bytes())
    } else {
        tlv(
            GENERALIZED_TIME,
            t.format("%Y%m%d%H%M%SZ").to_string().as_bytes(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oids_encode_as_in_the_standards() {
        assert_eq!(
            oid("1.2.840.113549.1.7.1"),
            [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x01]
        );
        assert_eq!(oid("2.5.29.19"), [0x55, 0x1d, 0x13]);
        assert_eq!(
            oid("2.16.840.1.101.3.4.1.42"),
            [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x2a]
        );
    }

    #[test]
    fn lengths_round_trip_in_short_and_long_form() {
        for n in [0usize, 1, 127, 128, 255, 256, 70000] {
            let enc = tlv(OCTET_STRING, &vec![7u8; n]);
            let mut r = Reader::new(&enc);
            let t = r.take().unwrap();
            assert_eq!(
                (t.tag, t.value.len(), t.raw.len()),
                (OCTET_STRING, n, enc.len())
            );
            assert!(r.is_empty());
        }
    }

    #[test]
    fn nested_values_and_optional_elements_read_back() {
        let enc = seq(&[
            &write_uint(3),
            &write_octets(b"abc"),
            &constructed(context(0, true), &[&write_oid("1.2.3")]),
        ]);
        let mut outer = Reader::new(&enc);
        let mut r = Reader::new(outer.expect(SEQUENCE).unwrap());
        assert_eq!(uint(r.expect(INTEGER).unwrap()).unwrap(), 3);
        assert_eq!(r.optional(NULL).unwrap(), None);
        assert_eq!(r.expect(OCTET_STRING).unwrap(), b"abc");
        assert_eq!(r.peek_tag(), Some(context(0, true)));
        let mut inner = Reader::new(r.expect(context(0, true)).unwrap());
        assert_eq!(inner.expect(OID).unwrap(), oid("1.2.3"));
        assert!(r.is_empty());
        assert!(r.take().is_err());
    }

    #[test]
    fn integers_are_positive_and_minimal() {
        assert_eq!(write_uint(0), [INTEGER, 1, 0]);
        assert_eq!(write_uint(0x80), [INTEGER, 2, 0, 0x80]);
        assert_eq!(write_uint(2000), [INTEGER, 2, 0x07, 0xd0]);
        assert_eq!(write_uint_bytes(&[0, 0, 0x7f]), [INTEGER, 1, 0x7f]);
        assert_eq!(uint(&[0, 0x80]).unwrap(), 0x80);
        assert!(uint(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).is_err());
    }

    #[test]
    fn indefinite_lengths_and_constructed_octet_strings_read() {
        // BER: an indefinite-length SEQUENCE holding a constructed OCTET STRING of two chunks
        let ber = [
            0x30, 0x80, 0x24, 0x80, 0x04, 0x02, b'a', b'b', 0x04, 0x01, b'c', 0, 0, 0, 0,
        ];
        let mut r = Reader::new(&ber);
        let s = r.take().unwrap();
        assert_eq!(s.tag, SEQUENCE);
        assert!(r.is_empty());
        let mut inner = Reader::new(s.value);
        let o = inner.take().unwrap();
        assert_eq!(octets(&o).unwrap(), b"abc");
    }

    #[test]
    fn truncated_or_oversized_input_is_an_error() {
        for bad in [
            &[0x30][..],
            &[0x30, 0x05, 0x01],
            &[0x04, 0x84, 0xff, 0xff, 0xff, 0xff],
            &[0x04, 0x85, 0, 0, 0, 0, 1],
        ] {
            assert!(Reader::new(bad).take().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn times_switch_to_generalized_in_2050() {
        use chrono::TimeZone;
        let t = chrono::Utc
            .with_ymd_and_hms(2046, 11, 21, 8, 15, 8)
            .unwrap();
        assert_eq!(write_time(t), tlv(UTC_TIME, b"461121081508Z"));
        let t = chrono::Utc.with_ymd_and_hms(2050, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(write_time(t), tlv(GENERALIZED_TIME, b"20500101000000Z"));
    }
}
