//! A minimal PNG writer for icons the OS hands over as pixels (Windows `HICON`s): 8-bit RGBA, no filtering, zlib
//! stored blocks. Icons are small, so size does not matter; correctness and no dependency do.

/// `rgba` holds `width * height` pixels, 4 bytes each, rows top to bottom.
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let row = width as usize * 4;
    assert_eq!(rgba.len(), row * height as usize, "pixel buffer size");
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for line in rgba.chunks(row.max(1)).take(height as usize) {
        raw.push(0); // filter type None
        raw.extend_from_slice(line);
    }
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8 bits, RGBA, deflate, adaptive filtering, no interlace
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// zlib framing around deflate "stored" blocks (at most 65535 bytes each).
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut blocks = data.chunks(65535).peekable();
    if blocks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none())); // BFINAL on the last block, BTYPE 00
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &d in data {
        a = (a + u32::from(d)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn a_two_by_one_image_is_a_valid_png() {
        let png = encode_rgba(2, 1, &[255, 0, 0, 255, 0, 0, 255, 128]);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR: length 13, then 2 x 1, 8-bit RGBA
        assert_eq!(&png[8..16], b"\0\0\0\x0dIHDR");
        assert_eq!(&png[16..29], &[0, 0, 0, 2, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        assert_eq!(&png[png.len() - 12..], b"\0\0\0\0IEND\xae\x42\x60\x82");
        // IDAT: zlib header, one final stored block holding the filter byte and the 8 pixel bytes, then Adler-32
        let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        assert_eq!(&png[37..41], b"IDAT");
        let z = &png[41..41 + idat_len];
        assert_eq!(&z[..2], &[0x78, 0x01]);
        assert_eq!(&z[2..7], &[1, 9, 0, 0xf6, 0xff]);
        assert_eq!(&z[7..16], &[0, 255, 0, 0, 255, 0, 0, 255, 128]);
        assert_eq!(&z[16..], &adler32(&z[7..16]).to_be_bytes());
        let crc = u32::from_be_bytes(png[41 + idat_len..45 + idat_len].try_into().unwrap());
        assert_eq!(crc, crc32(&png[37..41 + idat_len]));
    }
}
